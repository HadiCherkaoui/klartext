//! Engine oracle: self-contained `run_job` proofs + the real F20 DDE jobs.
//!
//! No BMW data is committed here. The self-contained proofs hand-assemble tiny
//! BEST/2 jobs and drive them through [`Ecu::run_job`] with a [`MockExchange`],
//! proving the run loop, the `Flow::Exchange` async boundary, the error-trap
//! path, and result emission end to end — and reproducing `klartext-semantic`'s
//! engine-temperature scaling (`raw × 0.1 − 273.14`) in the VM.
//!
//! The real-job oracle then runs the actual, unmodified F20 DDE
//! `STATUS_MOTORTEMPERATUR` and `STATUS_OELNIVEAU` jobs off BYO data (skipped
//! when that gitignored data is absent) through a [`RecordingExchange`], proving
//! that full-range decode + indexed addressing + the trap/opcode tail carry each
//! real job all the way to `eoj`. The Phase-1 "raw-only" reading was a
//! first-`eoj` truncation artifact (spec §1); these jobs run their whole bodies.

use klartext_best::{
    Ecu, ExchangeError, MockExchange, ResultData, ResultSet, RunError, UdsExchange, decode_job,
};

// ---- a tiny test-only BEST/2 assembler (emits [opcode][mode][operands]) ----

/// Addressing-mode nibbles (mirror `AddrMode`'s on-disk numbers).
const M_NONE: u8 = 0;
const M_REG: u8 = 2; // any register-mode nibble reads a selector byte, which
// itself carries the bank, so one "register" nibble suffices for B/I/L/S/F.
const M_IMM8: u8 = 5;
const M_IMM32: u8 = 7;
const M_IMMSTR: u8 = 8;

/// Register selector bytes (`register()` in the decoder resolves these).
fn s(i: u8) -> u8 {
    0x1C + i // S0..S7
}
fn ireg(i: u8) -> u8 {
    0x10 + i // I0..I7
}
fn f(i: u8) -> u8 {
    0x24 + i // F0..F7
}

fn mode(hi: u8, lo: u8) -> u8 {
    (hi << 4) | lo
}

/// Emit an ImmStr operand: u16 little-endian length, then the bytes.
fn immstr(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).expect("string fits u16");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

/// `move S<d>, {bytes}` — build a byte-buffer literal into an S register.
fn move_s_lit(out: &mut Vec<u8>, d: u8, bytes: &[u8]) {
    out.push(0x00);
    out.push(mode(M_REG, M_IMMSTR));
    out.push(s(d));
    immstr(out, bytes);
}

/// `xsend S<resp>, S<req>` — transmit the request; response into `S<resp>`.
fn xsend(out: &mut Vec<u8>, resp: u8, req: u8) {
    out.push(0x2A);
    out.push(mode(M_REG, M_REG));
    out.push(s(resp));
    out.push(s(req));
}

/// `move I<d>, S<src>` — read `I`'s width (2 bytes, little-endian) out of an S reg.
fn move_i_from_s(out: &mut Vec<u8>, d: u8, src: u8) {
    out.push(0x00);
    out.push(mode(M_REG, M_REG));
    out.push(ireg(d));
    out.push(s(src));
}

/// `fix2flt F<d>, I<src>` — integer register to float.
fn fix2flt(out: &mut Vec<u8>, d: u8, src: u8) {
    out.push(0x68);
    out.push(mode(M_REG, M_REG));
    out.push(f(d));
    out.push(ireg(src));
}

/// `a2flt F<d>, {text}` — parse a numeric string literal into a float register.
fn a2flt_lit(out: &mut Vec<u8>, d: u8, text: &str) {
    out.push(0x3A);
    out.push(mode(M_REG, M_IMMSTR));
    out.push(f(d));
    immstr(out, text.as_bytes());
}

/// `fmul F<d>, F<src>` / `fadd F<d>, F<src>`.
fn fmul(out: &mut Vec<u8>, d: u8, src: u8) {
    out.push(0x3D);
    out.push(mode(M_REG, M_REG));
    out.push(f(d));
    out.push(f(src));
}
fn fadd(out: &mut Vec<u8>, d: u8, src: u8) {
    out.push(0x3B);
    out.push(mode(M_REG, M_REG));
    out.push(f(d));
    out.push(f(src));
}

/// `ergr "{name}", F<src>` — emit a real result.
fn ergr(out: &mut Vec<u8>, name: &str, src: u8) {
    out.push(0x38);
    out.push(mode(M_IMMSTR, M_REG));
    immstr(out, name.as_bytes());
    out.push(f(src));
}

/// `eoj`.
fn eoj(out: &mut Vec<u8>) {
    out.push(0x1D);
    out.push(mode(M_NONE, M_NONE));
}

/// `settmr #Imm32(mask)` — set the error-trap MASK (opcode 0x44). A set bit
/// suppresses the hard abort for that error class so the job handles it via `jt`.
fn settmr_imm32(out: &mut Vec<u8>, mask: u32) {
    out.push(0x44);
    out.push(mode(M_IMM32, M_NONE));
    out.extend_from_slice(&mask.to_le_bytes());
}

/// `jt +rel, #Imm8(bit)` — branch by `rel` when trap `bit` is recorded (opcode
/// 0x47). `arg0` is the Imm32 displacement (added to the PC already past the
/// `jt`), `arg1` the Imm8 test bit.
fn jt_imm8(out: &mut Vec<u8>, rel: i32, bit: u8) {
    out.push(0x47);
    out.push(mode(M_IMM32, M_IMM8));
    out.extend_from_slice(&rel.to_le_bytes());
    out.push(bit);
}

/// `ergs "{name}", "{text}"` — emit a text result (opcode 0x39).
fn ergs_lit(out: &mut Vec<u8>, name: &str, text: &str) {
    out.push(0x39);
    out.push(mode(M_IMMSTR, M_IMMSTR));
    immstr(out, name.as_bytes());
    immstr(out, text.as_bytes());
}

/// Wrap hand-assembled bytecode as a one-job SGBD `Prg` (no BMW data).
///
/// Mirrors the container layout `klartext-sgbd`'s parser reads back: a plaintext
/// header (magic + job-directory pointer), then an XOR-`0xF7` body of a one-entry
/// job directory whose offset field points at `code`.
fn prg_with_one_job(name: &str, code: &[u8]) -> klartext_sgbd::Prg {
    const DATA_OFFSET: usize = 0xA0;
    const OFFSET_JOB_DIR: usize = 0x88;
    const JOB_ENTRY_SIZE: usize = 0x44;
    const NAME_FIELD_LEN: usize = 64;
    const XOR_KEY: u8 = 0xF7;

    let mut header = vec![0u8; DATA_OFFSET];
    header[..b"@EDIABAS OBJECT".len()].copy_from_slice(b"@EDIABAS OBJECT");
    header[0x10..0x14].copy_from_slice(&1u32.to_le_bytes()); // file type: variant
    header[OFFSET_JOB_DIR..OFFSET_JOB_DIR + 4]
        .copy_from_slice(&u32::try_from(DATA_OFFSET).unwrap().to_le_bytes());

    let code_offset = DATA_OFFSET + 4 + JOB_ENTRY_SIZE;
    let mut body = Vec::new();
    body.extend_from_slice(&1u32.to_le_bytes()); // job count
    let mut entry = vec![0u8; JOB_ENTRY_SIZE];
    entry[..name.len()].copy_from_slice(name.as_bytes());
    entry[NAME_FIELD_LEN..NAME_FIELD_LEN + 4]
        .copy_from_slice(&u32::try_from(code_offset).unwrap().to_le_bytes());
    body.extend_from_slice(&entry);
    body.extend_from_slice(code);
    for b in &mut body {
        *b ^= XOR_KEY;
    }
    header.extend_from_slice(&body);
    klartext_sgbd::Prg::parse(&header).expect("valid synthetic SGBD")
}

/// End-to-end proof: a hand-assembled job runs THROUGH `run_job`, exchanges a
/// request via the mock, reads the response, scales it, and emits a real result.
///
/// The job reproduces the engine-temperature transform (`raw × 0.1 − 273.14`):
/// it sends a request, the mock returns the raw deci-Kelvin word little-endian
/// (`2F 0E` = 3631), the job reads it into `I0`, floats it, multiplies by `0.1`,
/// adds `−273.14`, and emits `89.96`. This is a SYNTHETIC job (clearly not the
/// real DDE bytecode); it proves the VM's loop + `Flow::Exchange` + `ResultSet`
/// path and that the VM can reproduce `klartext-semantic`'s scaling math.
#[tokio::test]
async fn run_job_drives_hand_assembled_scaling_job() {
    let mut code = Vec::new();
    move_s_lit(&mut code, 1, &[0x22, 0xF3, 0x03]); // build request in S1
    xsend(&mut code, 4, 1); // xsend S4, S1 -> response into S4
    move_i_from_s(&mut code, 0, 4); // I0 = first word of S4 (LE) = 0x0E2F = 3631
    fix2flt(&mut code, 0, 0); // F0 = 3631.0
    a2flt_lit(&mut code, 1, "0.1"); // F1 = 0.1
    fmul(&mut code, 0, 1); // F0 = 363.1
    a2flt_lit(&mut code, 2, "-273.14"); // F2 = -273.14
    fadd(&mut code, 0, 2); // F0 = 89.96
    ergr(&mut code, "TEMP_WERT", 0); // emit real result
    eoj(&mut code);

    let ecu = Ecu::load(prg_with_one_job("SCALE_DEMO", &code));
    let mut mock = MockExchange::new();
    // Response raw word little-endian so the whole-register read yields 3631.
    mock.on(vec![0x22, 0xF3, 0x03], vec![0x2F, 0x0E]);

    let rs = ecu
        .run_job("SCALE_DEMO", 0x12, &[], &mock)
        .await
        .expect("runs");
    match rs.get("TEMP_WERT") {
        Some(ResultData::Real(v)) => assert!((v - 89.96).abs() < 0.01, "got {v}"),
        other => panic!("expected Real(89.96), got {other:?}"),
    }
}

// ---- the exchange-fault trap path: masked recovers, unmasked aborts ----

/// Assembles the trap-path proof job and wraps it as a one-job SGBD named `"T"`.
///
/// The job builds a request in `S1`, `xsend`s it, then `jt`s on trap bit 19 (the
/// "no response" class) to an error handler that emits `JOB_STATUS = "FAIL"`.
/// With `mask_no_response`, a leading `settmr` masks bit 19 so a failed exchange
/// is caught by the job's own `jt`; without it, the unmasked trap aborts the run.
fn trap_job(mask_no_response: bool) -> Ecu {
    // The `jt` jumps over the normal-path eoj ([0x1D, 0x00] = 2 bytes) to the
    // error handler; a taken branch adds this to the PC already past the `jt`.
    const EOJ_LEN: i32 = 2;
    let mut code = Vec::new();
    if mask_no_response {
        settmr_imm32(&mut code, 1 << 19); // mask the "no response" trap class
    }
    move_s_lit(&mut code, 1, &[0x22, 0x10, 0x01]); // build the request in S1
    xsend(&mut code, 1, 1); // xsend S1, S1 — the empty mock errors this exchange
    jt_imm8(&mut code, EOJ_LEN, 19); // on trap bit 19, jump to the error handler
    eoj(&mut code); // normal-path end (skipped when the trap fires)
    ergs_lit(&mut code, "JOB_STATUS", "FAIL"); // error handler
    eoj(&mut code);
    Ecu::load(prg_with_one_job("T", &code))
}

/// A failed exchange the job MASKS (bit 19) records the "no response" trap and
/// lets the job's own `jt` path run — emitting `JOB_STATUS = FAIL` — instead of
/// aborting the run.
#[tokio::test]
async fn exchange_error_traps_instead_of_aborting_when_masked() {
    // With an empty MockExchange every request errors → trap bit 19 → jt takes
    // the error path, which emits JOB_STATUS = FAIL and ends.
    let ecu = trap_job(true);
    let results = ecu
        .run_job("T", 0x12, &[], &MockExchange::new())
        .await
        .unwrap();
    assert_eq!(
        results.get("JOB_STATUS"),
        Some(&ResultData::Text("FAIL".into()))
    );
}

/// A failed exchange the job does NOT mask aborts the run, and the error carries
/// the ORIGINAL transport fault (`RunError::Exchange`), not just a generic trap.
#[tokio::test]
async fn exchange_error_aborts_when_unmasked() {
    // Same job without the settmr: bit 19 unmasked → the run aborts, and the
    // error carries the original transport fault, not just "trapped".
    let ecu = trap_job(false);
    let err = ecu
        .run_job("T", 0x12, &[], &MockExchange::new())
        .await
        .unwrap_err();
    assert!(matches!(err, RunError::Exchange(_)));
}

// ---- the engine oracle: run the REAL F20 DDE jobs to `eoj` off BYO data ----

/// Path to the real F20 DDE SGBD (BYO data; gitignored, never committed).
const REAL_DDE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/Testmodule(1)/Ecu/d72n47a0.prg"
);

/// The DDE's ECU address on the F20 gateway (`0x12`).
///
/// The caller's knowledge, not the bytecode's — [`Ecu::run_job`] takes the
/// target per call (spec §5 retired the hardcoded default). The real response
/// layouts keyed on it are `[verify against capture]` (car session 1).
const DDE_TARGET: u8 = 0x12;

/// Opens the real F20 DDE SGBD as a runnable [`Ecu`], or `None` when absent.
///
/// The BYO data is gitignored, so on a machine without it the tests skip rather
/// than fail; on this repo's data it loads for real.
fn open_dde() -> Option<Ecu> {
    Ecu::open(REAL_DDE).ok()
}

/// A [`UdsExchange`] that echoes each request and records what was transmitted.
///
/// Answers every request with a positive UDS echo (`SID + 0x40`, the echoed
/// service/DID, then a zero payload) and pushes the sent bytes into its log.
/// Offline stand-in for the car: the real response layouts are
/// `[verify against capture]` (car session 1, spec §9), so this cannot assert
/// scaled values — only that a job transmits and runs its decode to `eoj`. A
/// zero payload may steer a job down its (masked) error path; that still
/// exercises full-range decode, indexed addressing, and the trap/opcode tail.
struct RecordingExchange(std::sync::Mutex<Vec<Vec<u8>>>);

#[async_trait::async_trait]
impl UdsExchange for RecordingExchange {
    async fn request(&self, _target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        self.0.lock().unwrap().push(uds.to_vec());
        let mut resp = vec![uds[0] | 0x40];
        resp.extend_from_slice(&uds[1..uds.len().min(3)]);
        resp.extend_from_slice(&[0u8; 8]);
        Ok(resp)
    }
}

/// The static decoded op count of `job` in the real DDE, or `None` if unreadable.
fn static_op_count(job: &str) -> Option<usize> {
    let prg = klartext_sgbd::Prg::open(REAL_DDE).ok()?;
    let code = prg.job_bytecode(job)?;
    decode_job(code).ok().map(|ops| ops.len())
}

/// Prints run evidence (decoded ops, transmitted requests, `JOB_STATUS`).
///
/// libtest captures this on a passing `cargo test`, so a green gate stays
/// pristine; it surfaces only under `--nocapture` or on failure.
fn report_evidence(job: &str, rec: &RecordingExchange, results: &ResultSet) {
    let sent = rec.0.lock().unwrap();
    let ops = static_op_count(job).map_or_else(|| "unknown".to_owned(), |n| n.to_string());
    eprintln!(
        "[{job}] decoded ops: {ops}; exchanges transmitted: {}",
        sent.len()
    );
    for (i, req) in sent.iter().enumerate() {
        eprintln!("  request #{i}: {req:02X?}");
    }
    match results.get("JOB_STATUS") {
        Some(status) => eprintln!("  JOB_STATUS = {status:?}"),
        None => eprintln!("  JOB_STATUS absent"),
    }
}

/// The real `STATUS_MOTORTEMPERATUR` bytecode runs full-range to `eoj`.
///
/// Supersedes the Phase-1 `#[ignore]`d truncation tripwire: with indexed
/// addressing and the opcode tail landed (Tasks 5–7), the real job no longer
/// blocks on `Operand::Indexed`. It runs its whole body and emits `JOB_STATUS`;
/// the recorded exchange proves it ran past arg-validation into its telegram
/// code. Layout-derived values stay unasserted until the capture (spec §9).
#[tokio::test]
async fn real_status_motortemperatur_runs_to_eoj() {
    let Some(ecu) = open_dde() else {
        return; // BYO data absent: skip (never a failure) on other machines.
    };
    let rec = RecordingExchange(Default::default());
    let results = ecu
        .run_job("STATUS_MOTORTEMPERATUR", DDE_TARGET, &[], &rec)
        .await
        .expect("full-range decode + indexed + trap + tail carry the real job to eoj");
    report_evidence("STATUS_MOTORTEMPERATUR", &rec, &results);
    // The job always emits a status; a zero-filled response may make it an error
    // text, so the value stays unasserted (spec §9, car session 1).
    assert!(results.get("JOB_STATUS").is_some());
    assert!(
        !rec.0.lock().unwrap().is_empty(),
        "the job ran past arg-validation into its telegram exchange"
    );
}

/// The real `STATUS_OELNIVEAU` bytecode (an Item-5 target job) runs to `eoj`.
///
/// A larger generic-framework job than `STATUS_MOTORTEMPERATUR` (spec §1); it
/// likewise runs full-range and emits `JOB_STATUS`. Its scaled oil-level values
/// stay unasserted until the on-car capture (spec §9).
#[tokio::test]
async fn real_status_oelniveau_runs_to_eoj() {
    let Some(ecu) = open_dde() else {
        return;
    };
    let rec = RecordingExchange(Default::default());
    let results = ecu
        .run_job("STATUS_OELNIVEAU", DDE_TARGET, &[], &rec)
        .await
        .expect("full-range decode + indexed + trap + tail carry the real job to eoj");
    report_evidence("STATUS_OELNIVEAU", &rec, &results);
    assert!(results.get("JOB_STATUS").is_some());
}
