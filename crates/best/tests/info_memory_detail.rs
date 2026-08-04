//! `IS_LESEN_DETAIL`: reading ONE info-memory entry's detail, the way ISTA does.
//!
//! The info-memory (Infospeicher) store has its own detail job, and ISTA runs it
//! per entry while iterating `sg.INFO`: `apiJob(sg.ECU_SGBD, "IS_LESEN_DETAIL",
//! item.F_ORT.ToString(), …)` (`RheingoldDiagnostics` `doECUReadISDetails`
//! :226004). `F_ORT` comes from `getintResult`, so that single positional argument
//! is the fault code in DECIMAL.
//!
//! This matters because the `19 09`/`19 06`/`19 04` services address the FAULT
//! memory only — pointing them at an info-memory code drew two `7F 19 31`s from the
//! F25 DDE on 2026-08-02 (`docs/car-session-2-results.md` §3.6). The job is the
//! right instrument, and letting it build its own frames is also why klartext
//! invents no `22 20 <position>` record layout: the bytecode knows it, we do not.
//!
//! BYO data: skipped (not failed) when `data/Testmodule(1)/Ecu` is absent.

use klartext_best::{Ecu, MockExchange};

const DDE: u8 = 0x12;

/// The BMW-FAST response telegram a real ECU returns for `uds`.
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

/// A mock answering the store read with the three entries the real F25 DDE held on
/// 2026-08-02, plus a per-entry answer for any position the job asks for.
fn dde_info_memory() -> MockExchange {
    let mut exchange = MockExchange::new();
    exchange.on(
        vec![0x83, DDE, 0xF1, 0x22, 0x20, 0x00],
        response_telegram(
            DDE,
            &[
                0x62, 0x20, 0x00, 0x27, 0x7F, 0x00, 0x20, 0x2C, 0x9D, 0x00, 0x20, 0x36, 0xF8, 0x00,
                0x2F,
            ],
        ),
    );
    // The per-entry record layout has never been captured on a car, so these are
    // deliberately NOT asserted on — only that the job asks for one.
    for position in 0u8..=4 {
        exchange.on(
            vec![0x83, DDE, 0xF1, 0x22, 0x20, position],
            response_telegram(
                DDE,
                &[
                    0x62, 0x20, position, 0x36, 0xF8, 0x00, 0x2F, 0x00, 0x01, 0x00, 0x00, 0x12,
                    0x34,
                ],
            ),
        );
    }
    exchange
}

/// The job runs to `eoj` and drives the store read itself.
///
/// `_RESPONSE_2000` is the job's own record of the `22 20 00` exchange, so its
/// presence proves the frame went out — klartext transmits no hand-rolled
/// info-memory read here.
#[tokio::test]
async fn is_lesen_detail_runs_and_drives_its_own_info_memory_reads() {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    let ecu = Ecu::open(path).unwrap();
    // ISTA's `item.F_ORT.ToString()` for code 36 F8 00.
    let code = u32::from_be_bytes([0, 0x36, 0xF8, 0x00]).to_string();

    let results = ecu
        .run_job("IS_LESEN_DETAIL", DDE, code.as_bytes(), &dde_info_memory())
        .await
        .expect("IS_LESEN_DETAIL must run to eoj");

    let names: Vec<&str> = results.iter_sets().flatten().map(|(n, _)| n).collect();
    assert!(
        names.contains(&"_RESPONSE_2000"),
        "the job must perform the 22 2000 store read itself, got {names:?}"
    );
    assert!(
        names.contains(&"JOB_STATUS"),
        "the job must report a status rather than fail silently, got {names:?}"
    );
}

/// The argument is load-bearing: with NONE the job takes its own
/// "1 ARGUMENT NECESSARY - EITHER F_CODE OR F_POS" branch instead of reading.
///
/// This is the discriminating half — it proves the decimal `F_ORT` argument is
/// actually reaching the job's `parl`, not being ignored.
#[tokio::test]
async fn is_lesen_detail_without_an_argument_reports_its_own_argument_error() {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    let ecu = Ecu::open(path).unwrap();

    let results = ecu
        .run_job("IS_LESEN_DETAIL", DDE, b"", &dde_info_memory())
        .await
        .expect("an argument error is a job status, not a VM fault");

    let status = results
        .iter_sets()
        .flatten()
        .find(|(name, _)| *name == "JOB_STATUS")
        .map(|(_, value)| format!("{value:?}"))
        .expect("JOB_STATUS");
    assert!(
        status.to_uppercase().contains("ARGUMENT"),
        "expected the job's own missing-argument status, got {status}"
    );
    // …and with an argument it gets past that branch to a real read.
    let with_arg = ecu
        .run_job("IS_LESEN_DETAIL", DDE, b"3602432", &dde_info_memory())
        .await
        .unwrap();
    let names: Vec<&str> = with_arg.iter_sets().flatten().map(|(n, _)| n).collect();
    assert!(
        names.contains(&"_RESPONSE_2000"),
        "with F_ORT supplied the job must reach the store read, got {names:?}"
    );
}
