//! `IS_LESEN`: the info-memory store read, whose REQUEST FRAME is a per-ECU choice.
//!
//! klartext's direct read hardcodes `22 2000`. Measured across the shipped SGBD set
//! by extracting the literal each `IS_LESEN` moves into its request register (the
//! `move S1, <literal>` at op 3, the same slot in every variant):
//!
//! | `IS_LESEN` request template | SGBDs |
//! |---|---|
//! | `22 20 00` | 334 |
//! | `19 17 0C 01` (ISO 14229 `reportUserDefMemoryDTCByStatusMask`) | 255 |
//! | built dynamically | 294 |
//!
//! The split is generational — `acsm3`/`acsm4`/`acsm5` use the former, `acsm6`/
//! `acsm7`/`adcam_*`/`bat48_*` the latter. On one of the 255, the hardcoded frame
//! draws a negative and klartext concludes "this ECU has no info memory" when it
//! has one (`docs/car-session-2-results.md` §3.7).
//!
//! These tests prove the job settles it without klartext choosing: ONE job name,
//! two request families, the same decoded results.
//!
//! BYO data: skipped (not failed) when `data/Testmodule(1)/Ecu` is absent.

use klartext_best::{Ecu, MockExchange, ResultData};

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

/// Runs `IS_LESEN` on `sgbd` at `address`, answering ONLY `request` — so the job
/// reaching `eoj` with results proves it emitted exactly that frame.
async fn run_is_lesen(sgbd: &str, address: u8, request: &[u8], response: &[u8]) -> Vec<Vec<u8>> {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu").join(sgbd);
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return Vec::new();
    }
    let ecu = Ecu::open(&path).unwrap();
    let mut exchange = MockExchange::new();
    let mut framed = vec![0x80 | u8::try_from(request.len()).unwrap(), address, 0xF1];
    framed.extend_from_slice(request);
    exchange.on(framed, response_telegram(address, response));

    let results = ecu
        .run_job("IS_LESEN", address, b"", &exchange)
        .await
        .expect("IS_LESEN must run to eoj");
    results
        .iter_sets()
        .flatten()
        .filter_map(|(name, value)| match (name, value) {
            ("F_HEX_CODE", ResultData::Binary(bytes)) => Some(bytes.clone()),
            _ => None,
        })
        .collect()
}

/// The `22 20 00` family (the F25 DDE). Entries are the three the real car held.
#[tokio::test]
async fn is_lesen_on_the_22_2000_family_reads_its_entries() {
    let entries = run_is_lesen(
        "d72n47a0.prg",
        0x12,
        &[0x22, 0x20, 0x00],
        &[
            0x62, 0x20, 0x00, 0x27, 0x7F, 0x00, 0x20, 0x2C, 0x9D, 0x00, 0x20, 0x36, 0xF8, 0x00,
            0x2F,
        ],
    )
    .await;
    if entries.is_empty() {
        return; // BYO data absent
    }
    assert_eq!(entries.len(), 3, "three info entries, got {entries:?}");
    assert_eq!(entries[0], vec![0x27, 0x7F, 0x00, 0x20]);
    assert_eq!(entries[2], vec![0x36, 0xF8, 0x00, 0x2F]);
}

/// The `19 17 0C 01` family (the newer airbag ECU) — a DIFFERENT service, decoded
/// by the same job into the same `[code: 3][status: 1]` records.
///
/// This is the one klartext's hardcoded `22 2000` gets wrong. The mock answers only
/// `19 17 0C 01`, so the job could not have passed by sending anything else.
#[tokio::test]
async fn is_lesen_on_the_19_17_family_reads_the_same_shape() {
    let entries = run_is_lesen(
        "acsm6.prg",
        0x01,
        &[0x19, 0x17, 0x0C, 0x01],
        &[
            0x59, 0x17, 0x01, 0xFF, 0x27, 0x7F, 0x00, 0x20, 0x36, 0xF8, 0x00, 0x2F,
        ],
    )
    .await;
    if entries.is_empty() {
        return; // BYO data absent
    }
    assert_eq!(entries.len(), 2, "two info entries, got {entries:?}");
    assert_eq!(entries[0], vec![0x27, 0x7F, 0x00, 0x20]);
    assert_eq!(entries[1], vec![0x36, 0xF8, 0x00, 0x2F]);
}
