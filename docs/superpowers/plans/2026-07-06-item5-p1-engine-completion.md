# M11 Item 5 / P1 — BEST/2 Engine Completion (offline) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete `klartext-best` so real, unmodified F20 jobs (`STATUS_LESEN`,
`STATUS_MOTORTEMPERATUR`, `STATUS_OELNIVEAU`, …) decode fully and run to `eoj` offline,
proven by a VM-vs-`measurement.rs` differential oracle and a structured multi-result test.

**Architecture:** Per spec `docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md`
§5: full-range job decode (bounded by the job directory, resumed past early `eoj`s), the
EDIABAS trap-state + 9 remaining opcodes, `Operand::Indexed` read/write on the S bank,
`Flow::Wait` handled async in the run loop, and `run_job` gaining the ECU `target` param.
No new crates; all work lands in `crates/sgbd` (one task) and `crates/best`.

**Tech Stack:** Rust edition 2024, tokio (time feature added to `klartext-best`), thiserror.
Reference semantics: a local checkout of `ediabaslib` (C#), `EdiabasLib/EdiabasLib/`
(files `EdiabasNet.cs`, `EdOperations.cs`).

## Global Constraints

- **License wall:** `ediabaslib` is GPL-licensed, `ediabasx` is PolyForm-NC; this repo is
  AGPL-3.0. Read them as a spec for FACTS (semantics, constants, layouts) and cite
  `file:line` in doc comments; NEVER copy code, names of local variables, or comment prose.
  Write your own test vectors.
- **BYO-data:** never commit `.prg`/DB bytes or anything from `data/`. Tests that need the
  real SGBDs must skip (print + return) when `data/Testmodule(1)/Ecu/<file>.prg` is absent.
  Hand-built byte fixtures in test code are fine.
- **No degrade-to-raw inside the VM** (spec §5): unknown opcode, decode fault, bounds fault
  outside the trap model, budget exhaustion = hard error carrying context.
- **No hardcoding** (owner rule): no per-job special cases; everything derives from the SGBD
  data. The one retired constant is `DEFAULT_TARGET` (Task 8).
- **Gates per task:** `cargo fmt` (run via Bash, not the editor hook), then
  `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test -p <crate>`.
  Check exit codes directly — never pipe a gate through `| tail` or mask it.
- **No ms-rust marker comments** (e.g. "// Rust guideline compliant") anywhere.
- Conventional commits; commit exactly the files each task names.
- Doc comments follow the existing crate voice (see `exec.rs`/`machine.rs`): explain the
  EDIABAS semantics being mirrored + cite the reference line, never narrate the Rust.

## Reference semantics pinned during planning (cite these in code)

| Fact | Source |
|---|---|
| Job directory record = `name[0x40]` + `u32 LE address`; **no size field**; ediabaslib executes lazily from the address | `EdiabasNet.cs:4936-4989` |
| Jump operands stored **post-instruction-relative signed**; converted to absolute at fetch | `EdiabasNet.cs:5819-5822` |
| S register = 1024-byte buffer + used length; `GetData(complete)` returns full buffer or used prefix | `EdiabasNet.cs:1527-1572, 2504` |
| `ArrayMaxSize` = `_arrayMaxBufSize - 1` = **1023** | `EdiabasNet.cs:2504, 2935-2941` |
| Indexed read: index from imm/reg (+imm increment for IdxRegImm); no-len → slice to end; len → exact len; `index+len > ArrayMaxSize` → `SetError(BIP_0001)` + empty | `EdiabasNet.cs:243-365` |
| Indexed write: value → LE bytes of caller `dataLen`, bytes → as-is; resize (zero-fill) to `index+len`; bounds → `SetError(BIP_0001)` + skip | `EdiabasNet.cs:470-556` |
| Byte-array → integer conversion is **little-endian** (byte 0 = LSB), zero-extended | `EdiabasNet.cs:376-405` |
| Trap state: `_errorTrapBitNr` (-1 = clear) + `_errorTrapMask`; `SetError` maps error→bit via dict (unmapped → 0), records bit, aborts only if `(1<<bit) & ~mask != 0` | `EdiabasNet.cs:4140-4166, 3180-3210` |
| Trap dict: `BIP_0002→2, BIP_0006→6, BIP_0009→9, BIP_0010→10, IFH_0001..→11..` (BIP_0001 unmapped → bit 0; IFH_0009 "no response" → 19) | `EdiabasNet.cs:3180-3210` |
| `jt`/`jnt`: with arg1 bit>0 → detected iff `trap == bit` or (`trap == 0` && `bit == 32`); arg1 == 0 or absent → detected iff `trap >= 0x40000000`; `jt` jumps when detected, `jnt` when not | `EdOperations.cs:1481-1519 (jnt), 1555-1600 (jt)` |
| `clrt` → trap = -1; `gettmr` → arg0 = trap **mask** + update Z/S flags; `settmr` → mask = arg0 (misleading names are EDIABAS's own) | `EdOperations.cs:412-415, 1279-1287, 2130-2133` |
| `wait` sleeps `arg0` **seconds** | `EdOperations.cs:3267-3270` |
| `swap` reverses `len` bytes at `start` of arg0's S buffer in place; bounds → `SetError(BIP_0001)`; register length unchanged (`keepLength=true`) | `EdOperations.cs:2406-2425` |
| `fix2dez`: len = counterpart width (non-value counterpart → 1); casts i8/i16/i32; decimal ASCII into arg0 string | `EdOperations.cs:687-715` |
| `fix2hex`: same len rule; formats `0x` + uppercase hex, width 2/4/8 | `EdOperations.cs:750-780` |
| `parw` shares `parl`'s handler (numeric arg parse; register write truncates) | `EdiabasNet.cs:2037-2038` |
| Args = one string split on `;`, 1-based indexing in `par*` | `EdiabasNet.cs:2727-2737`, `EdOperations.cs:1765-1860` |

---

### Task 1: Bound each job's bytecode in `Prg`

**Files:**
- Modify: `crates/sgbd/src/prg.rs` (`job_bytecode`, `parse_job_offsets` region, docs)
- Test: `crates/sgbd/src/prg.rs` (module tests, follow existing fixture builders)

**Interfaces:**
- Consumes: existing `job_offsets: Vec<(String, usize)>`, `deob: Vec<u8>`.
- Produces: `Prg::job_bytecode(&self, name: &str) -> Option<&[u8]>` — same signature,
  now returns a slice that **ends at the next job's start address** (address order, not
  directory order) or at end-of-file for the address-wise last job. Every later task
  relies on this bound.

The directory record carries no size (`EdiabasNet.cs:4936-4989` — name + address only),
so the bound is the next job's address. Directory order is not guaranteed to be address
order: compute the bound by sorting addresses.

- [ ] **Step 1: Write the failing tests** (inside the existing `#[cfg(test)] mod tests`,
  reusing the existing `build_prg_with_job_code`-style fixture helpers — extend the builder
  to accept two jobs if it cannot already):

```rust
#[test]
fn job_bytecode_is_bounded_by_the_next_job_address() {
    // Two jobs; FIRST's code is 3 bytes, SECOND starts right after.
    let bytes = build_prg_with_two_jobs("FIRST", &[0x1D, 0x00, 0x00], "SECOND", &[0x1D]);
    let prg = Prg::parse(&bytes).unwrap();
    assert_eq!(prg.job_bytecode("FIRST").unwrap().len(), 3);
    assert_eq!(prg.job_bytecode("SECOND"), Some(&[0x1D][..]));
}

#[test]
fn last_job_by_address_runs_to_end_of_file() {
    let bytes = build_prg_with_job_code("ONLY", &[0x1D]);
    let prg = Prg::parse(&bytes).unwrap();
    // The single job is the address-wise last: its slice ends at EOF, so it is
    // AT LEAST its own code; assert it starts correctly and is non-empty.
    let code = prg.job_bytecode("ONLY").unwrap();
    assert_eq!(code[0], 0x1D);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-sgbd job_bytecode_is_bounded -- --nocapture`
Expected: FAIL (today the slice runs to EOF, so FIRST's len > 3), or a missing-helper
compile error if the two-job fixture builder does not exist yet — build it first.

- [ ] **Step 3: Implement the bound**

In `job_bytecode`, replace the open slice with a bounded one:

```rust
pub fn job_bytecode(&self, name: &str) -> Option<&[u8]> {
    let (_, off) = self.job_offsets.iter().find(|(n, _)| n == name)?;
    // The directory records no size (EdiabasNet.cs ReadAllJobs: name + address
    // only), so a job's extent ends where the address-wise next job begins —
    // or at end-of-file for the last one.
    let end = self
        .job_offsets
        .iter()
        .map(|&(_, o)| o)
        .filter(|&o| o > *off)
        .min()
        .unwrap_or(self.deob.len());
    self.deob.get(*off..end)
}
```

- [ ] **Step 4: Run the crate tests**

Run: `cargo test -p klartext-sgbd`
Expected: PASS (all existing + 2 new).

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-sgbd
git add crates/sgbd/src/prg.rs
git commit -m "feat(sgbd): bound job_bytecode by the next job's directory address"
```

---

### Task 2: Full-range decode — `eoj` ends execution, not decoding

**Files:**
- Modify: `crates/best/src/decode.rs` (`decode_job`, module docs)
- Test: `crates/best/src/decode.rs` module tests

**Interfaces:**
- Consumes: Task 1's bounded slices.
- Produces: `decode_job(code: &[u8]) -> Result<Vec<Op>, DecodeError>` — same signature.
  New semantics: decodes the ENTIRE slice, continuing past `eoj`. A decode error at an
  offset **after at least one decoded `eoj`** truncates the result there (trailing
  padding/data after the final `eoj` is tolerated); a decode error **before any `eoj`**
  stays a hard error. The engine's existing `BadPc` remains the backstop if a jump
  targets an undecoded offset.

Rationale (record in the module docs): ediabaslib never pre-decodes — it decodes lazily at
the live PC (`EdiabasNet.cs:4936` region shows the abandoned eager scan) — so first-`eoj`
truncation was our artifact (spec §1). Real jobs branch past early-exit `eoj`s; the probe
decoded all 451 jobs of `d72n47a0`/`dsc_10`/`komb01`/`ihka20` cleanly to their bounds.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn decode_continues_past_an_early_eoj() {
    // eoj; move B0,#1 — the move lies past the first eoj and must decode.
    // move = 0x00, mode byte 0x85 = arg0 RegAb(8)/arg1 Imm8(5) per AddrMode.
    let code = [0x1D, 0x00, 0x00, 0x85, 0x00, 0x01];
    let ops = decode_job(&code).unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[1].offset, 2);
}

#[test]
fn garbage_after_the_final_eoj_is_tolerated() {
    // eoj, then a byte that is not a valid opcode (0xFF > 0xB7).
    let code = [0x1D, 0x00, 0xFF];
    let ops = decode_job(&code).unwrap();
    assert_eq!(ops.len(), 1); // just the eoj; the tail is padding
}

#[test]
fn garbage_with_no_eoj_is_still_a_hard_error() {
    let code = [0xFF, 0x00];
    assert!(matches!(
        decode_job(&code),
        Err(DecodeError::UnknownOpcode(0xFF))
    ));
}
```

Check the exact `move` encoding against the existing decode tests in the file (they
construct instructions the same way) and fix the fixture bytes if the mode nibbles
differ — the assertion structure stays.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best decode_continues_past -- --nocapture`
Expected: FAIL — today `decode_job` stops after the first `eoj` (`ops.len() == 1`).

- [ ] **Step 3: Implement**

In `decode_job`'s loop, remove the `break` after `eoj` and track it:

```rust
pub fn decode_job(code: &[u8]) -> Result<Vec<Op>, DecodeError> {
    let mut reader = Reader::new(code);
    let mut ops = Vec::new();
    let mut seen_eoj = false;
    loop {
        let offset = reader.pos;
        if offset >= code.len() {
            break;
        }
        // … existing per-instruction decode body unchanged, EXCEPT the error
        // paths: wrap each `return Err(e)` as
        //     if seen_eoj { break } else { return Err(e) }
        // (trailing non-code after the final eoj is padding; before any eoj it
        // is a real fault), and replace the post-eoj `break` with:
        if byte == EOJ {
            seen_eoj = true;
        }
    }
    Ok(ops)
}
```

Keep the existing per-instruction logic verbatim; only the loop-exit policy changes.
Update the module doc header (`//! … stops after the eoj`) to describe the new policy
and cite the spec §1 truncation finding.

- [ ] **Step 4: Run the crate tests**

Run: `cargo test -p klartext-best`
Expected: PASS. If an existing test asserted the old stop-at-eoj behavior, update its
name and assertion to the new policy (it is asserting the artifact).

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/decode.rs
git commit -m "feat(best): decode the full job range; eoj ends execution, not decoding"
```

---

### Task 3: Offline decode sweep over the real SGBDs (data-gated)

**Files:**
- Create: `crates/best/tests/decode_sweep.rs`
- Delete: `crates/best/examples/probe_res.rs` (the brainstorm's throwaway probe — this
  test supersedes it; spec §9 P3 note says delete once recorded, and it is recorded)

**Interfaces:**
- Consumes: `Prg::open`, `Prg::job_names`, `Prg::job_bytecode` (Task 1), `decode_job` (Task 2).
- Produces: nothing new — a regression gate later tasks rely on.

- [ ] **Step 1: Write the test**

```rust
//! Full-range decode sweep over the real F20 SGBDs (BYO data; skips if absent).

use klartext_best::decode_job;
use klartext_sgbd::Prg;

const ECUS: [&str; 4] = ["d72n47a0.prg", "dsc_10.prg", "komb01.prg", "ihka20.prg"];

#[test]
fn every_job_in_the_f20_sgbds_decodes_full_range() {
    let dir = std::path::Path::new("../../data/Testmodule(1)/Ecu");
    if !dir.is_dir() {
        eprintln!("skipping: BYO data dir not present");
        return;
    }
    let mut biggest = 0usize;
    for ecu in ECUS {
        let prg = Prg::open(dir.join(ecu)).expect("SGBD parses");
        for job in prg.job_names() {
            let code = prg.job_bytecode(job).expect("job has bytecode");
            let ops = decode_job(code)
                .unwrap_or_else(|e| panic!("{ecu}/{job} failed full-range decode: {e}"));
            biggest = biggest.max(ops.len());
        }
    }
    // The generic framework jobs are ~27k-47k ops; if the bound or the past-eoj
    // policy regressed to prefix-only decode, this ceiling collapses to ~2k.
    assert!(biggest > 20_000, "biggest job decoded only {biggest} ops");
}
```

- [ ] **Step 2: Run it (with data present)**

Run: `cargo test -p klartext-best --test decode_sweep -- --nocapture`
Expected: PASS with data present (all 451 jobs decode; the probe already proved this
holds); prints the skip line when data is absent.

- [ ] **Step 3: Delete the probe example** (it was never committed — untracked — so the
  removal needs no staging; if `crates/best/examples/` is empty afterwards, remove the dir)

```bash
rm crates/best/examples/probe_res.rs
rmdir crates/best/examples 2>/dev/null || true
```

- [ ] **Step 4: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/tests/decode_sweep.rs
git commit -m "test(best): full-range decode sweep over the real F20 SGBDs"
```

---

### Task 4: Trap state + `jt`/`jnt`/`clrt`/`gettmr`/`settmr`

**Files:**
- Modify: `crates/best/src/machine.rs` (two fields + doc)
- Modify: `crates/best/src/exec.rs` (5 dispatch arms + helpers + tests)

**Interfaces:**
- Consumes: `branch(m, mnemonic, op, taken)` helper (exec.rs:712), `update_zs`,
  `read_value_data`, `arg_width`.
- Produces (later tasks use these):
  - `Machine.trap_bit: Option<u32>` (None = EDIABAS `-1`), `Machine.trap_mask: u32`,
    both `pub(crate)`, zeroed/None in `Machine::new`.
  - `ExecError::Trapped { bit: u32 }` — raised when a recorded error is not masked.
  - `pub(crate) fn set_error(m: &mut Machine, bit: u32) -> Result<(), ExecError>` in
    `exec.rs`: records `m.trap_bit = Some(bit)`; returns `Err(ExecError::Trapped{bit})`
    iff `(1u64 << bit) & !u64::from(m.trap_mask) != 0`, else `Ok(())`.
    (Faithful to `SetError`, EdiabasNet.cs:4140-4166: the bit is recorded first; the mask
    only gates the abort. Trap-bit constants: BIP_0001 is unmapped → bit 0;
    IFH_0009 "no response" → 19 — define `pub(crate) const TRAP_BIT_UNMAPPED: u32 = 0;`
    and `pub(crate) const TRAP_BIT_NO_RESPONSE: u32 = 19;` with the dict citation.)

- [ ] **Step 1: Write the failing tests** (in exec.rs's test module, using the existing
  `step_bare`/`op(..)` helpers — read a neighboring trap test at exec.rs:2682 first):

```rust
#[test]
fn jt_jumps_only_when_the_tested_trap_bit_matches() {
    let mut m = Machine::new();
    // No trap set: jt +4 with test bit 5 falls through.
    let jt = op_jump_with_arg1(0x47, 4, 5); // build like the existing branch tests
    assert_eq!(step_bare(&mut m, &jt), Ok(Flow::Next));
    // Trap bit 5 set: it jumps.
    m.trap_bit = Some(5);
    let pc_before = m.pc;
    assert_eq!(step_bare(&mut m, &jt), Ok(Flow::Jumped));
    assert_eq!(m.pc, pc_before + jt.len + 4);
    // Special case (EdOperations.cs:1560-1570): trap==0 matches test bit 32.
    m.pc = 0;
    m.trap_bit = Some(0);
    let jt32 = op_jump_with_arg1(0x47, 4, 32);
    assert_eq!(step_bare(&mut m, &jt32), Ok(Flow::Jumped));
}

#[test]
fn jnt_is_the_complement_and_clrt_clears() {
    let mut m = Machine::new();
    let jnt = op_jump_with_arg1(0x48, 4, 5);
    assert_eq!(step_bare(&mut m, &jnt), Ok(Flow::Jumped)); // no trap → jumps
    m.trap_bit = Some(5);
    m.pc = 0;
    assert_eq!(step_bare(&mut m, &jnt), Ok(Flow::Next)); // trap matches → falls through
    // clrt (0x46) clears the trap.
    assert_eq!(
        step_bare(&mut m, &op(0x46, Operand::None, Operand::None)),
        Ok(Flow::Next)
    );
    assert_eq!(m.trap_bit, None);
}

#[test]
fn settmr_and_gettmr_move_the_trap_mask() {
    // EDIABAS's names are misleading: settmr/gettmr set/read the TRAP MASK
    // (EdOperations.cs:2130, 1279), not a clock.
    let mut m = Machine::new();
    let set = op(0x44, Operand::Imm(0b1010_0000), Operand::None); // Imm32 mode
    step_bare(&mut m, &set).unwrap();
    assert_eq!(m.trap_mask, 0b1010_0000);
    let get = op(
        0x43,
        Operand::Reg { bank: RegBank::L, idx: 0 },
        Operand::None,
    );
    step_bare(&mut m, &get).unwrap();
    assert_eq!(m.read(&get.arg0).unwrap(), Value::Int(0b1010_0000));
}

#[test]
fn set_error_respects_the_mask() {
    let mut m = Machine::new();
    m.trap_mask = 1 << 19; // job masked "no response"
    assert_eq!(set_error(&mut m, 19), Ok(()));
    assert_eq!(m.trap_bit, Some(19));
    // Unmasked bit aborts.
    assert_eq!(
        set_error(&mut m, 2),
        Err(ExecError::Trapped { bit: 2 })
    );
}
```

Build `op_jump_with_arg1` the way the existing conditional-branch tests build a branch
op with an `Imm` arg1 (grep `fn op(` and the `ja`/`jz` tests in exec.rs for the exact
constructor; arg1 uses an Imm8 mode nibble).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best jt_jumps -- --nocapture`
Expected: FAIL — `jt` (0x47) currently returns `Unimplemented("jt")` (exec.rs:2684
asserts exactly that; delete that assertion as part of this task).

- [ ] **Step 3: Implement**

`machine.rs`: add the two fields with docs + zero them in `new()`:

```rust
/// EDIABAS's error-trap bit (`_errorTrapBitNr`, EdiabasNet.cs:2506): `None`
/// mirrors `-1` (no error); comm/bounds faults record their dictionary bit here
/// and `jt`/`jnt` branch on it.
pub(crate) trap_bit: Option<u32>,
/// EDIABAS's `_errorTrapMask` (EdiabasNet.cs:2505): a set bit suppresses the
/// abort for that error class, letting the job handle it via `jt`.
pub(crate) trap_mask: u32,
```

`exec.rs`: the helper + five arms in `step`'s match (replace their current fall-through
to `Unimplemented`):

```rust
0x43 => {
    // gettmr (EdOperations.cs:1279): arg0 = the trap MASK; Z/S update.
    let len = arg_width("gettmr", &op.arg0)?;
    m.write(&op.arg0, Value::Int(i64::from(m.trap_mask)))
        .map_err(ExecError::from)?;
    update_zs(&mut m.flags, m.trap_mask, len);
    Ok(Flow::Next)
}
0x44 => {
    // settmr (EdOperations.cs:2130): mask = arg0.
    m.trap_mask = read_value_data(m, "settmr", &op.arg0)?;
    Ok(Flow::Next)
}
0x46 => {
    m.trap_bit = None; // clrt (EdOperations.cs:412)
    Ok(Flow::Next)
}
0x47 => branch(m, "jt", op, trap_detected(m, op)?),
0x48 => branch(m, "jnt", op, !trap_detected(m, op)?),
```

with:

```rust
/// The `jt`/`jnt` error-detected predicate (EdOperations.cs:1481-1600): a test
/// bit > 0 matches its exact trap bit — with `trap == 0` (an unmapped error)
/// also answering to the conventional test bit 32 — while a zero/absent test
/// bit asks "any unclassifiable error" (the reference compares against
/// 0x40000000, which no dictionary bit reaches; ported literally).
fn trap_detected(m: &Machine, op: &Op) -> Result<bool, ExecError> {
    let test_bit = match &op.arg1 {
        Operand::None => 0,
        arg1 => read_value_data(m, "jt", arg1)?,
    };
    Ok(match (m.trap_bit, test_bit) {
        (Some(trap), bit) if bit > 0 => trap == bit || (trap == 0 && bit == 32),
        (Some(trap), _) => trap >= 0x4000_0000,
        (None, _) => false,
    })
}

pub(crate) fn set_error(m: &mut Machine, bit: u32) -> Result<(), ExecError> {
    m.trap_bit = Some(bit);
    if (1u64 << bit) & !u64::from(m.trap_mask) != 0 {
        return Err(ExecError::Trapped { bit });
    }
    Ok(())
}
```

Add `ExecError::Trapped { bit: u32 }` with message
`"an ECU/VM error raised trap bit {bit} and the job does not mask it"`.
Note `read_value_data`'s exact name/signature is at exec.rs:1224 — reuse it; if it
rejects `Imm` operands, use the same value-read the existing `settmr`-like ops use
(check `op_push`'s `value_data_len` path).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-best`
Expected: PASS (new tests + the updated formerly-`Unimplemented` assertions).

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/machine.rs crates/best/src/exec.rs
git commit -m "feat(best): EDIABAS trap state + jt/jnt/clrt/gettmr/settmr"
```

---

### Task 5: `Operand::Indexed` reads

**Files:**
- Modify: `crates/best/src/machine.rs` (the `read` Indexed arm + a resolve helper)
- Modify: `crates/best/src/exec.rs` (route indexed sources through the int/bytes/string
  read helpers; `set_error` on bounds)

**Interfaces:**
- Consumes: Task 4's `set_error`, `TRAP_BIT_UNMAPPED`.
- Produces:
  - `pub(crate) const ARRAY_MAX_SIZE: usize = 1023;` in `machine.rs`
    (EDIABAS `ArrayMaxSize` = `_arrayMaxBufSize(1024) - 1`, EdiabasNet.cs:2504/2935).
  - `Machine::resolve_index(&self, arg: &IndexArg) -> Result<usize, MachineError>` —
    an `Imm` yields its value; a `Reg` yields that register's value.
  - `Machine::read(&Operand::Indexed{..})` returns `Value::Bytes` per the table below.
  - Bounds faults do NOT hard-error at the machine layer: `read` returns a new
    `MachineError::IndexOutOfBounds` and the **exec** layer converts it to
    `set_error(m, TRAP_BIT_UNMAPPED)` + an empty `Value::Bytes` (faithful to
    `SetError(BIP_0001)` + `ByteArray0`, EdiabasNet.cs:283-289).

Semantics (base register MUST be the `S` bank — any other bank is
`MachineError::Unsupported`, loud; the F20 jobs only index S):

| Mode | index | result |
|---|---|---|
| `Indexed{index, len: None}` (IdxImm/IdxReg) | imm or reg | `used[index..]`; `index >= used.len()` → empty; `index + 1 > 1023` → bounds fault |
| IdxRegImm (decoder puts the increment in `len`… **verify**: read decode.rs's `Indexed` docs — if the increment landed in `len`, distinguish by the mode nibble via `Op.mode_byte`) | reg + imm increment | as above with the summed index |
| `Indexed{index, len: Some(l)}` (Idx*Len*) | imm/reg | exactly `l` bytes starting at `index`, **zero-extended** past the used length; `index + l > 1023` → bounds fault |

Record this doc note verbatim on the `read` arm (it is a deliberate, argued deviation):

```text
EDIABAS's no-len indexed read slices the COMPLETE 1024-byte backing buffer
(GetData(true), EdiabasNet.cs:1550-1559), so its tail is zeros beyond the used
length. We slice the used bytes only: every prefix consumer (little-endian
value reads, NUL-terminated string reads) behaves identically, and a job that
stored the zero-tail through a plain register write would poison later xsend
requests with a 1 KiB buffer. The len-variant zero-extends exactly like the
reference. If a capture ever shows a job depending on the zero-tail of a
no-len slice, revisit (spec §10).
```

- [ ] **Step 1: Write the failing tests** (machine.rs test module):

```rust
#[test]
fn indexed_read_slices_the_used_bytes_to_the_end() {
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![0xAA, 0xBB, 0xCC, 0xDD])).unwrap();
    let op = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Imm(1),
        len: None,
    };
    assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![0xBB, 0xCC, 0xDD]));
}

#[test]
fn indexed_read_past_the_used_length_is_empty() {
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![0xAA])).unwrap();
    let op = Operand::Indexed { base: s_reg_id(1), index: IndexArg::Imm(5), len: None };
    assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![]));
}

#[test]
fn indexed_len_read_zero_extends_like_the_reference_buffer() {
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![0xAA, 0xBB])).unwrap();
    let op = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Imm(1),
        len: Some(IndexArg::Imm(4)),
    };
    assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![0xBB, 0, 0, 0]));
}

#[test]
fn indexed_read_index_comes_from_a_register() {
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![1, 2, 3])).unwrap();
    m.write(&b_reg(0), Value::Int(2)).unwrap();
    let op = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Reg(b_reg_id(0)),
        len: None,
    };
    assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![3]));
}

#[test]
fn indexed_read_beyond_array_max_is_a_bounds_fault() {
    let m = Machine::new();
    let op = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Imm(1024),
        len: None,
    };
    assert!(matches!(m.read(&op), Err(MachineError::IndexOutOfBounds { .. })));
}
```

Write tiny local helpers `s_reg(idx)`, `s_reg_id(idx)`, `b_reg(idx)`, `b_reg_id(idx)`
returning the crate's real `Operand::Reg{..}`/`RegId` values (check `RegId`'s
constructor in decode.rs — it is exported from lib.rs).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best indexed_read -- --nocapture`
Expected: FAIL with `Unsupported("indexed operand access is not yet implemented")`.

- [ ] **Step 3: Implement** — replace the `read` Indexed arm:

```rust
Operand::Indexed { base, index, len } => {
    let data = match base.bank() {
        RegBank::S => self.s.get(usize::from(base.idx())).ok_or(MachineError::OutOfRange {
            bank: RegBank::S,
            idx: base.idx(),
        })?,
        other => return Err(MachineError::Unsupported(format!(
            "indexed access on a {other:?} register is not part of the executed subset"
        ))),
    };
    let idx = self.resolve_index(index)?;
    match len {
        None => {
            if idx + 1 > ARRAY_MAX_SIZE {
                return Err(MachineError::IndexOutOfBounds { index: idx, len: 0 });
            }
            Ok(Value::Bytes(data.get(idx..).map(<[u8]>::to_vec).unwrap_or_default()))
        }
        Some(l) => {
            let l = self.resolve_index(l)?;
            if idx + l > ARRAY_MAX_SIZE {
                return Err(MachineError::IndexOutOfBounds { index: idx, len: l });
            }
            let mut out = vec![0u8; l];
            if idx < data.len() {
                let take = (data.len() - idx).min(l);
                out[..take].copy_from_slice(&data[idx..idx + take]);
            }
            Ok(Value::Bytes(out))
        }
    }
}
```

Adjust to the crate's actual `RegId` accessor names (grep `impl RegId`). If the decoder
stores IdxRegImm's increment in the `len` slot (its doc comment says "length … or
increment"), the read arm must treat that mode's third operand as `index += increment`
with a to-end slice, NOT a length — key off the operand's addressing-mode nibbles in
`Op.mode_byte` (mode values: consult `AddrMode` in decode.rs). Write one extra test for
the increment mode once the encoding is confirmed from `AddrMode`'s docs.

In `exec.rs`, wire the faults: where `read_int`/`read_bytes`/`read_string` receive an
operand, an `Err(MachineError::IndexOutOfBounds{..})` from `m.read` becomes
`set_error(m, TRAP_BIT_UNMAPPED)?` and an empty-bytes value (mirror `SetError` +
`ByteArray0`). Integer reads over `Value::Bytes` already zero-extend little-endian —
verify `read_int`'s byte path matches `GetValueData` (EdiabasNet.cs:376-405: LSB first)
and add one regression test if uncovered:

```rust
#[test]
fn indexed_source_reads_little_endian_into_ints() {
    // move I0, S1[0,2] with S1 = [0x34, 0x12] → I0 = 0x1234.
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-best`
Expected: PASS.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/machine.rs crates/best/src/exec.rs
git commit -m "feat(best): Operand::Indexed reads (S-bank slicing, LE, zero-extend)"
```

---

### Task 6: `Operand::Indexed` writes + `swap`

**Files:**
- Modify: `crates/best/src/machine.rs` (`write_indexed`, the `write` Indexed arm)
- Modify: `crates/best/src/exec.rs` (`op_move`'s indexed-dest path, `swap` arm 0x51)

**Interfaces:**
- Consumes: Task 5's `resolve_index`, `ARRAY_MAX_SIZE`, `set_error`.
- Produces:
  - `Machine::write_indexed(&mut self, op: &Operand, value: &Value, len: usize) -> Result<(), MachineError>`
    — `Value::Int` serializes to `len` little-endian bytes; `Value::Bytes` writes all its
    bytes (`len` ignored); grows the used data (zero-filling any gap) to `index + n`;
    `index + n > ARRAY_MAX_SIZE` → `MachineError::IndexOutOfBounds` (exec converts to
    `set_error` + skip, per EdiabasNet.cs:536-541).
  - `Machine::write(&Operand::Indexed{..}, Value::Bytes(_))` delegates with the bytes'
    own length; an Int through plain `write` uses `len = 1` (the reference's
    `SetRawData(data)` default, EdiabasNet.cs:441-444). Ops that know a width
    (`move` via `GetArgsValueLength`) call `write_indexed` explicitly.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn indexed_write_extends_and_zero_fills() {
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![0xAA])).unwrap();
    let dest = Operand::Indexed { base: s_reg_id(1), index: IndexArg::Imm(3), len: None };
    m.write_indexed(&dest, &Value::Int(0x1234), 2).unwrap();
    // Gap zero-filled, value little-endian (EdiabasNet.cs:520-529).
    assert_eq!(
        m.read(&s_reg(1)).unwrap(),
        Value::Bytes(vec![0xAA, 0x00, 0x00, 0x34, 0x12])
    );
}

#[test]
fn indexed_write_of_bytes_uses_their_own_length() {
    let mut m = Machine::new();
    let dest = Operand::Indexed { base: s_reg_id(1), index: IndexArg::Imm(0), len: None };
    m.write_indexed(&dest, &Value::Bytes(vec![1, 2, 3]), 1).unwrap();
    assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![1, 2, 3]));
}

#[test]
fn swap_reverses_the_addressed_slice_in_place() {
    // swap S1[1,3] on [1,2,3,4,5] → [1,4,3,2,5]; length unchanged
    // (EdOperations.cs:2406-2425, keepLength=true).
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![1, 2, 3, 4, 5])).unwrap();
    let arg0 = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Imm(1),
        len: Some(IndexArg::Imm(3)),
    };
    step_bare(&mut m, &op_with(0x51, arg0, Operand::None)).unwrap();
    assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![1, 4, 3, 2, 5]));
}

#[test]
fn swap_past_the_used_length_pulls_in_zeros_but_keeps_length() {
    // used = [1,2]; swap [1,3) touches zeros from the backing buffer; the
    // register's used length stays 2 (keepLength=true), so only byte 1 changes.
    let mut m = Machine::new();
    m.write(&s_reg(1), Value::Bytes(vec![1, 2])).unwrap();
    let arg0 = Operand::Indexed {
        base: s_reg_id(1),
        index: IndexArg::Imm(1),
        len: Some(IndexArg::Imm(3)),
    };
    step_bare(&mut m, &op_with(0x51, arg0, Operand::None)).unwrap();
    // Reversal of [2,0,0] is [0,0,2]; first `used` bytes survive → [1, 0].
    assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![1, 0]));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best indexed_write -- --nocapture`
Expected: FAIL (`write_indexed` undefined / swap `Unimplemented`).

- [ ] **Step 3: Implement**

`machine.rs`:

```rust
pub(crate) fn write_indexed(
    &mut self,
    op: &Operand,
    value: &Value,
    len: usize,
) -> Result<(), MachineError> {
    let Operand::Indexed { base, index, len: _ } = op else {
        return Err(MachineError::Unsupported("write_indexed on a non-indexed operand".into()));
    };
    // (bank check + resolve_index as in Task 5's read)
    let bytes: Vec<u8> = match value {
        Value::Int(v) => (0..len).map(|i| (*v >> (i * 8)) as u8).collect(),
        Value::Bytes(b) => b.clone(),
        Value::Float(_) => {
            return Err(MachineError::Unsupported("cannot write a float through an index".into()));
        }
    };
    let idx = self.resolve_index(index)?;
    if idx + bytes.len() > ARRAY_MAX_SIZE {
        return Err(MachineError::IndexOutOfBounds { index: idx, len: bytes.len() });
    }
    if base.bank() != RegBank::S {
        return Err(MachineError::Unsupported(format!(
            "indexed write on a {:?} register is not part of the executed subset",
            base.bank()
        )));
    }
    let data = self.s.get_mut(usize::from(base.idx())).ok_or(MachineError::OutOfRange {
        bank: RegBank::S,
        idx: base.idx(),
    })?;
    if data.len() < idx + bytes.len() {
        data.resize(idx + bytes.len(), 0);
    }
    data[idx..idx + bytes.len()].copy_from_slice(&bytes);
    Ok(())
}
```

`exec.rs`: in `op_move` (and any op that writes through its dest generically), when the
dest is `Operand::Indexed` and the value is an Int, call
`m.write_indexed(&op.arg0, &value, len as usize)` with the op's computed width
(`GetArgsValueLength` analog — the existing `arg_width`/counterpart logic `op_move`
already has); Bytes go through plain `write`. The `swap` arm:

```rust
0x51 => {
    // swap (EdOperations.cs:2406): byte-reverse the addressed slice in place;
    // the register's used length is preserved (keepLength=true) — zeros from
    // the backing buffer participate when the slice overruns the used bytes.
    let Operand::Indexed { base, index, len: Some(len_arg) } = &op.arg0 else {
        return Err(ExecError::InvalidOperand("swap"));
    };
    let start = m.resolve_index(index).map_err(ExecError::from)?;
    let len = m.resolve_index(len_arg).map_err(ExecError::from)?;
    if start + len > ARRAY_MAX_SIZE {
        set_error(m, TRAP_BIT_UNMAPPED)?;
        return Ok(Flow::Next);
    }
    m.swap_s_slice(base, start, len).map_err(ExecError::from)?;
    Ok(Flow::Next)
}
```

with `Machine::swap_s_slice(&mut self, base: &RegId, start: usize, len: usize)`:
extend a copy of the used data with zeros to `start+len` if short, reverse
`[start, start+len)`, truncate back to the original used length, store.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-best`
Expected: PASS.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/machine.rs crates/best/src/exec.rs
git commit -m "feat(best): Operand::Indexed writes + swap"
```

---

### Task 7: `wait` as an async `Flow` + `fix2hex`/`fix2dez`

**Files:**
- Modify: `crates/best/src/exec.rs` (`Flow::Wait`, arms 0x6B/0x79/0x7A)
- Modify: `crates/best/src/engine.rs` (handle `Flow::Wait`)
- Modify: `crates/best/Cargo.toml` — via `cargo add tokio -p klartext-best --features time`
  (then verify the manifest line uses `workspace = true` inheritance; if cargo-add wrote a
  concrete version instead, rewrite that one line to
  `tokio = { workspace = true, features = ["time"] }` — switching to workspace inheritance
  is manifest *configuration*, not version-pinning, so the hand-edit is allowed).

**Interfaces:**
- Consumes: `read_value_data`, `write_string` (exec.rs:880).
- Produces: `Flow::Wait { seconds: u32 }` — the run loop sleeps
  `tokio::time::sleep(Duration::from_secs(seconds.into()))` and continues. Later phases
  (P2 flows) rely on this variant existing.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn wait_surfaces_seconds_to_the_run_loop() {
    // wait #2 → Flow::Wait{seconds: 2}; the sleep itself happens at the async
    // boundary (EdOperations.cs:3267 sleeps arg0 × 1000 ms synchronously).
    let mut m = Machine::new();
    let w = op(0x6B, Operand::Imm(2), Operand::None);
    assert_eq!(step_bare(&mut m, &w), Ok(Flow::Wait { seconds: 2 }));
}

#[test]
fn fix2dez_formats_signed_by_counterpart_width() {
    // (EdOperations.cs:687-715): len 1 casts i8 → 0xFF prints "-1".
    let mut m = Machine::new();
    m.write(&b_reg(0), Value::Int(0xFF)).unwrap();
    let op7a = op_with(0x7A, s_reg(1), b_reg(0));
    step_bare(&mut m, &op7a).unwrap();
    assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(b"-1".to_vec()));
}

#[test]
fn fix2hex_formats_prefixed_uppercase_by_width() {
    // (EdOperations.cs:750-780): len 2 → "0x%04X".
    let mut m = Machine::new();
    m.write(&i_reg(0), Value::Int(0x0ABC)).unwrap();
    let op79 = op_with(0x79, s_reg(1), i_reg(0));
    step_bare(&mut m, &op79).unwrap();
    assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(b"0x0ABC".to_vec()));
}
```

(`step_bare` is the existing no-context step helper in exec.rs's tests; if `write_string`
requires the ctx-taking variant the neighboring `flt2a` tests use, follow those tests'
helper exactly. Check whether `write_string` NUL-terminates and mirror the `flt2a`
tests' expected bytes, byte-exactly.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best wait_surfaces -- --nocapture`
Expected: FAIL (`Unimplemented("wait")`; `Flow::Wait` doesn't exist → compile error
first — add the variant, then the arms).

- [ ] **Step 3: Implement**

```rust
// Flow addition:
/// A `wait` (0x6B) requested a sleep: the run loop awaits `seconds` and
/// resumes at the already-advanced PC. Kept out of `step` so the executor
/// never blocks (the reference sleeps inline, EdOperations.cs:3267).
Wait { seconds: u32 },

// exec arms:
0x6B => Ok(Flow::Wait { seconds: read_value_data(m, "wait", &op.arg0)? }),
0x79 => op_fix2(m, "fix2hex", op),
0x7A => op_fix2(m, "fix2dez", op),
```

```rust
/// `fix2hex`/`fix2dez` (EdOperations.cs:750/687): format arg1 — signed decimal
/// or 0x-prefixed uppercase hex — at its width (a non-value counterpart
/// formats one byte) into string register arg0.
fn op_fix2(m: &mut Machine, mnemonic: &'static str, op: &Op) -> Result<Flow, ExecError> {
    let len = data_len(m, mnemonic, &op.arg1).unwrap_or(1);
    let raw = read_int(m, mnemonic, &op.arg1, len)?;
    let text = match (mnemonic, len) {
        ("fix2dez", 1) => format!("{}", raw as u8 as i8),
        ("fix2dez", 2) => format!("{}", raw as u16 as i16),
        ("fix2dez", 4) => format!("{}", raw as i32),
        ("fix2hex", 1) => format!("0x{raw:02X}"),
        ("fix2hex", 2) => format!("0x{raw:04X}"),
        ("fix2hex", 4) => format!("0x{raw:08X}"),
        _ => return Err(ExecError::InvalidOperand(mnemonic)),
    };
    write_string(m, &op.arg0, &text)?;
    Ok(Flow::Next)
}
```

(`data_len` is exec.rs:1241 — the reference's `GetDataLen` analog; its "non-value → 1"
fallback mirrors `arg1.GetDataType() != typeof(EdValueType) ? 1 : GetDataLen()`.)

`engine.rs` run loop — new match arm:

```rust
Flow::Wait { seconds } => {
    tokio::time::sleep(std::time::Duration::from_secs(seconds.into())).await;
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-best`
Expected: PASS.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/Cargo.toml Cargo.lock crates/best/src/exec.rs crates/best/src/engine.rs
git commit -m "feat(best): wait as async Flow::Wait + fix2hex/fix2dez"
```

---

### Task 8: Engine — `target` param, budget, exchange-error → trap

**Files:**
- Modify: `crates/best/src/engine.rs`
- Modify: `crates/best/tests/oracle.rs` (call-site updates only, keep assertions)

**Interfaces:**
- Consumes: everything above.
- Produces (P2 binaries + Tasks 9-10 rely on these exactly):
  - `Ecu::run_job(&self, name: &str, target: u8, args: &[u8], exchange: &dyn UdsExchange) -> Result<ResultSet, RunError>`
    — `DEFAULT_TARGET` const deleted; the caller supplies the ECU address (spec §5:
    never hardcode).
  - `const MAX_INSTRUCTIONS: usize = 2_000_000;` — justification in the doc comment: the
    largest F20 job is ~47k static ops and the generic jobs loop over up to ~1,800
    `SG_FUNKTIONEN` rows with tens of ops per row (~10⁵–10⁶ executed); 2M is an order
    above the worst legitimate case and still fails a runaway loop in well under a second.
  - Exchange faults no longer abort the run: an `Err` from `exchange.request` records
    the reference's "no response" trap (`set_error(m, TRAP_BIT_NO_RESPONSE)`, bit 19 —
    EdiabasNet.cs:3194) and writes an **empty** response into `dest`, letting the job's
    own `jt` error path produce its `JOB_STATUS` text. If the job does not mask bit 19,
    `set_error` aborts with `ExecError::Trapped` → `RunError::Exec` (faithful:
    unmasked errors abort the job).

- [ ] **Step 1: Write the failing tests** (engine tests live where the existing
  `run_job` unit tests are — grep `mod tests` in engine.rs):

The existing non-ignored end-to-end test in `tests/oracle.rs` hand-assembles a job
(`move`→`xsend`→…→`ergr`→`eoj`) with in-file encoding helpers — reuse those helpers
(move them into a shared `tests/common/mod.rs` if the engine unit tests cannot reach
them). The two test jobs, as instruction sequences:

- masked: `settmr #Imm32(1<<19)` · `move S1, #Str[0x22,0x10,0x01]` · `xsend S1, S1` ·
  `jt +<offset-of-ergs>, #Imm8(19)` · `eoj` · (error path:) `ergs #Str"JOB_STATUS", #Str"FAIL"` · `eoj`
- unmasked: the same sequence without the leading `settmr`.

```rust
#[tokio::test]
async fn exchange_error_traps_instead_of_aborting_when_masked() {
    // With an empty MockExchange every request errors → trap bit 19 → jt takes
    // the error path, which emits JOB_STATUS = FAIL and ends.
    let ecu = ecu_with_masked_trap_job(); // assembles the sequence above
    let results = ecu.run_job("T", 0x12, &[], &MockExchange::new()).await.unwrap();
    assert_eq!(results.get("JOB_STATUS"), Some(&ResultData::Text("FAIL".into())));
}

#[tokio::test]
async fn exchange_error_aborts_when_unmasked() {
    // Same job without the settmr: bit 19 unmasked → the run aborts, and the
    // error carries the ORIGINAL transport fault, not just "trapped".
    let ecu = ecu_with_unmasked_trap_job();
    let err = ecu.run_job("T", 0x12, &[], &MockExchange::new()).await.unwrap_err();
    assert!(matches!(err, RunError::Exchange(_)));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best exchange_error -- --nocapture`
Expected: compile FAIL on the new `target` param → make the signature change first,
then behavioral FAIL (today an exchange error returns `RunError::Exchange`).

- [ ] **Step 3: Implement** — in `run_job`:

```rust
Flow::Exchange { request, dest } => {
    match exchange.request(target, &request).await {
        Ok(response) => {
            m.write(&dest, Value::Bytes(response)).map_err(ExecError::from)?;
        }
        Err(e) => {
            // The reference records IFH_0009 ("no response", trap bit 19,
            // EdiabasNet.cs:3194) and lets the job's jt path handle it. When
            // the job does NOT mask bit 19, the run aborts — returning the
            // original transport error, which names the actual fault.
            if set_error(&mut m, TRAP_BIT_NO_RESPONSE).is_err() {
                return Err(RunError::Exchange(e));
            }
            m.write(&dest, Value::Bytes(Vec::new())).map_err(ExecError::from)?;
        }
    }
}
```

(`Flow::Wait` was handled in Task 7; `RunError::Exchange` stays — it is now the
unmasked-abort path and carries the transport fault.)
Update `MAX_INSTRUCTIONS` with the justified value + doc. Delete `DEFAULT_TARGET`.
Update every `run_job` call site (tests + `tests/oracle.rs`) to pass `0x12` explicitly.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-best`
Expected: PASS.

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/engine.rs crates/best/tests/oracle.rs
git commit -m "feat(best): run_job target param, 2M budget, exchange faults trap as IFH_0009"
```

---

### Task 9: Run the real jobs to `eoj` (oracle tripwire flipped)

**Files:**
- Modify: `crates/best/tests/oracle.rs`

**Interfaces:**
- Consumes: the whole engine; `data/Testmodule(1)/Ecu/d72n47a0.prg` (BYO, skip-if-absent).
- Produces: a `RecordingExchange` test helper other tests may copy (keep it in this file).

- [ ] **Step 1: Replace the `#[ignore]`d truncation tripwire** with live assertions.
  The old test asserted the *truncated* behavior (spec §1 corrected it); it now runs
  un-ignored and asserts the real jobs complete:

```rust
/// Answers every request with a positive UDS echo (SID+0x40 + echo + payload
/// zeros) and records what the job transmitted. Offline stand-in: the real
/// response layouts are `[verify against capture]` (car session 1).
struct RecordingExchange(std::sync::Mutex<Vec<Vec<u8>>>);

#[async_trait::async_trait]
impl UdsExchange for RecordingExchange {
    async fn request(&self, _t: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        self.0.lock().unwrap().push(uds.to_vec());
        let mut resp = vec![uds[0] | 0x40];
        resp.extend_from_slice(&uds[1..uds.len().min(3)]);
        resp.extend_from_slice(&[0u8; 8]);
        Ok(resp)
    }
}

#[tokio::test]
async fn real_status_motortemperatur_runs_to_eoj() {
    let Some(ecu) = open_dde() else { return }; // existing skip-if-absent helper
    let rec = RecordingExchange(Default::default());
    let results = ecu
        .run_job("STATUS_MOTORTEMPERATUR", 0x12, &[], &rec)
        .await
        .expect("full-range decode + indexed + tail run the real job");
    // The job always emits a status; layout-dependent values stay unasserted
    // until the capture (spec §9, car session 1).
    assert!(results.get("JOB_STATUS").is_some());
    assert!(!rec.0.lock().unwrap().is_empty(), "the job transmitted at least once");
}

#[tokio::test]
async fn real_status_oelniveau_runs_to_eoj() {
    let Some(ecu) = open_dde() else { return };
    let rec = RecordingExchange(Default::default());
    let results = ecu.run_job("STATUS_OELNIVEAU", 0x12, &[], &rec).await.unwrap();
    assert!(results.get("JOB_STATUS").is_some());
}
```

Expectation management: the zero-filled responses may drive the jobs down their error
paths (`JOB_STATUS` = an error text) — that is FINE and still proves decode + indexed +
tail + trap all execute 1,000+ real ops. Do NOT assert specific status strings. If a
job instead fails on a genuinely unimplemented opcode, the error names it — implement
it only if it is in the Task 4-7 set (a miss there is a task bug); otherwise STOP and
report (the opcode tail was measured from these exact jobs, so this should not happen).

- [ ] **Step 2: Run**

Run: `cargo test -p klartext-best --test oracle -- --nocapture`
Expected: PASS with data present (both jobs reach `eoj`); skip lines without data.

- [ ] **Step 3: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/tests/oracle.rs
git commit -m "test(best): real DDE jobs run to eoj — truncation tripwire flipped live"
```

---

### Task 10: The differential oracle + structured multi-result proof

**Files:**
- Create: `crates/best/tests/differential.rs`
- Modify: `crates/best/Cargo.toml` — from the workspace root:
  `cargo add --dev -p klartext-best --path crates/semantic`
  (adds `klartext-semantic` as a dev-dependency; acyclic — semantic depends only on sgbd)

**Interfaces:**
- Consumes: `Ecu::run_job` (Task 8 signature), `klartext_semantic::measurement::Measurements::{from_prg, get}`,
  `Measurement::scaled(raw) -> Option<ScaledMeasurement>`, `ResultSet::iter_current`.
- Produces: the milestone's acceptance tests (spec §8.3-4).

- [ ] **Step 1: Write the differential test** (spec §8.3 — VM vs Rust scaler on the SAME
  synthetic bytes; disagreement stops the line):

```rust
//! Differential oracle: the VM running the DDE's generic STATUS_LESEN must
//! agree with klartext-semantic's SG_FUNKTIONEN scaler on identical response
//! bytes (spec §8.3). BYO data; skips when absent.

use klartext_best::{Ecu, ExchangeError, ResultData, UdsExchange};
use klartext_semantic::measurement::Measurements;
use klartext_sgbd::Prg;

/// DDE measurements sampled across widths/scalings (id, ARG name) — M9-verified
/// names: oil temp, coolant temp, engine speed.
const SAMPLES: [(u16, &str); 3] = [(0x4517, "ITOEL"), (0x461B, "ITKUM"), (0x427F, "Nkw")];

struct DidExchange {
    did: u16,
    raw: Vec<u8>,
}

#[async_trait::async_trait]
impl UdsExchange for DidExchange {
    async fn request(&self, _t: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // Answer a 22-read of our DID (or the 2C-define + F3xx read pair the
        // generic job may build) with the canned raw value; echo per UDS.
        match uds {
            [0x22, hi, lo] if u16::from_be_bytes([*hi, *lo]) == self.did => {
                let mut r = vec![0x62, *hi, *lo];
                r.extend_from_slice(&self.raw);
                Ok(r)
            }
            [0x2C, 0x03, ..] => Ok(vec![0x6C, 0x03, uds[2], uds[3]]),
            [0x2C, 0x01, id_hi, id_lo, ..] => Ok(vec![0x6C, 0x01, *id_hi, *id_lo]),
            [0x22, f3_hi @ 0xF3, lo] => {
                let mut r = vec![0x62, *f3_hi, *lo];
                r.extend_from_slice(&self.raw);
                Ok(r)
            }
            other => Err(ExchangeError::Unexpected(other.to_vec())),
        }
    }
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
        let m = measurements.get(id).unwrap_or_else(|| panic!("{arg} in SG_FUNKTIONEN"));
        // Probe the row's width by trying the plausible raw sizes against the
        // Rust scaler (Measurement::scaled returns None on a width mismatch):
        let (raw, rust) = [1usize, 2, 4]
            .into_iter()
            .find_map(|w| {
                let raw: Vec<u8> = [0x0A, 0xBC, 0x01, 0x02][..w].to_vec();
                m.scaled(&raw).map(|s| (raw, s))
            })
            .unwrap_or_else(|| panic!("{arg}: no raw width scales in measurement.rs"));

        let exchange = DidExchange { did: id, raw: raw.clone() };
        let results = ecu
            .run_job("STATUS_LESEN", 0x12, format!("{arg}").as_bytes(), &exchange)
            .await
            .unwrap_or_else(|e| panic!("STATUS_LESEN({arg}) failed: {e}"));

        // The job emits STAT_<name>_WERT (or the row's RESULTNAME); find the
        // single Real result and compare within epsilon.
        let vm_value = results
            .iter_current()
            .find_map(|(n, v)| match v {
                ResultData::Real(f) if n.contains("WERT") || n == m.name() => Some(*f),
                _ => None,
            })
            .unwrap_or_else(|| panic!("STATUS_LESEN({arg}) emitted no scaled value: {results:?}"));
        assert!(
            (vm_value - rust.value).abs() < 1e-6,
            "{arg}: VM {vm_value} != Rust {}",
            rust.value
        );
    }
}
```

(`rust.value`: use `ScaledMeasurement`'s actual field name — see its definition at
`crates/semantic/src/measurement.rs:109` — the plan assumes `value`.)

**Exploration note (this is the one place the plan authorizes iteration):** the exact
request shapes `STATUS_LESEN` builds and the exact result names it emits are what this
test DISCOVERS. First run it with the `DidExchange` match arms above; when a request
doesn't match, the `Unexpected(bytes)` error PRINTS the real request — extend the match
arms to answer it (they are all read-class frames: `22`/`2C` per `SG_FUNKTIONEN
SERVICE=22;2C`). Same for result names: on mismatch, panic-print `results` and pin the
real names into the assertion. Freeze the final vectors in the test; add a comment
citing what the job actually sent. Do NOT weaken the value-equality assertion — if VM
and Rust disagree numerically, STOP and report (one of the two is wrong; spec §8.3).

- [ ] **Step 2: Write the structured multi-result proof** (spec §8.4, owner requirement
  (a)) in the same file:

```rust
#[tokio::test]
async fn vm_status_lesen_decodes_a_multi_row_res_table_on_the_dsc() {
    let path = std::path::Path::new("../../data/Testmodule(1)/Ecu/dsc_10.prg");
    if !path.is_file() {
        eprintln!("skipping: BYO data not present");
        return;
    }
    // dsc_10 SG_FUNKTIONEN row 0x4005 → RES_0X4005_D: 8 sub-results incl.
    // BITFIELDs and a % scalar (probe finding, spec §1). Look the ARG name up
    // from the table rather than hardcoding it.
    let prg = Prg::open(path).unwrap();
    let sgf = prg.table_ci("SG_FUNKTIONEN").unwrap();
    let arg_col = sgf.columns.iter().position(|c| c == "ARG").unwrap();
    let id_col = sgf.columns.iter().position(|c| c == "ID").unwrap();
    let row = sgf.rows.iter().find(|r| r[id_col].eq_ignore_ascii_case("0x4005")).unwrap();
    let arg = row[arg_col].clone();

    let exchange = DidExchange { did: 0x4005, raw: vec![0xFF, 0x42] };
    let ecu = Ecu::load(prg);
    let results = ecu
        .run_job("STATUS_LESEN", 0x29, arg.as_bytes(), &exchange)
        .await
        .unwrap();

    // Requirement (a): MULTIPLE named sub-results from one response.
    let named: Vec<&str> = results.iter_current().map(|(n, _)| n).collect();
    assert!(
        named.len() >= 3,
        "expected several named sub-results, got {named:?}"
    );
}
```

(Same exploration rule: extend `DidExchange` arms guided by `Unexpected` prints; the
DSC job may route via plain `22` since its SERVICE column says `31`/`22` per row —
answer what it actually sends. If `STATUS_LESEN` on the DSC needs a different target
byte, it is `0x29` per the ECU map — the test already passes it.)

- [ ] **Step 3: Run**

Run: `cargo test -p klartext-best --test differential -- --nocapture`
Expected: PASS with data (values agree; ≥3 named results); skips without.

- [ ] **Step 4: Full workspace gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git add crates/best/tests/differential.rs crates/best/Cargo.toml Cargo.lock
git commit -m "test(best): differential oracle vs measurement.rs + DSC multi-result proof"
```

---

## Self-review notes (run after all tasks)

- Spec §5 coverage: full-range decode (T1-3), Indexed (T5-6), opcode tail — `jt/jnt`,
  timers-as-trap-mask, `clrt` (T4), `wait`, `fix2hex/fix2dez` (T7), `swap` (T6);
  `enewset/etag/ergl/y42flt/y82flt/jpl` were already implemented in Phase 1 (verified
  exec.rs:239-299 during planning — the spec's "~15" was pre-verification); budget +
  target + args (T8); §8.2 sweep (T3), §8.6 run-to-eoj (T9), §8.3-4 oracles (T10).
  §8.1 per-opcode vectors are embedded in each op task. §8.5/§8.7 (gate tests,
  LERNWERTE rehearsal) are P2/P3 scope per spec §9 — NOT in this plan.
- After Task 10 passes with real data: update the memory file
  (`best2-vm-milestone.md`) — P1 done, oracle verdicts — and note any `DidExchange`
  vectors worth carrying into the P2 capture checklist.
