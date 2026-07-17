//! Bordnet (BNT-XML) → the ISTA ECU-tree topology tables in the semantic DB.
//!
//! ISTA's per-platform ECU tree (the graph view: short display names, bus
//! membership, grid layout, the always-present "minimal configuration", and
//! combined housings) ships as `BNT-XML-<series>` info objects whose XML bodies
//! live in the plaintext `xmlvalueprimitive_OTHER.sqlite`. The semantic extract
//! records each series' body pointer in `bordnet_doc` (see
//! `scripts/build-semantic-db.sh`); this pass fetches and parses each body and
//! writes the `ecu_tree` / `ecu_housing` tables back into the semantic DB.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::Reader;
use quick_xml::events::Event;
use rusqlite::{Connection, OpenFlags};

/// One ECU node of a platform's tree.
#[derive(Debug, Default)]
struct EcuNode {
    address: Option<i64>,
    name: Option<String>,
    group_sgbd: Option<String>,
    bus: Option<String>,
    col: Option<i64>,
    row: Option<i64>,
}

/// One parsed platform bordnet.
#[derive(Debug, Default)]
struct Bordnet {
    ecus: Vec<EcuNode>,
    /// Bus enum name (e.g. `FACAN`) → display label (e.g. `PT-CAN`).
    bus_labels: HashMap<String, String>,
    /// Diagnostic addresses of the always-present minimal configuration.
    minimal: Vec<i64>,
    /// Combined housings: (grid column, grid row, member addresses).
    housings: Vec<(Option<i64>, Option<i64>, Vec<i64>)>,
}

/// Parse one BNT-XML body into a [`Bordnet`].
///
/// Recognizes `EcuLogisticsEntry` (address/name attributes + GroupSgbd, Bus,
/// Column, Row children), `BusNameEntry`, `MinimalConfigurationEntry`, and
/// `CombinedEcuHousingEntry` (Column/Row + repeated RequiredEcuAddresses).
/// Unknown elements are skipped, so schema growth does not break the build.
fn parse_bordnet(xml: &str) -> Result<Bordnet> {
    /// Which container element the cursor is inside (they share child names:
    /// `Column`/`Row` occur in both ECU and housing entries).
    #[derive(PartialEq)]
    enum Ctx {
        None,
        Ecu,
        BusName,
        Minimal,
        Housing,
    }
    let mut reader = Reader::from_str(xml);
    let mut out = Bordnet::default();
    let mut ctx = Ctx::None;
    // The current leaf element's tag, so the next text event knows its field.
    let mut field = String::new();
    let mut bus_enum = String::new();
    let mut housing: (Option<i64>, Option<i64>, Vec<i64>) = (None, None, Vec::new());
    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                match tag.as_str() {
                    "EcuLogisticsEntry" => {
                        ctx = Ctx::Ecu;
                        let mut node = EcuNode::default();
                        for attr in e.attributes() {
                            let attr = attr.map_err(quick_xml::Error::from)?;
                            let value = String::from_utf8_lossy(&attr.value).to_string();
                            match attr.key.as_ref() {
                                b"DiagAddress" => node.address = value.trim().parse().ok(),
                                b"Name" => node.name = Some(value),
                                _ => {}
                            }
                        }
                        out.ecus.push(node);
                    }
                    "BusNameEntry" => {
                        ctx = Ctx::BusName;
                        bus_enum.clear();
                        for attr in e.attributes() {
                            let attr = attr.map_err(quick_xml::Error::from)?;
                            if attr.key.as_ref() == b"Bus" {
                                bus_enum = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                    }
                    "MinimalConfigurationEntry" => ctx = Ctx::Minimal,
                    "CombinedEcuHousingEntry" => {
                        ctx = Ctx::Housing;
                        housing = (None, None, Vec::new());
                    }
                    _ => field = tag,
                }
            }
            Event::Text(t) => {
                let text = t
                    .decode()
                    .map_err(quick_xml::Error::from)?
                    .trim()
                    .to_string();
                if text.is_empty() {
                    continue;
                }
                match ctx {
                    Ctx::Ecu => {
                        if let Some(node) = out.ecus.last_mut() {
                            match field.as_str() {
                                "GroupSgbd" => node.group_sgbd = Some(text),
                                "Bus" => node.bus = Some(text),
                                "Column" => node.col = text.parse().ok(),
                                "Row" => node.row = text.parse().ok(),
                                _ => {}
                            }
                        }
                    }
                    Ctx::BusName => {
                        if !bus_enum.is_empty() {
                            out.bus_labels.insert(bus_enum.clone(), text);
                        }
                    }
                    Ctx::Minimal => {
                        if let Ok(addr) = text.parse() {
                            out.minimal.push(addr);
                        }
                    }
                    Ctx::Housing => match field.as_str() {
                        "Column" => housing.0 = text.parse().ok(),
                        "Row" => housing.1 = text.parse().ok(),
                        "RequiredEcuAddresses" => {
                            if let Ok(addr) = text.parse() {
                                housing.2.push(addr);
                            }
                        }
                        _ => {}
                    },
                    Ctx::None => {}
                }
            }
            Event::End(e) => {
                let tag = e.name();
                let tag = String::from_utf8_lossy(tag.as_ref());
                match tag.as_ref() {
                    "EcuLogisticsEntry" | "BusNameEntry" | "MinimalConfigurationEntry" => {
                        ctx = Ctx::None;
                    }
                    "CombinedEcuHousingEntry" => {
                        out.housings.push(std::mem::take(&mut housing));
                        ctx = Ctx::None;
                    }
                    _ => field.clear(),
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Build the `ecu_tree` + `ecu_housing` tables in the semantic DB, returning
/// `(platforms, ecu rows)` written.
///
/// Reads the `bordnet_doc` pointer table (series → xmlvalue doc id) from
/// `semantic_db`, fetches each XML body from the plaintext
/// `xmlvalueprimitive_OTHER.sqlite`, parses it, and writes one `ecu_tree` row
/// per catalogued ECU (with its bus label and minimal-configuration flag) plus
/// one `ecu_housing` row per combined-housing member. A semantic DB predating
/// `bordnet_doc`, or a pointer with no body in this install, degrades to
/// skipped work, not an error.
///
/// # Errors
///
/// Returns an error if a DB cannot be opened or written, or a body fails to
/// parse.
pub fn build_ecu_tree(semantic_db: &Path, xmlvalue_other_db: &Path) -> Result<(usize, usize)> {
    let sem = Connection::open(semantic_db)
        .with_context(|| format!("opening semantic DB {}", semantic_db.display()))?;
    let has_pointers: bool = sem
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='bordnet_doc'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)?;
    if !has_pointers {
        return Ok((0, 0)); // pre-v5 semantic extract — nothing to do
    }
    let xmlv = Connection::open_with_flags(xmlvalue_other_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening xmlvalue DB {}", xmlvalue_other_db.display()))?;

    let mut stmt = sem.prepare("SELECT series, doc_id FROM bordnet_doc ORDER BY series")?;
    let wanted: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    // Global-id (c0, TEXT) → rowid PK, built once (c0 is unindexed).
    let mut map_stmt = xmlv.prepare("SELECT id, c0 FROM xmlvalueprimitive_content")?;
    let mut id_of: HashMap<String, i64> = HashMap::new();
    for row in map_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
        let (rowid, c0) = row?;
        id_of.insert(c0, rowid);
    }

    sem.execute_batch(
        "DROP TABLE IF EXISTS ecu_tree;
         DROP TABLE IF EXISTS ecu_housing;
         CREATE TABLE ecu_tree (series TEXT, address INTEGER, name TEXT, group_sgbd TEXT,
                                bus TEXT, bus_label TEXT, col INTEGER, row INTEGER,
                                minimal INTEGER);
         CREATE TABLE ecu_housing (series TEXT, col INTEGER, row INTEGER, address INTEGER);
         CREATE INDEX idx_ecu_tree ON ecu_tree(series, address);",
    )?;
    let tx = sem.unchecked_transaction()?;
    let mut body_stmt = xmlv.prepare("SELECT c3 FROM xmlvalueprimitive_content WHERE id = ?1")?;
    let (mut platforms, mut rows) = (0usize, 0usize);
    for (series, doc_id) in wanted {
        let Some(&rowid) = id_of.get(&doc_id.to_string()) else {
            continue; // pointer with no body in this install — skip, not an error
        };
        let xml: String = body_stmt.query_row([rowid], |r| r.get(0))?;
        let bordnet = parse_bordnet(&xml).with_context(|| format!("parsing bordnet {series}"))?;
        for ecu in &bordnet.ecus {
            let Some(address) = ecu.address else { continue };
            let bus_label = ecu
                .bus
                .as_ref()
                .and_then(|b| bordnet.bus_labels.get(b))
                .cloned();
            tx.execute(
                "INSERT INTO ecu_tree VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                rusqlite::params![
                    series,
                    address,
                    ecu.name,
                    ecu.group_sgbd,
                    ecu.bus,
                    bus_label,
                    ecu.col,
                    ecu.row,
                    i64::from(bordnet.minimal.contains(&address)),
                ],
            )?;
            rows += 1;
        }
        for (col, row, members) in &bordnet.housings {
            for address in members {
                tx.execute(
                    "INSERT INTO ecu_housing VALUES (?1,?2,?3,?4)",
                    rusqlite::params![series, col, row, address],
                )?;
            }
        }
        platforms += 1;
    }
    tx.commit()?;
    Ok((platforms, rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// A synthetic two-ECU bordnet in the real BNT-XML shape (no BMW data).
    const XML: &str = r#"<?xml version="1.0"?>
<EcuTreeConfiguration xmlns="http://bmw.com/Rheingold/EcuTreeConfiguration" MainSeriesSgbd="X01">
  <EcuLogisticsList>
    <EcuLogisticsEntry DiagAddress="18" Name="DME">
      <GroupSgbd>G_MOTOR</GroupSgbd>
      <Bus>FACAN</Bus>
      <Column>1</Column>
      <Row>2</Row>
      <SubDiagAddress xsi:nil="true" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"/>
    </EcuLogisticsEntry>
    <EcuLogisticsEntry DiagAddress="64" Name="FEM">
      <GroupSgbd>G_FEM</GroupSgbd>
      <Bus>KCAN</Bus>
      <Column>0</Column>
      <Row>5</Row>
    </EcuLogisticsEntry>
  </EcuLogisticsList>
  <CombinedEcuHousingList>
    <CombinedEcuHousingEntry>
      <Column>0</Column>
      <Row>5</Row>
      <EcuCount>2</EcuCount>
      <RequiredEcuAddresses>64</RequiredEcuAddresses>
      <RequiredEcuAddresses>18</RequiredEcuAddresses>
    </CombinedEcuHousingEntry>
  </CombinedEcuHousingList>
  <BusNameList>
    <BusNameEntry Bus="FACAN">PT-CAN</BusNameEntry>
    <BusNameEntry Bus="KCAN">K-CAN</BusNameEntry>
  </BusNameList>
  <MinimalConfigurationList>
    <MinimalConfigurationEntry>18</MinimalConfigurationEntry>
  </MinimalConfigurationList>
</EcuTreeConfiguration>"#;

    #[test]
    fn parses_ecus_buses_minimal_and_housings() {
        let b = parse_bordnet(XML).unwrap();
        assert_eq!(b.ecus.len(), 2);
        let dme = &b.ecus[0];
        assert_eq!(dme.address, Some(18));
        assert_eq!(dme.name.as_deref(), Some("DME"));
        assert_eq!(dme.group_sgbd.as_deref(), Some("G_MOTOR"));
        assert_eq!(dme.bus.as_deref(), Some("FACAN"));
        assert_eq!((dme.col, dme.row), (Some(1), Some(2)));
        assert_eq!(b.bus_labels["FACAN"], "PT-CAN");
        assert_eq!(b.minimal, [18]);
        assert_eq!(b.housings, [(Some(0), Some(5), vec![64, 18])]);
    }

    #[test]
    fn writes_tree_and_housing_tables_into_the_semantic_db() {
        let dir = tempfile::tempdir().unwrap();
        let sem_path = dir.path().join("semantic.db");
        let xml_path = dir.path().join("xmlvalue.db");
        {
            let sem = Connection::open(&sem_path).unwrap();
            sem.execute_batch(
                "CREATE TABLE bordnet_doc (series TEXT, doc_id INTEGER);
                 INSERT INTO bordnet_doc VALUES ('X01', 4242);
                 INSERT INTO bordnet_doc VALUES ('MISSING', 9999);",
            )
            .unwrap();
            let xmlv = Connection::open(&xml_path).unwrap();
            xmlv.execute_batch(
                "CREATE TABLE xmlvalueprimitive_content (id INTEGER PRIMARY KEY, c0 TEXT, c3 TEXT);",
            )
            .unwrap();
            xmlv.execute(
                "INSERT INTO xmlvalueprimitive_content VALUES (1, '4242', ?1)",
                [XML],
            )
            .unwrap();
        }
        let (platforms, rows) = build_ecu_tree(&sem_path, &xml_path).unwrap();
        // The missing-body series is skipped; the real one lands fully.
        assert_eq!((platforms, rows), (1, 2));
        let sem = Connection::open(&sem_path).unwrap();
        let (bus_label, minimal): (String, i64) = sem
            .query_row(
                "SELECT bus_label, minimal FROM ecu_tree WHERE series='X01' AND address=18",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(bus_label, "PT-CAN");
        assert_eq!(minimal, 1);
        let housing_members: i64 = sem
            .query_row(
                "SELECT COUNT(*) FROM ecu_housing WHERE series='X01' AND col=0 AND row=5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(housing_members, 2);
    }

    #[test]
    fn degrades_to_noop_without_the_pointer_table() {
        let dir = tempfile::tempdir().unwrap();
        let sem_path = dir.path().join("semantic.db");
        let xml_path = dir.path().join("xmlvalue.db");
        Connection::open(&sem_path).unwrap();
        Connection::open(&xml_path).unwrap();
        assert_eq!(build_ecu_tree(&sem_path, &xml_path).unwrap(), (0, 0));
    }
}
