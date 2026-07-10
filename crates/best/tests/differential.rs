//! Differential oracle: the VM running the DDE's generic `STATUS_LESEN` must
//! agree with `klartext-semantic`'s `SG_FUNKTIONEN` scaler on identical response
//! bytes (spec §8.3), plus a structured multi-result proof on the DSC (§8.4).
//! BYO data; skips when absent.
//!
//! ## What the exploration discovered (frozen here)
//! `STATUS_LESEN`'s real shape, established by running it against the BYO `.prg`
//! and reading its own error texts (Task 10 report §evidence):
//! * **Job arguments** are the EDIABAS `;`-joined string `"<SPALTE>;<STATUS…>"`,
//!   where `SPALTE` is the `SG_FUNKTIONEN` lookup column (`ARG`/`ID`/`LABEL`) and
//!   each following field is a value to read — e.g. `"ARG;ITOEL"` (NOT bare
//!   `"ITOEL"`, which the job rejects as `ARGUMENT_SPALTE='ITOEL' not valid`).
//! * **Requests are BMW-FAST telegrams**, `[0x80|len][target][source][uds…]`
//!   (observed `83 12 F1 22 45 17` = a static `0x22` read of DID `0x4517`), not the
//!   bare `[0x22, hi, lo]`. NOTE (corrected per the 2026-07-10 job audit): `STATUS_LESEN`
//!   is a static-only reader, so it emits this `22 <id>` for ANY measurement — but the
//!   DDE rows below are `SERVICE=22;2C` (DYNAMIC): a real ECU REJECTS the static read
//!   (`7F 22 31`, car-session-1 finding 1); the correct path is
//!   `measurement.rs::build_read_request` (the 2C define, unit-tested there; `run_job`
//!   now redirects dynamic measurements to it). This oracle validates only the VM's
//!   response DECODE against `measurement.rs`, feeding canned `22`-responses — it does
//!   NOT exercise the request path, which is why the routing bug slipped. The DSC
//!   `0x4005` case IS static (`SERVICE=22`), so there `STATUS_LESEN` is correct.
//! * **Responses must be BMW-FAST framed too**: `[0x80|len][0xF1][ecu][uds…]
//!   [checksum]`. The job length-checks `total == 1 + headerSize + dataLen`
//!   (a trailing checksum byte is required but its VALUE is never verified — the
//!   job strips it) and checks `resp[1]==0xF1` / `resp[2]==ecu`. A wrong length
//!   yields `JOB_STATUS=ERROR_ECU_INCORRECT_LEN`.
//! * On a well-formed response the job scales in bytecode and emits the row's
//!   `RESULTNAME` split into `…_WERT` (value) / `…_EINH` (unit) / `…_INFO`.

use klartext_best::{Ecu, ExchangeError, ResultData, UdsExchange};
use klartext_semantic::measurement::Measurements;
use klartext_sgbd::Prg;

/// DDE measurements sampled across the engine's direct-scale rows (id, ARG name):
/// oil temp (`0.01·raw − 100`), coolant temp (same), engine speed (`0.091554·raw`).
const SAMPLES: [(u16, &str); 3] = [(0x4517, "ITOEL"), (0x461B, "ITKUM"), (0x427F, "Nkw")];

/// A [`UdsExchange`] double that answers a BMW-FAST-framed static `0x22` read of
/// one DID with a canned raw value, re-framed as the ECU's response telegram.
///
/// It parses the request telegram (strips the 3-byte `[0x80|len][target][source]`
/// header to reach the UDS payload), matches the `0x22 <did>` read, and returns a
/// `[0x80|len][0xF1][ecu][62 <did> <raw>][checksum]` response — the exact shape
/// the job's length + address checks accept. The `0x2C` dynamic-define arms are
/// present for completeness though these rows use the static read.
struct DidExchange {
    did: u16,
    raw: Vec<u8>,
}

impl DidExchange {
    /// Wrap `uds` in the ECU→tester BMW-FAST short-form response telegram.
    ///
    /// The response direction puts the tester `0xF1` in the target byte and the
    /// `ecu` (echoed from the request's target) in the source byte, so this
    /// defers to the production [`klartext_best::encode`] with that mapping.
    /// Delegating means the frame carries the real ADDITIVE checksum a live ECU
    /// would send — the job length-checks the trailing byte but never verifies
    /// its value, so this still satisfies the job while exercising (and DRYing
    /// the test onto) the production codec.
    fn frame(ecu: u8, uds: &[u8]) -> Vec<u8> {
        klartext_best::encode(0xF1, ecu, uds)
    }
}

#[async_trait::async_trait]
impl UdsExchange for DidExchange {
    async fn request(&self, _target: u8, req: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // The request is a BMW-FAST telegram `[0x80|len][target][source][uds…]`;
        // the ECU address to echo back is the request's target byte.
        let ecu = *req
            .get(1)
            .ok_or_else(|| ExchangeError::Unexpected(req.to_vec()))?;
        let uds = req.get(3..).unwrap_or(req);
        let response = match uds {
            [0x22, hi, lo] if u16::from_be_bytes([*hi, *lo]) == self.did => {
                let mut r = vec![0x62, *hi, *lo];
                r.extend_from_slice(&self.raw);
                Self::frame(ecu, &r)
            }
            [0x2C, 0x03, ..] => Self::frame(ecu, &[0x6C, 0x03]),
            [0x2C, 0x01, id_hi, id_lo, ..] => Self::frame(ecu, &[0x6C, 0x01, *id_hi, *id_lo]),
            [0x22, 0xF3, 0x03] => {
                let mut r = vec![0x62, 0xF3, 0x03];
                r.extend_from_slice(&self.raw);
                Self::frame(ecu, &r)
            }
            other => return Err(ExchangeError::Unexpected(other.to_vec())),
        };
        Ok(response)
    }
}

/// The first `Real` result whose name marks it the scaled value (`…_WERT`).
fn scaled_wert(results: &klartext_best::ResultSet) -> Option<f64> {
    results.iter_current().find_map(|(n, v)| match v {
        ResultData::Real(f) if n.contains("WERT") => Some(*f),
        _ => None,
    })
}

/// The distinct RES-derived sub-result STEMS in `results`.
///
/// Every read emits wrapper boilerplate (`_REQUEST`, `_RESPONSE`, `JOB_STATUS`,
/// and `JOB_MESSAGE` on error) plus up to four facets per physical sub-result
/// (`<stem>_WERT`/`_EINH`/`_INFO`/`_TEXT`; a bitfield bit is its bare stem with
/// an `_INFO` facet). Dropping the wrappers and collapsing each name to its stem
/// counts physical sub-results, so this DISCRIMINATES multi-result decode from
/// boilerplate: the observed single-value DDE ITOEL read (`_REQUEST`,
/// `_RESPONSE`, `STAT_MOTOROEL_TEMPERATUR_{WERT,EINH,INFO}`, `JOB_STATUS` — six
/// names) collapses to exactly ONE stem and can never pass a `>= 3` bound.
fn res_stems(results: &klartext_best::ResultSet) -> std::collections::BTreeSet<String> {
    results
        .iter_current()
        .filter(|(n, _)| !n.starts_with('_') && *n != "JOB_STATUS" && *n != "JOB_MESSAGE")
        .map(|(n, _)| {
            ["_WERT", "_EINH", "_INFO", "_TEXT"]
                .iter()
                .find_map(|s| n.strip_suffix(s))
                .unwrap_or(n)
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn vm_status_lesen_agrees_with_the_rust_scaler() {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    let prg = Prg::open(path).unwrap();
    let measurements = Measurements::from_prg(&prg).unwrap();
    let ecu = Ecu::load(prg);

    for (id, arg) in SAMPLES {
        let m = measurements
            .get(id)
            .unwrap_or_else(|| panic!("{arg} in SG_FUNKTIONEN"));
        // Probe the row's width by trying the plausible raw sizes against the
        // Rust scaler (`scaled` returns None on a width mismatch). The bytes are
        // arbitrary but SHARED with the VM below, so any read-order disagreement
        // surfaces as a value mismatch — the whole point of the oracle.
        let (raw, rust) = [1usize, 2, 4]
            .into_iter()
            .find_map(|w| {
                let raw: Vec<u8> = [0x0A, 0xBC, 0x01, 0x02][..w].to_vec();
                m.scaled(&raw).map(|s| (raw, s))
            })
            .unwrap_or_else(|| panic!("{arg}: no raw width scales in measurement.rs"));

        // ARGUMENT_SPALTE=ARG, STATUS[1]=the row's ARG name (discovered grammar).
        let job_args = format!("ARG;{arg}");
        let exchange = DidExchange {
            did: id,
            raw: raw.clone(),
        };
        let results = ecu
            .run_job("STATUS_LESEN", 0x12, job_args.as_bytes(), &exchange)
            .await
            .unwrap_or_else(|e| panic!("STATUS_LESEN({arg}) failed: {e}"));

        let vm_value = scaled_wert(&results)
            .unwrap_or_else(|| panic!("STATUS_LESEN({arg}) emitted no scaled value: {results:?}"));
        assert!(
            (vm_value - rust.value).abs() < 1e-6,
            "{arg}: VM {vm_value} != Rust {} (raw {raw:02X?})",
            rust.value
        );
    }
}

#[tokio::test]
async fn vm_status_lesen_decodes_a_multi_row_res_table_on_the_dsc() {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu/dsc_10.prg");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    // dsc_10 SG_FUNKTIONEN row 0x4005 → RES_0x4005_D: 8 rows (2 BITFIELDs, five
    // unsigned-char scalars, a TAB_DSC_RADPOSITION-mapped position). Observed
    // decode of the 8-byte payload below (frozen from the run): the BITFIELDs
    // expand into 9 named bits (STAT_WARNUNG_AKTIV, STAT_REJECTION_PHASE,
    // STAT_SYSTEMFUNKTION_AKTIV, STAT_STANDARDISIERUNG_AKTIV,
    // STAT_BLINDPHASE_AKTIV, STAT_BREMSLICHTSCHALTER_AKTIV,
    // STAT_PLATTROLLEN_ERKANNT, STAT_3PLUS1_ERKANNT, STAT_NEUREIFEN_ERKANNT),
    // the scalars into STAT_NAEHERUNG_WARNGRENZE_S (%) and STAT_DSC_SIGNAL_
    // {VR,VL,HR,HL} (km/h), and the last byte into STAT_DEFLATION_POSITON with
    // a table-mapped `_TEXT` — 15 distinct sub-result stems, JOB_STATUS=OKAY.
    // Look the ARG name up from the table rather than hardcoding it.
    let prg = Prg::open(path).unwrap();
    let sgf = prg.table_ci("SG_FUNKTIONEN").unwrap();
    let arg_col = sgf.columns.iter().position(|c| c == "ARG").unwrap();
    let id_col = sgf.columns.iter().position(|c| c == "ID").unwrap();
    let row = sgf
        .rows
        .iter()
        .find(|r| r[id_col].eq_ignore_ascii_case("0x4005"))
        .unwrap();
    let arg = row[arg_col].clone();

    // The RES table decodes several bytes; give the read enough payload for every
    // sub-result (2 bitfield/status bytes + 5 unsigned-char scalars + 1 position).
    let exchange = DidExchange {
        did: 0x4005,
        raw: vec![0xFF, 0x42, 0x10, 0x20, 0x30, 0x40, 0x03, 0x02],
    };
    let ecu = Ecu::load(prg);
    let results = ecu
        .run_job(
            "STATUS_LESEN",
            0x29,
            format!("ARG;{arg}").as_bytes(),
            &exchange,
        )
        .await
        .unwrap();

    // Requirement (a): MULTIPLE physical sub-results decoded from ONE response.
    // Count distinct stems, not raw names — `res_stems` drops the wrapper
    // boilerplate every read emits, so a single-scalar read (one stem) FAILS
    // this bound; see the discriminator proof below.
    let stems = res_stems(&results);
    assert!(
        stems.len() >= 3,
        "expected several RES-derived sub-results, got {stems:?}"
    );
    // Pin names straight from the RES_0x4005_D rows (offline .prg data, safe to
    // assert) — one bitfield-expanded bit, the % scalar, a wheel speed, and the
    // table-mapped position: every row shape the walk decodes.
    for expected in [
        "STAT_WARNUNG_AKTIV",
        "STAT_NAEHERUNG_WARNGRENZE_S",
        "STAT_DSC_SIGNAL_VR",
        "STAT_DEFLATION_POSITON",
    ] {
        assert!(stems.contains(expected), "missing {expected} in {stems:?}");
    }
    // The full 8-byte payload decodes cleanly (no truncation error path), and
    // the position byte 0x02 resolves through TAB_DSC_RADPOSITION to its text.
    assert_eq!(
        results.get("JOB_STATUS"),
        Some(&ResultData::Text("OKAY".into()))
    );
    assert_eq!(
        results.get("STAT_DEFLATION_POSITON_TEXT"),
        Some(&ResultData::Text("vorn rechts".into()))
    );

    // The DISCRIMINATOR: the same stem filter on a single-value DDE read yields
    // exactly ONE stem, so a scalar read fails the `>= 3` bound above — the
    // multi-result assertion cannot be satisfied by read boilerplate.
    let dde_path = std::path::Path::new("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
    if dde_path.is_file() {
        let dde = Ecu::load(Prg::open(dde_path).unwrap());
        let one = DidExchange {
            did: 0x4517,
            raw: vec![0x0A, 0xBC],
        };
        let single = dde
            .run_job("STATUS_LESEN", 0x12, b"ARG;ITOEL", &one)
            .await
            .unwrap();
        assert_eq!(
            res_stems(&single).len(),
            1,
            "a single-scalar read must collapse to one stem"
        );
        // Task 10 _INFO regression on real data: the SG_FUNKTIONEN INFO cell
        // ("gefilterte Öltemperatur", `Ö` = CP1252 0xD6) must survive the
        // tabget -> write_string -> ergs -> read_string round trip intact. The
        // pre-fix UTF-8 write split `Ö` into two bytes that read back as mojibake
        // ("gefilterte Ã\u{96}ltemperatur").
        assert_eq!(
            single.get("STAT_MOTOROEL_TEMPERATUR_INFO"),
            Some(&ResultData::Text("gefilterte Öltemperatur".into()))
        );
    } else {
        eprintln!("skipping discriminator sub-proof: DDE BYO data not present");
    }
}
