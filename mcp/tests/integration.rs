//! In-process integration tests driving the MCP tools directly (no real car).

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use klartext_mcp::dto::{
    ClearAllFaultsRequest, ClearFaultsRequest, ConnectRequest, FaultHelpRequest,
    ListMeasurementsRequest, ListServiceFunctionsRequest, ReadAllFaultsRequest, ReadDataRequest,
    ReadFaultDetailRequest, ReadFaultsRequest, RunJobRequest, ScanEcusRequest,
};
use rmcp::handler::server::wrapper::Parameters;
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::net::TcpListener;

/// Build a server config with defaults, overriding nothing (no car needed).
fn test_config() -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp"])
}

#[tokio::test]
async fn disconnect_without_connection_reports_not_connected() {
    let server = KlartextServer::new(test_config());
    let result = server.disconnect().await.unwrap();
    assert!(!result.0.was_connected);
}

#[test]
fn advertises_disconnect_tool() {
    let server = KlartextServer::new(test_config());
    let tools = server.advertised_tools();
    assert!(
        tools.contains(&"disconnect".to_string()),
        "tools: {tools:?}"
    );
}

/// Build a synthetic semantic DB (no BMW data) matching the extract schema.
fn fixture_db() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
         CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
         INSERT INTO dtc VALUES (64,'fem_20',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: bus, no communication');
         INSERT INTO dtc VALUES (64,'fem_21',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
         INSERT INTO ecu VALUES (16,'zgw_x','d_0010','Gateway',NULL);
         INSERT INTO ecu VALUES (18,'dde_x','d_0012','Digital Diesel Electronics',NULL);
         INSERT INTO ecu VALUES (64,'fem_20','d_0040','Front Electronic Module',NULL);
         -- 0x18 is in the model map but absent on the mock car (not in the gateway SVT).
         INSERT INTO ecu VALUES (24,'egs_x','d_0018','Transmission',NULL);
         -- v4 extract: ISTA's measurement catalog (the \"index\"). Synthetic rows only.
         CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT);
         INSERT INTO measurement VALUES ('dde_x','STAT_EXAMPLE_TEMP_WERT','°C',1.0,0.0,0,NULL,'STATUS_LESEN');
         INSERT INTO measurement VALUES ('dde_x','STAT_EXAMPLE_RPM_WERT','1/min',1.0,0.0,0,NULL,'STATUS_MOTORDREHZAHL');",
    )
    .unwrap();
    (dir, path)
}

/// Build a synthetic semantic DB that also carries the M11 Item 4 repair-doc tables.
///
/// Extends the base fixture shape with a `fault_doc ⋈ infoobject` pair so `fault_help`
/// resolves offline. The DTC `0x4B1234` bridges to 4919860 (big-endian 24-bit concat)
/// and address 18 is `0x12`; two ISTA documents link to that fault.
fn fixture_db_with_docs() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("semantic.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
         INSERT INTO ecu VALUES (18,'dde_x','d_0012','Digital Diesel Electronics',NULL);
         CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
         INSERT INTO dtc VALUES (18,'dde_x',4919860,'P123400',NULL,'Glow plug circuit');
         CREATE TABLE fault_doc (address INT, code INT, infoobject_id INT, content_engb INT, content_dede INT);
         INSERT INTO fault_doc VALUES (18,4919860,1001,55501,55502);
         INSERT INTO fault_doc VALUES (18,4919860,1002,55601,55602);
         CREATE TABLE infoobject (id INT, infotype TEXT, docnumber TEXT, safety_relevant INT, title_en TEXT, title_de TEXT);
         INSERT INTO infoobject VALUES (1001,'FKB','DOC-1',0,'Glow plug fault','Gluehkerzenfehler');
         INSERT INTO infoobject VALUES (1002,'ABL','DOC-2',1,NULL,'Gluehkerze pruefen');",
    )
    .unwrap();
    (dir, path)
}

/// A server config pointed at a fixture DB (no gateway set).
fn config_with_db(path: &Path) -> ServerConfig {
    ServerConfig::parse_from(["klartext-mcp", "--semantic-db", path.to_str().unwrap()])
}

#[tokio::test]
async fn list_ecus_names_ecus_from_the_db() {
    let (_dir, path) = fixture_db();
    let server = KlartextServer::new(config_with_db(&path));
    let result = server.list_ecus().await.unwrap();
    assert!(result.0.db_available);
    assert!(result.0.db_error.is_none());
    // 0x40 is the FEM group d_0040 with its DB title and variant candidates.
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.group_name, "d_0040");
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));
    assert!(fem.variants.iter().any(|v| v == "fem_20"));
    // 0x12 is the engine group d_0012. No hardcoded "DME"/"CAS" aliases exist.
    let dde = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x12")
        .unwrap();
    assert_eq!(dde.group_name, "d_0012");
}

#[tokio::test]
async fn list_ecus_without_db_is_empty() {
    let server = KlartextServer::new(test_config());
    let result = server.list_ecus().await.unwrap();
    assert!(!result.0.db_available);
    assert!(result.0.ecus.is_empty());
}

// M11 Item 4: fault_help is a PURE semantic-DB read — no connect, no mock gateway.
// It resolves the ECU + DTC and returns the fault's linked ISTA documents (the
// title/pointer layer), degrading to empty docs (never erroring) without the extract.
#[tokio::test]
async fn fault_help_returns_linked_docs_offline() {
    // A server with a fixture semantic DB and NO car connection.
    let (_dir, db) = fixture_db_with_docs();
    let server = KlartextServer::new(config_with_db(&db));

    let out = server
        .fault_help(Parameters(FaultHelpRequest {
            ecu: "0x12".to_string(),
            code: "4B1234".to_string(),
        }))
        .await
        .unwrap();
    let r = out.0;
    assert_eq!(r.code_hex, "4B1234");
    // Two ISTA documents link to the fault; one is the English fault description.
    assert_eq!(r.docs.len(), 2);
    assert!(
        r.docs
            .iter()
            .any(|d| d.title.as_deref() == Some("Glow plug fault"))
    );
    // The German fallback title and the safety flag survive the DTO mapping.
    let procedure = r.docs.iter().find(|d| d.infoobject_id == 1002).unwrap();
    assert_eq!(procedure.title.as_deref(), Some("Gluehkerze pruefen"));
    assert!(procedure.safety_relevant);
    assert_eq!(procedure.docnumber.as_deref(), Some("DOC-2"));
    // DB-only: the fault text also resolved from the dtc table.
    assert!(
        r.descriptions
            .iter()
            .any(|d| d.text.as_deref() == Some("Glow plug circuit"))
    );
    // Non-empty-docs note is the title-layer caveat, not a "build the DB" message.
    assert!(r.note.contains("ISTA document"), "{}", r.note);
    // This fixture has no sibling klartext-docs.db, so the rendered FKB body degrades
    // to empty (the `docs` pointers still apply). The positive render path is unit-tested
    // in klartext-semantic's fault_body_reads_rendered_markdown_from_sibling_docs_db.
    assert!(r.body.is_empty());
}

/// The recorded, ordered UDS payloads a mock gateway has received (keepalives excluded).
type FrameLog = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// The ECUs the mock car answers for; 0x18 (in the DB map) is deliberately absent.
const MOCK_PRESENT: &[u8] = &[0x10, 0x12, 0x40];

/// A loopback mock gateway with several ECUs demultiplexed over one connection.
///
/// Answers the SVT installed-ECU list (`22 3F 07`, used by scan discovery),
/// VIN/DTC/PID reads, the dynamic-measurement sequence, and the extended-session +
/// standard-clear handshakes; ignores keepalives. Only [`MOCK_PRESENT`] addresses
/// answer, and the SVT it returns lists exactly that set. Every reply swaps SRC/TGT
/// (as the real gateway does), so the client's demux routes it by the answering
/// ECU's address. Each ECU tracks a "cleared" flag so a post-clear re-read comes
/// back clean. Every non-keepalive UDS payload is recorded in the returned log for
/// exact-frame assertions.
async fn spawn_mock_gateway() -> (std::net::SocketAddr, FrameLog) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log: FrameLog = std::sync::Arc::default();
    let shared = std::sync::Arc::clone(&log);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let log = std::sync::Arc::clone(&shared);
            tokio::spawn(async move {
                let mut cleared: std::collections::HashSet<u8> = Default::default();
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    if frame.payload.as_slice() == [0x3E, 0x80] {
                        continue; // keepalive — unlogged (timing-dependent), no reply
                    }
                    if !MOCK_PRESENT.contains(&ecu) {
                        continue; // absent ECU — silence (a read there times out)
                    }
                    log.lock().unwrap().push(frame.payload.clone());
                    let reply = match frame.payload.as_slice() {
                        [0x3E, 0x00] => vec![0x7E, 0x00], // presence probe
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            uds
                        }
                        // SVT installed-ECU list (22 3F 07) from the ZGW: a u16-BE count
                        // then one address byte per fitted ECU. Returns MOCK_PRESENT so
                        // SVT discovery yields exactly the ECUs that answer reads (0x18,
                        // in the DB map, is not listed). DERIVED framing
                        // (STATUS_VCM_GET_ECU_LIST_ALL), [verify against capture].
                        [0x22, 0x3F, 0x07] => {
                            let mut uds = vec![0x62, 0x3F, 0x07, 0x00, MOCK_PRESENT.len() as u8];
                            uds.extend_from_slice(MOCK_PRESENT);
                            uds
                        }
                        // The gateway identity reads (M11 Item 2): integration level
                        // (I-Stufe, 22 100B) and the vehicle order (FA, 22 3F06, raw
                        // region). I-Stufe is a binary-packed 8-byte record ("F020" +
                        // year 21 (0x15) + month 11 (0x0B) + patch 500 (0x01F4)) →
                        // "F020-21-11-500"; the FA framing stays [verify against capture].
                        [0x22, 0x10, 0x0B] => {
                            let mut uds = vec![0x62, 0x10, 0x0B];
                            uds.extend_from_slice(&[
                                0x46, 0x30, 0x32, 0x30, 0x15, 0x0B, 0x01, 0xF4,
                            ]);
                            uds
                        }
                        [0x22, 0x3F, 0x06] => vec![0x62, 0x3F, 0x06, 0xAA, 0xBB],
                        // Any other identification DID (22 F1xx besides the VIN F190
                        // handled above) is answered negatively so identify_vehicle's
                        // per-ECU identification skips it fast — exactly as a real ECU
                        // that does not serve the DID would. requestOutOfRange (0x31).
                        [0x22, 0xF1, _] => vec![0x7F, 0x22, 0x31],
                        // OBDDataIdentifier for PID 0x0C (engine RPM): 0D 48 -> 850 rpm.
                        [0x22, 0xF4, 0x0C] => vec![0x62, 0xF4, 0x0C, 0x0D, 0x48],
                        // The DDE's static `0x22` read of DID 0x4517 (SG_FUNKTIONEN row
                        // ITOEL) — the frame the real STATUS_LESEN(ARG;ITOEL) bytecode
                        // emits (frozen in crates/best/tests/differential.rs). Answers a
                        // raw word the job scales; drives run_job's live read path.
                        [0x22, 0x45, 0x17] => vec![0x62, 0x45, 0x17, 0x0A, 0xBC],
                        // M6 Part B: the DDE "selektiv lesen" sequence for engine
                        // temp (id 0x4BC3, u16), DERIVED from the d72n47a0
                        // disassembly (docs/sgbd-findings.md §7a): clear, define
                        // F303 from source DID 4BC3, then read F303 -> raw 0E 2F
                        // (u16 3631 * 0.1 - 273.14 = 89.96 degC).
                        [0x2C, 0x03, 0xF3, 0x03] => vec![0x6C, 0x03, 0xF3, 0x03],
                        [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02] => {
                            vec![0x6C, 0x01, 0xF3, 0x03]
                        }
                        [0x22, 0xF3, 0x03] => vec![0x62, 0xF3, 0x03, 0x0E, 0x2F],
                        // Freeze-frame detail (M11) for DTC 24 00 00. Snapshot: record
                        // 1, 2 identifiers — coolant 0x5205 = 0x7B, RPM 0x5955 = 0x1068.
                        // Extended: HFK (0x02) = 0x1F. Severity: 0x20 / 0x10. DERIVED
                        // ISO 14229 framing, [verify against capture].
                        [0x19, 0x04, 0x24, 0x00, 0x00, 0xFF] => vec![
                            0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x02, 0x52, 0x05, 0x7B, 0x59,
                            0x55, 0x10, 0x68,
                        ],
                        [0x19, 0x06, 0x24, 0x00, 0x00, 0xFF] => {
                            vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F]
                        }
                        [0x19, 0x09, 0x24, 0x00, 0x00] => {
                            vec![0x59, 0x09, 0xFF, 0x20, 0x10, 0x24, 0x00, 0x00, 0x08]
                        }
                        // After a clear, this ECU reads clean.
                        [0x19, 0x02, _mask] if cleared.contains(&ecu) => vec![0x59, 0x02, 0xFF],
                        // One relevant DTC (D9 04 0A, status 0x08) + one "not tested
                        // this cycle" catalog entry (AA BB CC, status 0x40) to
                        // exercise the relevance partition.
                        [0x19, 0x02, _mask] => vec![
                            0x59, 0x02, 0xFF, 0xD9, 0x04, 0x0A, 0x08, 0xAA, 0xBB, 0xCC, 0x40,
                        ],
                        // Extended session + the standard clear-all (M9 Part B).
                        [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                        [0x14, 0xFF, 0xFF, 0xFF] => {
                            cleared.insert(ecu);
                            vec![0x54]
                        }
                        _ => continue,
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, reply); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    (addr, log)
}

/// The BYO SGBD directory (never committed): `data/Testmodule(1)/Ecu` in the workspace.
fn sgbd_test_dir() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../data/Testmodule(1)/Ecu")
        .to_str()
        .unwrap()
        .to_string()
}

/// A server config pointed at the mock gateway + a fixture DB.
fn config_for_mock(addr: std::net::SocketAddr, db: &Path) -> ServerConfig {
    ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
    ])
}

#[tokio::test]
async fn connect_returns_vin_from_the_gateway() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));

    let result = server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    assert!(result.0.connected);
    assert_eq!(result.0.vin.as_deref(), Some("WBA3B5C50EK123456"));
    assert_eq!(result.0.vin_source, "did_f190");
}

#[tokio::test]
async fn read_faults_decodes_flags_and_descriptions() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0x40 is in the fixture's dtc + ecu tables (group d_0040).
    let result = server
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
            include_not_tested: false,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    // Only the relevant fault is shown; the "not tested this cycle" entry is counted.
    assert_eq!(result.0.count, 1);
    assert_eq!(result.0.not_tested_count, 1);
    let fault = &result.0.faults[0];
    assert_eq!(fault.code_hex, "D9040A");
    assert_eq!(fault.status_hex, "08");
    assert_eq!(fault.status_flags, vec!["confirmedDTC".to_string()]);
    assert!(result.0.db_available);
    // Two variants share the code at address 0x40.
    assert_eq!(fault.descriptions.len(), 2);
    assert!(fault.descriptions.iter().any(|d| {
        d.text
            .as_deref()
            .is_some_and(|t| t.contains("EXAMPLE fault A"))
    }));
}

#[tokio::test]
async fn read_faults_without_connect_errors_clearly() {
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_with_db(&db));
    let result = server
        .read_faults(Parameters(ReadFaultsRequest {
            ecu: "0x40".to_string(),
            include_not_tested: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a not-connected error, got Ok");
    };
    assert!(err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn read_fault_detail_reads_all_three_services_and_degrades_without_sgbd() {
    // Without --sgbd-dir the fields cannot be decoded, but the plumbing still runs:
    // all three reads succeed, severity is parsed, and the notes explain the raw
    // state and the capture caveat. Runs in CI (no BYO data needed).
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "240000".to_string(),
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.code_hex, "240000");
    // Severity (19 09) is parsed even without the SGBD.
    assert_eq!(result.0.severity_hex.as_deref(), Some("20"));
    assert_eq!(result.0.functional_unit_hex.as_deref(), Some("10"));
    // No SGBD → fields stay raw, flagged, with the derived-framing caveat.
    assert!(!result.0.sgbd_available);
    assert!(result.0.snapshot.is_empty());
    assert!(
        result.0.notes.iter().any(|n| n.contains("no SGBD variant")),
        "expected a no-SGBD note, got {:?}",
        result.0.notes
    );
    assert!(
        result.0.notes.iter().any(|n| n.contains("provisional")),
        "expected the capture caveat note"
    );
}

#[tokio::test]
async fn read_fault_detail_rejects_a_malformed_code() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    let result = server
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "24ZZ".to_string(),
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an invalid-code error, got Ok");
    };
    assert!(err.message.contains("invalid DTC code"), "{}", err.message);
}

// The full decode path with the real DDE SGBD: the snapshot's coolant (0x5205, u8
// − 40) and RPM (0x5955, u16 × 0.5) fields resolve to values. Ignored by default
// (needs the BYO `.prg`). The wire bytes are DERIVED, not captured.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_fault_detail_decodes_snapshot_with_real_sgbd() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_fault_detail(Parameters(ReadFaultDetailRequest {
            ecu: "0x12".to_string(),
            code: "240000".to_string(),
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert!(result.0.sgbd_available);
    // Coolant 0x5205 = 0x7B (123 − 40 = 83 °C) and RPM 0x5955 = 0x1068 (4200 × 0.5).
    let coolant = result
        .0
        .snapshot
        .iter()
        .find(|f| f.id_hex == "5205")
        .expect("coolant field decoded");
    assert!((coolant.value.unwrap() - 83.0).abs() < 0.01);
    let rpm = result
        .0
        .snapshot
        .iter()
        .find(|f| f.id_hex == "5955")
        .expect("rpm field decoded");
    assert!((rpm.value.unwrap() - 2100.0).abs() < 0.01);
    // Extended data: the HFK occurrence counter.
    assert!(result.0.extended.iter().any(|r| r.label == "HFK"));
}

#[tokio::test]
async fn read_data_decodes_vin() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("F190".to_string()),
            name: None,
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "F190");
    assert_eq!(result.0.name.as_deref(), Some("VIN"));
    assert_eq!(result.0.value_text.as_deref(), Some("WBA3B5C50EK123456"));
    // A non-PID identification DID carries no engineering value.
    assert_eq!(result.0.scaled_value, None);
    assert_eq!(result.0.unit, None);
}

#[tokio::test]
async fn read_data_scales_a_standard_pid() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // 0xF40C = OBDDataIdentifier for engine RPM; the mock returns 0D 48 -> 850 rpm.
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("F40C".to_string()),
            name: None,
            variant: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "F40C");
    assert_eq!(result.0.name.as_deref(), Some("Engine RPM"));
    assert_eq!(result.0.unit.as_deref(), Some("rpm"));
    let value = result.0.scaled_value.expect("standard PID should scale");
    assert!((value - 850.0).abs() < 1e-6, "got {value}");
    // Raw bytes are always present alongside the scaled value.
    assert_eq!(result.0.raw_hex, "0D 48");
}

// M6 Part B: the full dynamic-measurement path — define -> read -> scale — over
// real loopback frames, with NO BYO data. The engine-temp formula is public and
// the 2C/22 frames are DERIVED from the d72n47a0 disassembly (docs/sgbd-findings.md
// §7a). Exercises the same client + semantic code the read_data tool runs for a
// dynamic SG_FUNKTIONEN measurement, without the proprietary `.prg`.
#[tokio::test]
async fn dynamic_measurement_defines_reads_and_scales() {
    use klartext_client::{ClientConfig, DiagnosticClient};
    use klartext_semantic::{DataType, Measurement, build_read_request};

    let (addr, _frames) = spawn_mock_gateway().await;
    let config = ClientConfig {
        port: addr.port(),
        ..ClientConfig::default()
    };
    let client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

    // The engine-temperature measurement (id 0x4BC3, u16, SERVICE "22;2C") — the
    // public scaling formula, not BMW data.
    let measurement = Measurement {
        arg: "ITMOT".to_string(),
        id: 0x4BC3,
        result_name: "STAT_MOTORTEMPERATUR_WERT".to_string(),
        description: "Motortemperatur".to_string(),
        unit: "degC".to_string(),
        datatype: DataType::U16,
        mul: 0.1,
        div: 1.0,
        add: -273.14,
        sg_adr: "12".to_string(),
        service: "22;2C".to_string(),
    };
    assert!(measurement.is_dynamic());

    // define -> read over the wire, then scale via Part A.
    let requests = build_read_request(&measurement);
    let raw = client
        .read_dynamic_measurement(0x12, &requests)
        .await
        .unwrap();
    assert_eq!(raw, vec![0x0E, 0x2F]); // raw bytes preserved
    let scaled = measurement.scaled(&raw).expect("scales");
    assert_eq!(scaled.name, "Motortemperatur");
    assert_eq!(scaled.unit, "degC");
    assert!((scaled.value - 89.96).abs() < 0.01, "got {}", scaled.value);
}

// M6 Part B: a proprietary DYNAMIC measurement (SERVICE "22;2C") read through the
// MCP read_data tool with the real SGBD — the server runs the 0x2C define + 0x22
// read sequence (answered by the mock) then scales. Ignored by default (needs the
// BYO `.prg`); run with `--ignored`. Offline precursor to the on-car manual step.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_data_scales_a_proprietary_measurement() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: Some("4BC3".to_string()),
            name: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.0.name.as_deref(), Some("Motortemperatur"));
    assert_eq!(result.0.unit.as_deref(), Some("degC"));
    let value = result
        .0
        .scaled_value
        .expect("proprietary value should scale");
    assert!((value - 89.96).abs() < 0.01, "got {value}");
    assert_eq!(result.0.raw_hex, "0E 2F");
}

#[tokio::test]
async fn read_data_requires_exactly_one_of_did_or_name() {
    // The did/name contract is checked before any connection is touched.
    let server = KlartextServer::new(test_config());
    for (did, name) in [(None, None), (Some("F190"), Some("ITMOT"))] {
        let result = server
            .read_data(Parameters(ReadDataRequest {
                ecu: "0x10".to_string(),
                did: did.map(String::from),
                name: name.map(String::from),
                variant: None,
            }))
            .await;
        let Err(err) = result else {
            panic!("expected an error for did={did:?} name={name:?}, got Ok");
        };
        assert!(err.message.contains("exactly one"), "{}", err.message);
    }
}

#[tokio::test]
async fn read_data_with_an_unservable_variant_errors_loudly() {
    // An explicit `variant` the server cannot load (here: no --sgbd-dir) is a
    // configuration error, not a silent degrade-to-raw — the caller asked for
    // scaled values and must learn why they cannot have them.
    let server = KlartextServer::new(test_config());
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: Some("4BC3".to_string()),
            name: None,
            variant: Some("d72n47a0".to_string()),
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a no-SGBD error, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

#[tokio::test]
async fn read_data_by_name_requires_a_variant() {
    // A name can only be resolved through an SGBD catalog, which `variant` picks.
    let server = KlartextServer::new(test_config());
    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: None,
            name: Some("Motortemperatur".to_string()),
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without variant, got Ok");
    };
    assert!(err.message.contains("variant"), "{}", err.message);
}

// M9 Part A discover→read: a measurement found via list_measurements is read by
// NAME — no DID knowledge needed. The server resolves "Motortemperatur" through
// the real SGBD to id 0x4BC3, runs the dynamic 2C/22 sequence (answered by the
// mock), and returns the scaled value + unit. Ignored by default (BYO `.prg`).
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn read_data_reads_a_measurement_by_name() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x12".to_string(),
            did: None,
            name: Some("Motortemperatur".to_string()),
            variant: Some("d72n47a0".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(result.0.did_hex, "4BC3");
    assert_eq!(result.0.name.as_deref(), Some("Motortemperatur"));
    assert_eq!(result.0.unit.as_deref(), Some("degC"));
    let value = result.0.scaled_value.expect("scales by name");
    assert!((value - 89.96).abs() < 0.01, "got {value}");
}

#[tokio::test]
async fn read_data_rejects_bad_did_hex() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_data(Parameters(ReadDataRequest {
            ecu: "0x10".to_string(),
            did: Some("ZZZZ".to_string()),
            name: None,
            variant: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an invalid-DID error, got Ok");
    };
    assert!(err.message.contains("invalid DID hex"), "{}", err.message);
}

#[tokio::test]
async fn clear_faults_refuses_without_confirm() {
    // The confirmation gate is checked before anything else — even before the
    // "not connected" check — so a refusal never touches the car.
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    assert!(err.message.contains("confirm"), "{}", err.message);
    assert!(err.message.contains("freeze-frame"), "{}", err.message);
    assert!(err.message.contains("readiness"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn clear_faults_confirmed_but_disconnected_errors_clearly() {
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: true,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a not-connected error, got Ok");
    };
    assert!(err.message.contains("not connected"), "{}", err.message);
}

// M9 Part B: the confirmed clear over the wire — and the refined safety invariant,
// behaviorally: every frame this write path sends is ISO-standard UDS (DTC pre-read,
// extended session, ClearDiagnosticInformation). No derived/proprietary frame, ever.
#[tokio::test]
async fn clear_faults_with_confirm_clears_and_sends_only_standard_frames() {
    let (addr, frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .clear_faults(Parameters(ClearFaultsRequest {
            ecu: "0x40".to_string(),
            confirm: true,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.address, "0x40");
    assert!(result.0.cleared);
    // The pre-read records EVERY stored code discarded — relevant and not-tested.
    assert_eq!(
        result.0.codes_cleared,
        vec!["D9040A".to_string(), "AABBCC".to_string()]
    );
    assert_eq!(result.0.count, 2);
    assert!(result.0.note.contains("read_faults"), "{}", result.0.note);

    let frames = frames.lock().unwrap().clone();
    assert_eq!(
        frames,
        vec![
            vec![0x22, 0xF1, 0x90],       // connect: VIN read (from the gateway)
            vec![0x19, 0x02, 0xFF],       // pre-read: record what will be discarded
            vec![0x10, 0x03],             // extended session (required before a clear)
            vec![0x14, 0xFF, 0xFF, 0xFF], // standard clear-all (M2 path, no new frame)
        ]
    );
}

// Item 5 P2: run_job executes a read job (STATUS_LESEN) end to end over the
// read-only live stack and surfaces its named result sets on the MCP surface — the
// same path the CLI `job run` (Task 7) exposes. The real DDE bytecode builds the
// BMW-FAST telegram, the gate passes the static 0x22 read to the car, and the job
// scales the SG_FUNKTIONEN row. BYO-data gated on the DDE `.prg`; the wire value is
// DERIVED, [verify against capture] (car session 1).
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn run_job_reads_named_results_over_the_read_only_gate() {
    let (addr, frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from([
        "klartext-mcp",
        "--gateway-ip",
        &addr.ip().to_string(),
        "--port",
        &addr.port().to_string(),
        "--semantic-db",
        db.to_str().unwrap(),
        "--sgbd-dir",
        &sgbd_dir,
    ]);
    let server = KlartextServer::new(config);
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .run_job(Parameters(RunJobRequest {
            ecu: "0x12".to_string(),
            variant: Some("d72n47a0".to_string()),
            job: "STATUS_LESEN".to_string(),
            args: vec!["ARG".to_string(), "ITOEL".to_string()],
        }))
        .await
        .unwrap();

    // The job emitted named results; the scaled reading surfaces as a `…_WERT`
    // value facet, and nothing was truncated (well under the per-call cap).
    let names: Vec<&str> = result
        .0
        .sets
        .iter()
        .flatten()
        .map(|v| v.name.as_str())
        .collect();
    assert!(result.0.total >= 1, "no results: {:?}", result.0.sets);
    assert!(
        names.iter().any(|n| n.contains("WERT")),
        "no _WERT value in {names:?}"
    );
    assert!(
        result.0.note.is_none(),
        "unexpected truncation: {:?}",
        result.0.note
    );

    // The read-only gate passed exactly the static 0x22 read to the car — and no
    // write frame (0x2E write / 0x31 routine) ever reached the mock.
    let frames = frames.lock().unwrap().clone();
    assert!(
        frames.iter().any(|f| f.as_slice() == [0x22, 0x45, 0x17]),
        "expected the DID read frame, got {frames:02X?}"
    );
    assert!(
        !frames
            .iter()
            .any(|f| matches!(f.first(), Some(0x2E | 0x31 | 0x2F | 0x14 | 0x27))),
        "a write frame reached the car: {frames:02X?}"
    );
}

// Item 5 P2 — the BEHAVIORAL half of the read-only invariant (the surface test
// `advertises_exactly_the_refined_tool_surface` is the structural half). `run_job`
// runs an ECU's own bytecode, which could in principle emit ANY UDS service; the
// single thing that keeps it read-only is the `GatedExchange::read_only` its
// transport is wrapped in (server.rs `run_job`). Here we drive that EXACT gate
// composition — `GatedExchange::read_only(TelegramExchange::new(<bridge over a real
// client>))` — against the suite's frame-recording mock, feeding it a WRITE (0x2E)
// telegram of the shape a `STEUERN_*` job's `xsend` would build. The gate must
// refuse it at the transmit boundary, so NO write frame reaches the wire; a READ
// (0x22) through the same stack then DOES reach the mock, proving the write's
// absence is the gate refusing — not a severed transport.
//
// Why not drive the whole `run_job` TOOL with a write-emitting job? That needs a BYO
// `.prg` whose bytecode emits a write (BMW data, uncommittable), so the tool-level
// write case cannot be a committed test — the read-path tool test
// `run_job_reads_named_results_over_the_read_only_gate` is `#[ignore]` for the same
// reason. This proves the same seam `run_job` relies on, over the real client + HSFZ
// transport, with no BYO data. (The gate's own veto — write refused, inner never
// touched — is unit-tested in `crates/best/src/gate.rs`; the Refused→invalid_request
// mapping in `mcp/src/server.rs`. The `ClientBridge` below is byte-identical to the
// crate-private `SessionBridge` `run_job` uses.)
#[tokio::test]
async fn run_job_gate_refuses_a_write_before_the_wire() {
    use klartext_best::{
        BareUdsTransport, ExchangeError, GatedExchange, TelegramExchange, UdsExchange, encode,
    };
    use klartext_client::{ClientConfig, DiagnosticClient};

    // A bare-UDS bridge onto the live client — identical to the server's private
    // `SessionBridge`, reproduced here only because that type is crate-internal.
    struct ClientBridge<'a> {
        client: &'a DiagnosticClient,
    }
    #[async_trait::async_trait]
    impl BareUdsTransport for ClientBridge<'_> {
        async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            self.client
                .request(target, uds)
                .await
                .map_err(|e| ExchangeError::Transport(format!("{e}")))
        }
    }

    let (addr, frames) = spawn_mock_gateway().await;
    let client = DiagnosticClient::connect(
        addr.ip(),
        &ClientConfig {
            port: addr.port(),
            ..Default::default()
        },
    )
    .await
    .expect("connect to mock gateway");

    // The EXACT stack `run_job` wraps its transport in (server.rs `run_job`).
    let gate = GatedExchange::read_only(TelegramExchange::new(ClientBridge { client: &client }));

    // A write telegram of the shape a STEUERN_* job's `xsend` transmits: 0x2E
    // writeDataByIdentifier to ECU 0x12. The gate must refuse it at the seam.
    let write = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
    match gate.request(0x12, &write).await {
        Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
        other => panic!("expected the read-only gate to refuse the write, got {other:?}"),
    }
    // No write/actuation/flashing frame reached the car — the gate refused before the
    // transport was ever touched.
    let after_write = frames.lock().unwrap().clone();
    assert!(
        !after_write.iter().any(|f| matches!(
            f.first(),
            Some(0x2E | 0x31 | 0x2F | 0x14 | 0x27 | 0x34..=0x37)
        )),
        "a write frame reached the car: {after_write:02X?}"
    );

    // Positive control: a READ (0x22 VIN) through the SAME gate DOES reach the mock,
    // proving the transport is live — so the write's absence above is the gate
    // refusing, not a dead link.
    gate.request(0x12, &encode(0x12, 0xF1, &[0x22, 0xF1, 0x90]))
        .await
        .expect("the read-only gate must pass a read to the car");
    let after_read = frames.lock().unwrap().clone();
    assert!(
        after_read
            .iter()
            .any(|f| f.as_slice() == [0x22, 0xF1, 0x90]),
        "the read never reached the car: {after_read:02X?}"
    );
}

// The refined M9 surface invariant, in its P2 form: read tools — now including the
// read-only EDIABAS job runner `run_job` — plus exactly ONE standard, non-physical,
// confirmation-gated write (clear_faults). NO physical actuation and NO
// service-function/derived-unconfirmed-frame WRITE may ever appear as a tool — those
// stay human-executed in the CLI. `run_job` runs a job's bytecode over a read-only
// SID gate, so it is a READ on the surface, not a write exception. (The wire-level
// half of the invariant — only standard frames leave the clear path, and NO write
// frame leaves the run_job path — is asserted by
// `clear_faults_with_confirm_clears_and_sends_only_standard_frames` and
// `run_job_gate_refuses_a_write_before_the_wire`.)
#[test]
fn advertises_exactly_the_refined_tool_surface() {
    let server = KlartextServer::new(test_config());
    let mut tools = server.advertised_tools();
    tools.sort();
    assert_eq!(
        tools,
        vec![
            "clear_all_faults".to_string(),
            "clear_faults".to_string(),
            "connect".to_string(),
            "disconnect".to_string(),
            "fault_help".to_string(),
            "identify_vehicle".to_string(),
            "list_ecus".to_string(),
            "list_measurements".to_string(),
            "list_service_functions".to_string(),
            "read_all_faults".to_string(),
            "read_data".to_string(),
            "read_fault_detail".to_string(),
            "read_faults".to_string(),
            "read_info_memory".to_string(),
            "run_job".to_string(),
            "scan_ecus".to_string(),
        ]
    );
    // `list_service_functions` LISTS control functions and `run_job` RUNS an
    // EDIABAS job — but neither may gain the power to actuate or write: every
    // actuation/write verb is forbidden as a substring of every tool name. The
    // blanket `"run"` ban that stood before P2 is dropped (it would now catch the
    // admitted read-only `run_job`); the specific write/actuation verbs it proxied
    // for stay, and a dedicated check below pins `run_job` as the ONLY `run`-named
    // tool. `run_job`'s read-only-ness is proven on the wire by
    // `run_job_gate_refuses_a_write_before_the_wire`.
    for forbidden in [
        "actuat",
        "io_control",
        "execut",
        "routine",
        "regen",
        "calibrat",
        "write",
        "code",
        "coding",
        "reset",
        "flash",
    ] {
        assert!(
            !tools.iter().any(|t| t.contains(forbidden)),
            "forbidden tool present: {forbidden}"
        );
    }
    // The ONE admitted `run`-named tool is the read-only job runner; a future
    // `run_actuator` (or any other `run*` write) would still trip this dedicated
    // check even though the blanket `"run"` substring ban is gone.
    let run_tools: Vec<&str> = tools
        .iter()
        .filter(|t| t.contains("run"))
        .map(String::as_str)
        .collect();
    assert_eq!(run_tools, vec!["run_job"], "only run_job may contain 'run'");
    // "clear" appears only on the two confirmation-gated clears (per-ECU + whole-car),
    // both standard UDS 0x14 — never on an actuation/coding verb.
    let mut clears: Vec<&String> = tools.iter().filter(|t| t.contains("clear")).collect();
    clears.sort();
    assert_eq!(clears, vec!["clear_all_faults", "clear_faults"]);
}

#[tokio::test]
async fn list_measurements_requires_an_sgbd_dir() {
    // Without --sgbd-dir there is no measurement catalog to serve; the tool errors
    // clearly rather than inventing entries.
    let server = KlartextServer::new(test_config());
    let result = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            search: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without --sgbd-dir, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

#[tokio::test]
async fn list_measurements_falls_back_to_the_ista_catalog_without_an_sgbd() {
    // No --sgbd-dir, but the semantic DB carries the measurement catalog: an
    // inline-scaling ECU (no SGBD) still lists — names + units + reading job from
    // ISTA's index, marked source "ista_catalog".
    let (_dir, db) = fixture_db();
    let config = ServerConfig::parse_from(["klartext-mcp", "--semantic-db", db.to_str().unwrap()]);
    let server = KlartextServer::new(config);
    let result = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("dde_x".to_string()),
            ecu: None,
            search: None,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.total, 2);
    assert!(
        result
            .0
            .measurements
            .iter()
            .all(|m| m.source == "ista_catalog"),
        "all entries should be catalog-sourced"
    );
    let temp = result
        .0
        .measurements
        .iter()
        .find(|m| m.name == "STAT_EXAMPLE_TEMP_WERT")
        .unwrap();
    assert_eq!(temp.unit, "°C");
    assert_eq!(temp.job.as_deref(), Some("STATUS_LESEN"));
    assert!(
        result.0.note.contains("ISTA measurement catalog"),
        "{}",
        result.0.note
    );
    // The search filter applies to the catalog too.
    let filtered = server
        .list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("dde_x".to_string()),
            ecu: None,
            search: Some("RPM".to_string()),
        }))
        .await
        .unwrap();
    assert_eq!(filtered.0.total, 1);
}

// M9 Part A over the real DDE SGBD: the diesel-useful live-data set — oil temp,
// coolant temp, DPF soot/ash mass, regeneration status, engine RPM — surfaces from
// SG_FUNKTIONEN by name, and the huge catalog is capped with an explicit note (no
// silent truncation). Ignored by default (BYO data); run with `--ignored`.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn list_measurements_lists_the_real_dde_catalog() {
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from(["klartext-mcp", "--sgbd-dir", &sgbd_dir]);
    let server = KlartextServer::new(config);
    let list = |search: Option<&str>| {
        server.list_measurements(Parameters(ListMeasurementsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            search: search.map(String::from),
        }))
    };

    // Unfiltered: the DDE defines ~1800 measurements; the reply caps and says so.
    let all = list(None).await.unwrap();
    assert!(all.0.total > 1000, "got {}", all.0.total);
    assert_eq!(all.0.count, all.0.measurements.len());
    assert!(all.0.count < all.0.total, "expected the cap to apply");
    assert!(all.0.note.contains("search"), "{}", all.0.note);

    // Oil temperature: id 4517 (ITOEL), scaled in degC.
    let oil = list(Some("Öltemperatur")).await.unwrap();
    let itoel = oil
        .0
        .measurements
        .iter()
        .find(|m| m.arg == "ITOEL")
        .expect("ITOEL in the oil-temperature listing");
    assert_eq!(itoel.id_hex, "4517");
    assert_eq!(itoel.unit, "degC");
    assert_eq!(itoel.name, "gefilterte Öltemperatur");
    // The listed ECU address round-trips into read_data/read_faults' `ecu` form.
    assert_eq!(itoel.ecu_address, "0x12");

    // Coolant, DPF soot mass, regeneration status, engine RPM — all discoverable.
    for (search, arg) in [
        ("Kühlmitteltemperatur", "ITKUM"),
        ("Rußmasse", "IMRUP"),
        ("Regenerationsanforderung", "PFltRgn_numRgn"),
        ("Motordrehzahl", "Nkw"),
    ] {
        let found = list(Some(search)).await.unwrap();
        assert!(
            found.0.measurements.iter().any(|m| m.arg == arg),
            "search '{search}' should surface {arg}"
        );
    }
}

#[tokio::test]
async fn list_service_functions_requires_an_sgbd_dir() {
    // With no --sgbd-dir configured, the catalog cannot be served and the tool errors
    // clearly rather than executing or panicking.
    let server = KlartextServer::new(test_config());
    let result = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: None,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected an error without --sgbd-dir, got Ok");
    };
    assert!(err.message.contains("no SGBD"), "{}", err.message);
}

// The read-only service-function listing over the real DDE SGBD. Ignored by default
// (needs the BYO `.prg`); run with `--ignored`. Asserts the catalog, risk tiers, and
// derivation status — and that NO frame bytes are exposed (list-only).
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn list_service_functions_lists_the_real_dde_catalog() {
    let sgbd_dir = sgbd_test_dir();
    let config = ServerConfig::parse_from(["klartext-mcp", "--sgbd-dir", &sgbd_dir]);
    let server = KlartextServer::new(config);

    // Full catalog: 160 functions (156 control-table rows + 4 derived resets).
    let all = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: None,
        }))
        .await
        .unwrap();
    assert_eq!(all.0.count, all.0.functions.len());
    assert!(all.0.count > 100, "got {}", all.0.count);

    // The engine-oil CBS reset: low-risk, derived (unconfirmed), runnable in the CLI.
    let oil = all.0.functions.iter().find(|f| f.label == "Oel").unwrap();
    assert_eq!(oil.risk, "low");
    assert_eq!(oil.derivation, "derived-unconfirmed");
    assert!(oil.runnable_in_cli);
    assert!(oil.citation.as_deref().unwrap().contains("CBS_RESET"));

    // A throttle actuator: high-risk, never runnable via this surface.
    let dro = all.0.functions.iter().find(|f| f.label == "DRO").unwrap();
    assert_eq!(dro.risk, "high");
    assert!(!dro.runnable_in_cli);

    // The risk filter narrows to low-risk only.
    let low = server
        .list_service_functions(Parameters(ListServiceFunctionsRequest {
            variant: Some("d72n47a0".to_string()),
            ecu: None,
            risk: Some("low".to_string()),
        }))
        .await
        .unwrap();
    assert!(low.0.functions.iter().all(|f| f.risk == "low"));
    assert!(low.0.count < all.0.count);
}

// ── Whole-car tools (scan_ecus / read_all_faults / clear_all_faults) ──────────

#[tokio::test]
async fn scan_ecus_finds_only_the_fitted_set() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await
        .unwrap();
    // The gateway's SVT lists {0x10,0x12,0x40}; 0x18 is in the DB model map but not
    // fitted on this car, so the SVT read excludes it.
    let addrs: Vec<&str> = result
        .0
        .ecus
        .iter()
        .map(|e| e.address_hex.as_str())
        .collect();
    assert_eq!(addrs, ["0x10", "0x12", "0x40"]);
    // Names come from the DB.
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));
}

#[tokio::test]
async fn read_all_faults_reads_every_fitted_ecu_and_partitions() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server
        .read_all_faults(Parameters(ReadAllFaultsRequest { rescan: false }))
        .await
        .unwrap();
    // One EcuFaults entry per fitted ECU; each has the one relevant fault and one
    // not-tested entry counted.
    assert_eq!(result.0.ecus.len(), 3);
    assert_eq!(result.0.total_relevant, 3);
    for ecu in &result.0.ecus {
        assert_eq!(ecu.faults.len(), 1, "{}", ecu.address_hex);
        assert_eq!(ecu.faults[0].code_hex, "D9040A");
        assert_eq!(ecu.not_tested_count, 1);
        assert!(ecu.error.is_none());
    }
}

#[tokio::test]
async fn clear_all_faults_refuses_without_confirm() {
    // The whole-car confirmation gate refuses before touching anything.
    let server = KlartextServer::new(test_config());
    let result = server
        .clear_all_faults(Parameters(ClearAllFaultsRequest {
            confirm: false,
            rescan: false,
        }))
        .await;
    let Err(err) = result else {
        panic!("expected a refusal without confirm, got Ok");
    };
    assert!(err.message.contains("whole car"), "{}", err.message);
    assert!(err.message.contains("freeze-frame"), "{}", err.message);
    assert!(!err.message.contains("not connected"), "{}", err.message);
}

#[tokio::test]
async fn clear_all_faults_confirmed_clears_every_fitted_ecu_and_verifies() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();
    // Scan first so the fitted list is cached.
    server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await
        .unwrap();

    let result = server
        .clear_all_faults(Parameters(ClearAllFaultsRequest {
            confirm: true,
            rescan: false,
        }))
        .await
        .unwrap();
    assert_eq!(result.0.ecus.len(), 3);
    assert_eq!(result.0.cleared_clean, 3);
    for ecu in &result.0.ecus {
        assert!(ecu.verified_clean, "{}", ecu.address_hex);
        // Every ECU stored both codes before the clear (the discard record).
        assert_eq!(
            ecu.codes_before,
            vec!["D9040A".to_string(), "AABBCC".to_string()]
        );
        assert!(ecu.error.is_none());
    }
}

// ── identify_vehicle (M11 Item 2) ─────────────────────────────────────────────

#[tokio::test]
async fn identify_vehicle_returns_vin_and_named_fitted_list() {
    let (addr, _frames) = spawn_mock_gateway().await;
    let (_dir, db) = fixture_db();
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    let result = server.identify_vehicle().await.unwrap();
    // VIN and I-Stufe come from the ZGW identity reads; the FA raw region round-trips.
    assert_eq!(result.0.vin.as_deref(), Some("WBA3B5C50EK123456"));
    assert_eq!(result.0.i_stufe.as_deref(), Some("F020-21-11-500"));
    assert_eq!(result.0.vehicle_order.raw_hex, "AA BB");
    // FA fields stay capture-gated: the raw region is too short to carry a version.
    assert_eq!(result.0.vehicle_order.version, None);

    // The fitted list is the gateway SVT {0x10,0x12,0x40}, named from the DB.
    let addrs: Vec<&str> = result
        .0
        .ecus
        .iter()
        .map(|e| e.address_hex.as_str())
        .collect();
    assert_eq!(addrs, ["0x10", "0x12", "0x40"]);
    let fem = result
        .0
        .ecus
        .iter()
        .find(|e| e.address_hex == "0x40")
        .unwrap();
    assert_eq!(fem.group_name.as_deref(), Some("d_0040"));
    assert_eq!(fem.title.as_deref(), Some("Front Electronic Module"));

    // One identification block per fitted ECU; the VIN DID (F190) decodes by name at
    // the surface (the client returns raw), the rest answered negatively and skipped.
    assert_eq!(result.0.identification.len(), 3);
    let vin_field = result
        .0
        .identification
        .iter()
        .flat_map(|b| &b.fields)
        .find(|f| f.did_hex == "F190")
        .expect("VIN identification field present");
    assert_eq!(vin_field.name.as_deref(), Some("VIN"));
    assert_eq!(vin_field.text.as_deref(), Some("WBA3B5C50EK123456"));
}

/// A loopback gateway that answers the VIN (so `connect` succeeds) but REJECTS the
/// SVT installed-ECU read (`22 3F 07`) with a negative response.
///
/// Regression fixture for the no-fallback invariant: a failed SVT read must surface
/// as an error from the discovery tools, never degrade to an empty/partial success.
async fn spawn_mock_gateway_svt_fails() -> std::net::SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                    if frame.control != control::DIAGNOSTIC {
                        continue;
                    }
                    let (tester, ecu) = frame.addr.unwrap();
                    let uds = match frame.payload.as_slice() {
                        [0x3E, 0x80] => continue,         // keepalive — no reply
                        [0x3E, 0x00] => vec![0x7E, 0x00], // presence probe
                        [0x22, 0xF1, 0x90] => {
                            let mut uds = vec![0x62, 0xF1, 0x90];
                            uds.extend_from_slice(b"WBA3B5C50EK123456");
                            uds
                        }
                        // The SVT read is REJECTED (requestOutOfRange). The client must
                        // propagate this, not fall back to an empty fitted list.
                        [0x22, 0x3F, 0x07] => vec![0x7F, 0x22, 0x31],
                        _ => continue,
                    };
                    let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap SRC/TGT
                    let _ = write_frame(&mut stream, &reply).await;
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn discovery_errors_when_the_svt_read_fails() {
    // Regression lock (Task 8 review): a failed installed-ECU (SVT) read must surface
    // as an ERROR from the discovery tools, never a degraded empty/partial success.
    let (_dir, db) = fixture_db();
    let addr = spawn_mock_gateway_svt_fails().await;
    let server = KlartextServer::new(config_for_mock(addr, &db));
    server
        .connect(Parameters(ConnectRequest { gateway_ip: None }))
        .await
        .unwrap();

    // scan_ecus reads the SVT directly — a rejected read is an error, not empty ECUs.
    let scan = server
        .scan_ecus(Parameters(ScanEcusRequest { rescan: false }))
        .await;
    let Err(err) = scan else {
        panic!("expected scan_ecus to error on a failed SVT read, got Ok");
    };
    assert!(err.message.contains("SVT"), "{}", err.message);

    // identify_vehicle aggregates the same SVT read (no probe fallback) — it must fail
    // too, not return a degraded identity with an empty fitted list.
    let ident = server.identify_vehicle().await;
    let Err(err) = ident else {
        panic!("expected identify_vehicle to error on a failed SVT read, got Ok");
    };
    assert!(err.message.contains("vehicle identity"), "{}", err.message);
}
