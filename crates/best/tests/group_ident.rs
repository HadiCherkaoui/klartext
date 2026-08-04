//! The group-SGBD IDENT rung: resolving an ECU's variant the way ISTA does.
//!
//! ISTA never guesses which `.prg` an ECU speaks. It runs the GROUP SGBD's
//! `IDENTIFIKATION` job and takes the job result `VARIANTE`
//! (`RheingoldDiagnostics` `DoAfterIdentProcessing`: `mECU.ECU_SGBD =
//! identJob.getStringResult("VARIANTE")`, :223322). That job is where the
//! variant answer actually comes from, and it is pure data plus bytecode:
//!
//! ```text
//! op   1  move S1, [83 FF FF 22 F1 50]        UDS ReadDataByIdentifier F150
//! op  29  xsend S3
//! op 551  tabsetex "ZuordnungsTabelleUDS", "t_grtb"
//! op 565  tabseek  "ADR_INDEX"                 "<addr> <ident index>"
//! op 583  tabget   S5, "SGBD"                  the variant name
//! op 593  ergs     "VARIANTE"
//! ```
//!
//! The whole job was one opcode short of running here — `tabsetex` (0xAA), which
//! switches the table source to ANOTHER SGBD file (`t_grtb`, the shared group
//! table). These tests are the proof it runs now, on real BYO data.
//!
//! BYO data: skipped (not failed) when `data/Testmodule(1)/Ecu` is absent.

use klartext_best::{Ecu, MockExchange};

/// The SGBD set, where the group files and `t_grtb` live together — EDIABAS's
/// `EcuPath`, which is what `tabsetex` resolves a base file against.
const ECU_DIR: &str = "../../data/Testmodule(1)/Ecu";

/// Builds the BMW-FAST response telegram a real ECU returns for `uds`.
///
/// `[0x80|len][target=F1][source][uds…][additive checksum]` — the short form,
/// which is all these ident answers need.
fn response_telegram(source: u8, uds: &[u8]) -> Vec<u8> {
    let mut telegram = vec![
        0x80 | u8::try_from(uds.len()).expect("short form"),
        0xF1,
        source,
    ];
    telegram.extend_from_slice(uds);
    let checksum = telegram.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
    telegram.push(checksum);
    telegram
}

/// `g_klima3.grp` `IDENTIFIKATION` resolves a real variant, end to end.
///
/// Address `0x7B` (rear climate) with ident index `0F11B0` is a genuine row of
/// `t_grtb`'s `ZuordnungsTabelleUDS`: `["7B 0F11B0", "HKA_02", "G_KLIMA3", "F02",
/// …]`. The job must reach that row through `tabsetex` and emit `HKA_02` — the
/// exact string ISTA assigns to `ECU_SGBD`.
#[tokio::test]
async fn group_ident_job_resolves_the_variant_through_the_shared_group_table() {
    let path = std::path::Path::new(ECU_DIR).join("g_klima3.grp");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    let ecu = Ecu::open(&path).unwrap();

    let mut exchange = MockExchange::new();
    // The telegram the job itself builds, observed from its own xsend: the
    // literal `83 FF FF 22 F1 50` with the addresses filled in.
    exchange.on(
        vec![0x83, 0x7B, 0xF1, 0x22, 0xF1, 0x50],
        response_telegram(0x7B, &[0x62, 0xF1, 0x50, 0x0F, 0x11, 0xB0]),
    );

    let results = ecu
        .run_job("IDENTIFIKATION", 0x7B, b"", &exchange)
        .await
        .expect("the group ident job must run to eoj");

    let variante = results
        .iter_sets()
        .flatten()
        .find(|(name, _)| *name == "VARIANTE")
        .map(|(_, value)| format!("{value:?}"))
        .expect("the job must emit VARIANTE — that is what ISTA reads");
    assert!(
        variante.contains("HKA_02"),
        "expected the ZuordnungsTabelleUDS row for `7B 0F11B0`, got {variante}"
    );
}

/// A different index selects a different variant from the same table — proof the
/// lookup is real and not a constant. `7B 0F21C0` is the G12 row (`HKA_G12`).
#[tokio::test]
async fn group_ident_maps_a_different_ident_index_to_a_different_variant() {
    let path = std::path::Path::new(ECU_DIR).join("g_klima3.grp");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    let ecu = Ecu::open(&path).unwrap();

    let mut exchange = MockExchange::new();
    exchange.on(
        vec![0x83, 0x7B, 0xF1, 0x22, 0xF1, 0x50],
        response_telegram(0x7B, &[0x62, 0xF1, 0x50, 0x0F, 0x21, 0xC0]),
    );

    let results = ecu
        .run_job("IDENTIFIKATION", 0x7B, b"", &exchange)
        .await
        .expect("the group ident job must run to eoj");

    let variante = results
        .iter_sets()
        .flatten()
        .find(|(name, _)| *name == "VARIANTE")
        .map(|(_, value)| format!("{value:?}"))
        .expect("VARIANTE");
    assert!(
        variante.contains("HKA_G12"),
        "a different ident index must select a different row, got {variante}"
    );
}
