//! Offline engine oracle + a self-contained end-to-end proof of `run_job`.
//!
//! No BMW data is committed here. The non-ignored proof hand-assembles a tiny
//! BEST/2 job and drives it through [`Ecu::run_job`] with a [`MockExchange`],
//! proving the run loop, the `Flow::Exchange` async boundary, and result
//! emission end to end — and reproducing `klartext-semantic`'s engine-temperature
//! scaling (`raw × 0.1 − 273.14`) in the VM. The ignored oracle runs the REAL
//! F20 DDE `STATUS_MOTORTEMPERATUR` job off BYO data.

use klartext_best::{Ecu, MockExchange, Operand, ResultData, RunError, decode_job};

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

// ---- the ignored engine oracle: run the REAL F20 DDE job off BYO data ----

const REAL_DDE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../data/Testmodule(1)/Ecu/d72n47a0.prg"
);

/// True for the result-store opcodes (`ergb`..`ergl`).
fn is_erg(byte: u8) -> bool {
    matches!(byte, 0x34..=0x39 | 0x3F | 0x81 | 0x82)
}

/// The NUL-trimmed ASCII text of an `ImmStr` operand (an `erg*` result name).
fn immstr_text(op: &Operand) -> Option<String> {
    match op {
        Operand::Str(b) => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            Some(b[..end].iter().map(|&c| char::from(c)).collect())
        }
        _ => None,
    }
}

/// The engine oracle, run against the REAL F20 DDE `STATUS_MOTORTEMPERATUR` job.
///
/// # What running the real job revealed (2026-07-05)
/// The plan assumed this call reproduces `klartext-semantic`'s 89.96 °C by running
/// the DDE's *bytecode* scaling. Running the real `d72n47a0.prg` job disproves that
/// premise, and this test pins the truthful state instead of a fabricated pass:
///
/// * `STATUS_MOTORTEMPERATUR` emits ONLY `_REQUEST_1`/`_RESPONSE_1` (the raw
///   telegrams, via `ergy`) and `JOB_STATUS`/`JOB_MESSAGE` (a status text looked up
///   in the `JobResult` table). It contains no `fmul`/`fadd`/`ergr` and never emits
///   `STAT_MOTORTEMPERATUR_WERT` — it returns the RAW response.
/// * No job in this SGBD hardcodes a `*MOTORTEMPERATUR*` result name (asserted in
///   part 1): per the M6 finding (`docs/sgbd-findings.md` §5), the scaled value and
///   its result name come from the `SG_FUNKTIONEN` table, applied in Rust by
///   `klartext-semantic` (`measurement::real_dde_scales_motor_temperature` verifies
///   `0x0E2F → 89.96`). The 89.96 is the table scaler's output, not this job's.
/// * The `run_job` HARNESS runs the real bytecode correctly until it reaches a
///   genuinely-unimplemented addressing mode: indexed `S`-register access
///   ([`Operand::Indexed`], the deferred Task 10/11 feature the job uses to slice
///   the response telegram). Reaching `eoj` additionally needs
///   `gettmr`/`settmr`/`clrt`/`wait`/`fix2hex` + the `jt` error-trap — a scoped
///   follow-up, none of which would make this job produce 89.96.
///
/// This test therefore asserts (1) the raw-only structural finding and (2) that the
/// harness executes the real bytecode up to the deferred addressing mode. It will
/// trip once indexed addressing lands, which is the right moment to revisit it.
#[tokio::test]
#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
async fn engine_temperature_matches_measurement_rs() {
    let Ok(prg) = klartext_sgbd::Prg::open(REAL_DDE) else {
        return;
    };

    // (1) Structural finding: no job hardcodes a MOTORTEMPERATUR result name — the
    // scaled value's name is read from SG_FUNKTIONEN at runtime, so scaling is
    // table-driven (measurement.rs), not per-job bytecode.
    for name in prg.job_names() {
        let Some(code) = prg.job_bytecode(name) else {
            continue;
        };
        let Ok(ops) = decode_job(code) else {
            continue;
        };
        for op in &ops {
            if is_erg(op.byte)
                && let Some(text) = immstr_text(&op.arg0)
            {
                assert!(
                    !text.contains("MOTORTEMPERATUR"),
                    "unexpected hardcoded result name {text:?} in job {name}"
                );
            }
        }
    }

    // (2) The harness runs the real bytecode until the deferred indexed-addressing
    // mode. Execution blocks (at `move B0, S0[Imm(0)]`) BEFORE the first `xsend`,
    // so no exchange is consulted — an empty mock is correct here.
    let ecu = Ecu::load(prg);
    let mock = MockExchange::new();
    let err = ecu
        .run_job("STATUS_MOTORTEMPERATUR", 0x12, &[], &mock)
        .await
        .expect_err("real job blocks on the deferred indexed-addressing mode");
    let msg = err.to_string();
    assert!(
        msg.contains("indexed operand access"),
        "expected the indexed-addressing blocker, got: {msg}"
    );
}
