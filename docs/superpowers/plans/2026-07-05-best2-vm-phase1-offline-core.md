# BEST/2 VM — Phase 1 (offline core) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the offline core of a BEST/2 bytecode VM (`klartext-best`) that executes a real EDIABAS *read* job end-to-end against a mock transport and reproduces `measurement.rs`'s engine scaling — no car, no live I/O, no writes.

**Architecture:** A new library crate `crates/best` decodes a job's BEST/2 bytecode (`klartext-sgbd` supplies the bytes) into an instruction list, runs it on a register/stack machine, and routes the job's request/response comm through a `UdsExchange` trait whose Phase-1 impl is an in-memory mock. The measurement path is the VM running the generic `tabset SG_FUNKTIONEN → tabseek → tabget → RES_ walk → scale` idiom. Success is gated by an oracle: the VM's result for the engine's `STATUS_MOTORTEMPERATUR` must equal `measurement.rs` (89.96 °C from raw `0E 2F`).

**Tech Stack:** Rust edition 2024, tokio (async only at the `run_job`/exchange boundary), thiserror. Depends on `klartext-sgbd` (container/tables/job-bytecode) and `klartext-uds` (SID constants). References studied for facts only: `ediabaslib` (`EdiabasNet.cs`), `ediabasx`.

## Global Constraints

- **Rust edition 2024, latest stable.** Add crate via `cargo new --lib`; add deps via `cargo add` — never hand-edit Cargo.toml versions.
- **Workspace layout:** library crate under `crates/`; package name `klartext-best`; share versions via `[workspace.dependencies]` / `[workspace.package]`.
- **Errors:** `thiserror` in this library; never `anyhow` here.
- **License / no-copy (hard):** klartext is AGPL-3.0. `ediabaslib` is **GPLv3**, `ediabasx` is **PolyForm Noncommercial 1.0.0**. Read them for **facts only** (opcode byte assignments, addressing-mode encoding, machine semantics — BMW's format, not copyrightable expression). **Copy no code and no test vectors.** Derive every test vector ourselves (first-principles math for arithmetic/float; real `.prg` bytes for decode; hand-built synthetic tables for the measurement path).
- **BYO-data:** never commit `.prg`/table content; real-data tests are `#[ignore]`d and read from `data/Testmodule(1)/Ecu/…` supplied by the user.
- **No degrade-to-raw INSIDE the VM:** an unknown/unimplemented opcode, a malformed decode, or a failed table lookup is a hard `Err` — never a silent guess. (Degrade-to-raw stays in the semantic layer that *calls* the VM, later.)
- **Phase boundary:** Phase 1 implements only the compute/table/result/param opcodes **plus the single request→response comm bridge** (`xsend`/`xrequf` → `UdsExchange`) needed to drive a read job against the mock. The full `x*` comm family, transport/timing opcodes, the live `Session` backend, and the write-gate are **Phase 2/3 — out of scope here.** Unimplemented opcodes fail loud.
- `cargo fmt` (via Bash — the Edit hook uses an older rustfmt) and `cargo clippy -- -D warnings` clean before the phase is done.
- Conventional commits; commit after each task.

---

### Task 1: Scaffold the `klartext-best` crate

**Files:**
- Create: `crates/best/Cargo.toml`, `crates/best/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Produces: the crate `klartext-best` compiling in the workspace; `pub use` surface stubs (`Ecu`, `RunError`) filled in later tasks.

- [ ] **Step 1: Create the crate and wire the workspace**

Run:
```bash
cargo new --lib crates/best --name klartext-best
```
Then add it to the workspace `members` in the root `Cargo.toml` (edit the existing `members = [...]` array to include `"crates/best"`), and add dependencies via CLI:
```bash
cargo add -p klartext-best thiserror --workspace
cargo add -p klartext-best klartext-sgbd klartext-uds --path-workspace 2>/dev/null || \
  cargo add -p klartext-best --path crates/sgbd --path crates/uds
```
(If the workspace already declares these under `[workspace.dependencies]`, add them with `cargo add -p klartext-best <name> --workspace` and set `klartext-sgbd = { workspace = true }` accordingly — match how `crates/semantic/Cargo.toml` references `klartext-sgbd`.)

- [ ] **Step 2: Write the crate doc + a smoke test in `lib.rs`**

```rust
//! BEST/2 bytecode VM + EDIABAS job engine for klartext (offline Phase 1).
//!
//! Decodes and interprets a BMW BEST/2 job to execute one named EDIABAS job:
//! build the UDS request(s), exchange them (Phase 1: a mock), and parse the
//! response into named, scaled results. See
//! `docs/superpowers/specs/2026-07-05-best2-vm-job-engine-design.md`.

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
```

- [ ] **Step 3: Verify it builds and tests pass**

Run: `cargo test -p klartext-best`
Expected: PASS (1 test).

- [ ] **Step 4: fmt + commit**

```bash
cargo fmt -p klartext-best
git add crates/best Cargo.toml Cargo.lock
git commit -m "feat(best): scaffold klartext-best crate"
```

---

### Task 2: Expose job bytecode + case-insensitive table lookup from `klartext-sgbd`

**Files:**
- Modify: `crates/sgbd/src/prg.rs` (retain the deobfuscated buffer + per-job offsets; add accessors)
- Modify: `crates/sgbd/src/lib.rs` (no API change, re-exports already cover `Prg`)
- Test: in `crates/sgbd/src/prg.rs` `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `Prg::job_bytecode(&self, name: &str) -> Option<&[u8]>` — the deobfuscated bytecode slice from the job's entry offset to end-of-body (the decoder stops at the `eoj` opcode).
  - `Prg::table_ci(&self, name: &str) -> Option<&Table>` — case-insensitive table lookup (`RES_0x5001` ref resolves `RES_0X5001` stored; see the `sgbd-table-name-casing` finding).

- [ ] **Step 1: Write failing tests**

Add to `crates/sgbd/src/prg.rs` tests (reuse the existing `build_prg_with_jobs` helper; extend it to write real bytecode bytes after each job-name field — see Step 3):

```rust
#[test]
fn job_bytecode_returns_slice_from_entry_offset() {
    // A job whose bytecode is the single byte 0x1D (eoj).
    let bytes = build_prg_with_job_code("STATUS_X", &[0x1D]);
    let prg = Prg::parse(&bytes).unwrap();
    assert_eq!(prg.job_bytecode("STATUS_X"), Some(&[0x1D][..]));
    assert_eq!(prg.job_bytecode("NOPE"), None);
}

#[test]
fn table_ci_resolves_case_insensitively() {
    let bytes = build_prg(&[Tbl { name: "RES_0X5001", columns: &["A"], rows: &[&["1"]] }]);
    let prg = Prg::parse(&bytes).unwrap();
    assert!(prg.table("RES_0x5001").is_none()); // exact: no
    assert!(prg.table_ci("RES_0x5001").is_some()); // case-insensitive: yes
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-sgbd job_bytecode table_ci`
Expected: FAIL (methods/helper not defined).

- [ ] **Step 3: Implement**

In `Prg`, retain the deobfuscated buffer and per-job offsets. Change the struct and `parse`:

```rust
pub struct Prg {
    tables: Vec<Table>,
    jobs: Vec<String>,
    deob: Vec<u8>,          // full buffer, header plaintext + body de-XORed
    job_offsets: Vec<(String, usize)>, // (name, absolute bytecode offset)
}
```

In `parse`, build the deobfuscated buffer once and record each job's bytecode offset. The job directory entry already carries a `u32` bytecode offset right after the 64-byte name field (`ENTRY_CELL_PTR` is table-only; for jobs the offset is at name-field end). Read it de-obfuscated:

```rust
fn parse_job_offsets(bytes: &[u8]) -> Vec<(String, usize)> {
    let Some((entries_start, count)) = read_directory(bytes, OFFSET_JOB_DIR) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for i in 0..count as usize {
        let entry = entries_start + i * JOB_ENTRY_SIZE;
        if entry + JOB_ENTRY_SIZE > bytes.len() { break; }
        let (name, _) = read_string(bytes, entry, NAME_FIELD_LEN);
        let off = deobf_u32(bytes, entry + NAME_FIELD_LEN).unwrap_or(0) as usize;
        if !name.is_empty() { out.push((name, off)); }
    }
    out
}

fn deobf_buffer(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().enumerate()
        .map(|(i, &b)| deobfuscate(b, i))
        .collect()
}
```

Wire them in `parse` (store `deob: deobf_buffer(bytes)`, `job_offsets: parse_job_offsets(bytes)`), and add the accessors:

```rust
pub fn job_bytecode(&self, name: &str) -> Option<&[u8]> {
    let (_, off) = self.job_offsets.iter().find(|(n, _)| n == name)?;
    self.deob.get(*off..)
}

pub fn table_ci(&self, name: &str) -> Option<&Table> {
    self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}
```

Add the test helper `build_prg_with_job_code(name, code)` that lays out one job whose bytecode-offset field points at `code` bytes appended after the job directory (XOR-`0xF7` the whole body as the existing helper does).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p klartext-sgbd`
Expected: PASS (all existing + 2 new).

- [ ] **Step 5: fmt + commit**

```bash
cargo fmt -p klartext-sgbd
git add crates/sgbd/src/prg.rs
git commit -m "feat(sgbd): expose per-job bytecode + case-insensitive table lookup"
```

---

### Task 3: The opcode fact table (`opcode.rs`)

**Files:**
- Create: `crates/best/src/opcode.rs`
- Modify: `crates/best/src/lib.rs` (`mod opcode;`)

**Interfaces:**
- Produces: `OpInfo { mnemonic: &'static str, class: OpClass, is_jump: bool }`, `fn info(byte: u8) -> Option<&'static OpInfo>`, and `enum OpClass { Arith, Control, Flag, Stack, StringOp, Comm, Result, Float, Table, Param, ByteConv, Misc, Unimplemented }`.

**Facts source:** `ediabaslib/EdiabasLib/EdiabasLib/EdiabasNet.cs:1951-2135` (`OcList`), byte + mnemonic only. **Caution:** a `null` handler in `OcList` does NOT always mean "unimplemented" — `0x0C jtsr`, `0x0D ret`, and `0x6F tosp` are core control/stack ops special-cased in the execute loop; classify them `Control`/`Stack`, not `Unimplemented`. The genuinely-unimplemented (extended-comm) null entries are `0x70 xdownl, 0x78 xstoptr, 0x84 xsendr, 0x85 xrecv, 0x86 xinfo, 0x8D xparraw, 0xA0 pcall, 0xAF xopen, 0xB0 xclose, 0xB1 xcloseex, 0xB2 xswitch, 0xB3 xsendex, 0xB4 xrecvex` → `OpClass::Unimplemented` (fail loud if reached). Phase 1 additionally treats the whole live-`x*` comm family as out-of-scope except the `xsend`/`xrequf` bridge (Task 12).

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_opcodes_map_to_class() {
        assert_eq!(info(0x00).unwrap().mnemonic, "move");
        assert_eq!(info(0x3D).unwrap().class, OpClass::Float); // fmul
        assert_eq!(info(0x7B).unwrap().class, OpClass::Table); // tabset
        assert!(info(0x0B).unwrap().is_jump); // jump
        assert_eq!(info(0xB7).unwrap().mnemonic, "tabrows");
        assert_eq!(info(0xAF).unwrap().class, OpClass::Unimplemented); // xopen (null)
    }
    #[test]
    fn out_of_range_byte_is_none() {
        assert!(info(0xC0).is_none()); // > 0xB7
    }
}
```

- [ ] **Step 2: Run to verify failure** — Run: `cargo test -p klartext-best opcode`; Expected: FAIL.

- [ ] **Step 3: Implement** — transcribe the 184-entry table (facts). Use a `const OPCODES: [OpInfo; 184]` indexed by byte, classified per the groupings below (these are the class boundaries in `OcList`):

```rust
// Classes by byte range (from OcList):
//  0x00-0x0A arith/logic/move; 0x0B-0x15 control; 0x16-0x1B flag/shift;
//  0x1C-0x1F misc/stack (nop,eoj,push,pop); 0x20-0x25 string;
//  0x26-0x33 comm (x*); 0x34-0x39 result (ergb..ergs); 0x3A-0x3E float;
//  0x3F-0x41 result (ergy,enewset,etag); 0x50 atsp(Table); tab* 0x7B-0x7D,
//  0x83 tabline, 0x9A tabseeku, 0xAA tabsetex, 0xB6/0xB7 tabcols/tabrows (Table);
//  par* 0x55-0x58,0x69,0x7F,0x80 (Param); byte-conv a2y/hex2y/y2* (ByteConv);
//  the null-handler bytes -> Unimplemented. Full list: EdiabasNet.cs:1951.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass { Arith, Control, Flag, Stack, StringOp, Comm, Result, Float, Table, Param, ByteConv, Misc, Unimplemented }

pub struct OpInfo { pub mnemonic: &'static str, pub class: OpClass, pub is_jump: bool }

pub fn info(byte: u8) -> Option<&'static OpInfo> { OPCODES.get(byte as usize) }
```

Fill `OPCODES` with all 184 entries (byte order 0x00..=0xB7). Mnemonics and the `is_jump` flag are the facts from `OcList`; class is assigned per the ranges above.

- [ ] **Step 4: Run to verify pass** — Run: `cargo test -p klartext-best opcode`; Expected: PASS.

- [ ] **Step 5: fmt + commit**

```bash
cargo fmt -p klartext-best && git add crates/best/src && git commit -m "feat(best): BEST/2 opcode fact table"
```

---

### Task 4: Addressing modes + instruction decode (`decode.rs`)

**Files:**
- Create: `crates/best/src/decode.rs`
- Modify: `crates/best/src/lib.rs` (`mod decode;`)

**Interfaces:**
- Consumes: `opcode::{info, OpClass}`.
- Produces:
  - `enum AddrMode { Imm8, Imm16, Imm32, RegB, RegI, RegL, RegS, RegF, IdxImm, IdxReg, /* … 16 total */ }`
  - `struct Op { pub byte: u8, pub mode_byte: u8, pub arg0: Operand, pub arg1: Operand, pub len: usize }`
  - `enum Operand { None, Imm(i64), Reg{ bank: RegBank, idx: u8 }, Indexed{ … } }`
  - `fn decode_job(code: &[u8]) -> Result<Vec<Op>, DecodeError>` — decodes from offset 0, stopping after `eoj` (0x1D); `DecodeError::UnknownOpcode(u8)` / `Unimplemented(&'static str)` / `Truncated`.

**Facts source:** the operand encoding (`[opcode][addrModeByte][arg0][arg1]`, addr-mode byte hi nibble = arg0 mode, lo = arg1 mode; 16 modes incl. immediate 8/16/32 LE, register, indexed/indirect+length, string) — `EdiabasNet.cs` operand-decode path. Transcribe the mode numbering; reimplement.

- [ ] **Step 1: Write failing test** (real encoding bytes — facts, hand-assembled):

```rust
#[test]
fn decodes_move_immediate_into_register() {
    // move (0x00), addr-mode byte 0x?? = (arg0 = reg B, arg1 = imm8), then operands.
    // Use the mode nibble values transcribed from EdiabasNet.cs; assert structure.
    let code = [0x00, MODE_REGB_IMM8, 0x00 /*B0*/, 0x2A /*42*/, 0x1D /*eoj*/];
    let ops = decode_job(&code).unwrap();
    assert_eq!(ops.len(), 2); // move + eoj
    assert_eq!(ops[0].byte, 0x00);
    assert!(matches!(ops[0].arg1, Operand::Imm(0x2A)));
    assert_eq!(ops[1].byte, 0x1D);
}

#[test]
fn unknown_opcode_is_hard_error() {
    assert!(matches!(decode_job(&[0xC0]), Err(DecodeError::UnknownOpcode(0xC0))));
}
```
(Define `MODE_REGB_IMM8` from the transcribed nibble constants.)

- [ ] **Step 2: Run to verify failure** — Run: `cargo test -p klartext-best decode`; Expected: FAIL.

- [ ] **Step 3: Implement** the `AddrMode` enum (16 modes), the addr-mode byte split (`hi = b >> 4`, `lo = b & 0x0F`), per-mode operand read (advancing `len`), and `decode_job` looping until `eoj`. Fail loud on `OpClass::Unimplemented` and unknown bytes. No degrade.

- [ ] **Step 4: Run to verify pass** — Run: `cargo test -p klartext-best decode`; Expected: PASS.

- [ ] **Step 5: fmt + commit** — `git commit -m "feat(best): BEST/2 instruction decoder + addressing modes"`

---

### Task 5: The machine — registers, flags, stacks, operand access (`machine.rs`)

**Files:**
- Create: `crates/best/src/machine.rs`
- Modify: `crates/best/src/lib.rs` (`mod machine;`)

**Interfaces:**
- Consumes: `decode::{Op, Operand, RegBank}`.
- Produces:
  - `struct Machine { b: [u8;16], i: [u16;16], l: [u32;8], s: [Vec<u8>;16], f: [f64;8], flags: Flags, call_stack: Vec<usize>, data_stack: Vec<u64>, pc: usize }`
  - `struct Flags { z: bool, s: bool, c: bool, v: bool }`
  - `fn read(&self, op: &Operand) -> Value` / `fn write(&mut self, op: &Operand, v: Value)` where `enum Value { Int(i64), Float(f64), Bytes(Vec<u8>) }`.

**Facts source:** register model in `docs/sgbd-findings.md` §3 (`B0-BF` u8, `I0-IF` u16, `L0-L7` u32, `S0-SF` byte/string, `F0-F7` f64; flags Z/S/C/V; call + data stacks).

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn reg_write_then_read_roundtrips() {
    let mut m = Machine::new();
    m.write(&Operand::Reg { bank: RegBank::I, idx: 3 }, Value::Int(0x0E2F));
    assert_eq!(m.read(&Operand::Reg { bank: RegBank::I, idx: 3 }), Value::Int(0x0E2F));
}
#[test]
fn b_register_truncates_to_u8() {
    let mut m = Machine::new();
    m.write(&Operand::Reg { bank: RegBank::B, idx: 0 }, Value::Int(0x1FF));
    assert_eq!(m.read(&Operand::Reg { bank: RegBank::B, idx: 0 }), Value::Int(0xFF));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p klartext-best machine`; Expected: FAIL.

- [ ] **Step 3: Implement** `Machine`, `Flags`, `Value`, and `read`/`write` dispatch over `Operand` (register banks with correct widths; immediate reads; indexed reads slice an `S` register). `Machine::new()` zeroes all.

- [ ] **Step 4: Run to verify pass** — Expected: PASS.

- [ ] **Step 5: fmt + commit** — `git commit -m "feat(best): VM machine state + operand access"`

---

### Task 6: Result model (`result.rs`)

**Files:**
- Create: `crates/best/src/result.rs`; Modify: `lib.rs` (`mod result;`)

**Interfaces:**
- Produces: `enum ResultData { Byte(u8), Word(u16), Dword(u32), Int(i64), Real(f64), Text(String), Binary(Vec<u8>) }`, `struct ResultSet { sets: Vec<Vec<(String, ResultData)>> }` with `push_named`, `new_set`, `iter_current`, and `get(name) -> Option<&ResultData>` (searches the current set).

**Facts source:** `ResultType` (`EdiabasNet.cs:1473`, values 0..10 = B/W/D/Q/C/I/L/LL/R/S/Y) and the `ergb/ergw/ergd/ergi/ergr/ergs/ergy/ergc/ergl` store ops + `enewset` (commit a set) + `etag` (filter by requested name).

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn named_results_store_and_read_back() {
    let mut rs = ResultSet::new();
    rs.push_named("STAT_MOTORTEMPERATUR_WERT", ResultData::Real(89.96));
    rs.push_named("STAT_MOTORTEMPERATUR_EINH", ResultData::Text("degC".into()));
    assert!(matches!(rs.get("STAT_MOTORTEMPERATUR_WERT"), Some(ResultData::Real(v)) if (*v-89.96).abs()<1e-9));
    assert!(matches!(rs.get("STAT_MOTORTEMPERATUR_EINH"), Some(ResultData::Text(t)) if t=="degC"));
}
```

- [ ] **Step 2-5:** run→fail, implement, run→pass, commit (`feat(best): EDIABAS result set model`).

---

### Task 7: Executor — arithmetic, logic, move, flags, shifts (`exec.rs`)

**Files:**
- Create: `crates/best/src/exec.rs`; Modify: `lib.rs` (`mod exec;`)

**Interfaces:**
- Consumes: `machine::Machine`, `decode::Op`, `opcode`.
- Produces: `fn step(m: &mut Machine, op: &Op, ctx: &mut ExecCtx) -> Result<Flow, ExecError>` where `enum Flow { Next, Jumped, EndOfJob }`; this task handles the `OpClass::{Arith, Flag}` and `move` opcodes; other classes return `ExecError::Unimplemented(mnemonic)` until their task lands.

**Scope (opcodes):** `0x00 move, 0x01 clear, 0x02 comp, 0x03 subb, 0x04 adds, 0x05 mult, 0x06 divs, 0x07 and, 0x08 or, 0x09 xor, 0x0A not, 0x16 clrc, 0x17 setc, 0x18 asr, 0x19 lsl, 0x1A lsr, 0x1B asl, 0x1C nop`. Semantics are standard integer ops with Z/S/C/V flag effects (facts; confirm carry/overflow rules against `OpAdds`/`OpSubb` in `EdiabasNet.cs`).

- [ ] **Step 1: Write failing tests** (first-principles vectors — no reference copied):

```rust
#[test]
fn mult_multiplies_into_arg0_and_sets_zero_flag() {
    let mut m = Machine::new();
    m.write(&reg_i(0), Value::Int(3));
    m.write(&reg_i(1), Value::Int(4));
    step(&mut m, &op_mult(reg_i(0), reg_i(1)), &mut ExecCtx::default()).unwrap();
    assert_eq!(m.read(&reg_i(0)), Value::Int(12));
    assert!(!m.flags.z);
}
#[test]
fn and_masks_bits() {
    let mut m = Machine::new();
    m.write(&reg_b(0), Value::Int(0xC0));
    step(&mut m, &op_and(reg_b(0), imm(0x3F)), &mut ExecCtx::default()).unwrap();
    assert_eq!(m.read(&reg_b(0)), Value::Int(0x00));
    assert!(m.flags.z);
}
```
(Provide `reg_i/reg_b/imm/op_mult/op_and` test builders.)

- [ ] **Step 2-5:** run→fail, implement the arith/flag/move handlers + the `step` dispatch skeleton (match on `info(op.byte).class`), run→pass, commit (`feat(best): executor — arithmetic/logic/move`).

---

### Task 8: Executor — control flow (`exec.rs`, cont.)

**Scope:** `0x0B jump, 0x0C jtsr, 0x0D ret, 0x0E jc, 0x0F jae, 0x10 jz, 0x11 jnz, 0x12 jv, 0x13 jnv, 0x14 jmi, 0x15 jpl, 0x47 jt, 0x48 jnt, 0x5A jg, 0x5B jge, 0x5C jl, 0x5D jle, 0x5E ja, 0x5F jbe, 0x1D eoj, 0x1E push, 0x1F pop, 0x4B break`. Jumps take a PC-relative `Imm32`; conditionals test `Flags`; `jtsr`/`ret` use the call stack; `eoj` → `Flow::EndOfJob`.

- [ ] **Step 1: Write failing test** — a countdown loop that decrements `I0` and `jnz` back until zero, asserting it terminates and `I0 == 0`.
- [ ] **Step 2-5:** run→fail, implement (map jump targets to instruction indices; the decoder must record each `Op`'s byte offset so PC-relative targets resolve — add `Op.offset` in Task 4 if not already present, and a `offset → index` map in the executor), run→pass, commit (`feat(best): executor — control flow`).

> **Note:** if Task 4's `Op` lacks a byte `offset` field, add it there (each `Op` records its start offset) so jumps resolve. Update Task 4's tests accordingly.

---

### Task 9: Executor — float + byte conversions (`exec.rs`, cont.)

**Scope:** `0x3A a2flt, 0x3B fadd, 0x3C fsub, 0x3D fmul, 0x3E fdiv, 0x67 a2fix, 0x68 fix2flt, 0x96 flt2fix, 0x87 flt2a, 0x8C a2y, 0x8E hex2y, 0x91 y2bcd, 0x92 y2hex, 0x9D y42flt, 0x9E y82flt`. IEEE-754 double math in the `F` registers; `a2flt` converts an integer/bytes reg to float; `fix2flt`/`flt2fix` convert between `L`/`F`. Byte-conv ops move between `S` (byte buffers) and numeric/hex string forms.

- [ ] **Step 1: Write failing test** (the engine scaling math, first-principles):

```rust
#[test]
fn fmul_then_fadd_scales_engine_temp() {
    // F0 = raw 3631, F1 = 0.1  ; fmul -> 363.1 ; then fadd offset -273.14 -> 89.96
    let mut m = Machine::new();
    m.f[0] = 3631.0; m.f[1] = 0.1;
    step(&mut m, &op_fmul(reg_f(1), reg_f(0)), &mut ExecCtx::default()).unwrap(); // F1 *= F0
    assert!((m.f[1] - 363.1).abs() < 1e-6);
    m.f[0] = -273.14;
    step(&mut m, &op_fadd(reg_f(1), reg_f(0)), &mut ExecCtx::default()).unwrap();
    assert!((m.f[1] - 89.96).abs() < 1e-6);
}
```

- [ ] **Step 2-5:** run→fail, implement, run→pass, commit (`feat(best): executor — float + byte conversions`).

---

### Task 10: Executor — string + result-store + param ops (`exec.rs`, cont.)

**Scope:** string `0x20 scmp, 0x21 scat, 0x22 scut, 0x23 slen, 0x24 spaste, 0x25 serase, 0x7E strcat, 0x8F strcmp, 0x90 strlen`; result-store `0x34 ergb .. 0x39 ergs, 0x3F ergy, 0x81 ergc, 0x82 ergl` + `0x40 enewset, 0x41 etag`; param `0x55 parb, 0x56 parw, 0x57 parl, 0x58 pars, 0x69 parr, 0x7F pary, 0x80 parn` (read the job's input arguments from `ExecCtx.args`).

- [ ] **Step 1: Write failing test** — `ergr("NAME", F1)` pushes `ResultData::Real` into the current set; `enewset` starts a new set; a `parl` reads arg 0 into `L0`.
- [ ] **Step 2-5:** run→fail, implement (result-store ops write into the `ResultSet` from Task 6; `ExecCtx` carries `args: &[JobArg]` and `results: &mut ResultSet`), run→pass, commit (`feat(best): executor — string/result/param`).

---

### Task 11: Table ops + the SG_FUNKTIONEN / RES_ measurement decode (`table.rs`)

**Files:**
- Create: `crates/best/src/table.rs`; Modify: `exec.rs` (route `OpClass::Table`), `lib.rs`.

**Interfaces:**
- Consumes: `klartext_sgbd::{Prg, Table}` (via `table_ci`), `machine::Machine`, `result::ResultData`.
- Produces: handlers for `0x50 atsp, 0x7B tabset, 0x7C tabseek, 0x7D tabget, 0x9A tabseeku, 0x83 tabline, 0xB6 tabcols, 0xB7 tabrows`; and the helper `decode_res_row(cols, row, raw: &[u8], cursor: &mut usize) -> (String, ResultData)` implementing the `RES_` walk: per sub-result read `DATENTYP` width from `raw` at the running cursor, apply `MASKE` bit extraction when present, scale `raw·MUL/DIV+ADD`, attach `EINHEIT`.

**Facts source (verified this session):** `SG_FUNKTIONEN` columns `ARG,ID,RESULTNAME,INFO,EINHEIT,LABEL,L/H,DATENTYP,NAME,MUL,DIV,ADD,SG_ADR,SERVICE,ARG_TABELLE,RES_TABELLE`; a `RES_<did>` table's columns `RESULTNAME,EINHEIT,L/H,DATENTYP,MASKE,NAME,MUL,DIV,ADD,INFO`, one row per sub-result; `RES_TABELLE` refs resolve via `table_ci`.

- [ ] **Step 1: Write failing test** (synthetic tables, like `measurement.rs`'s):

```rust
#[test]
fn res_walk_decodes_two_subresults_with_mask() {
    // A RES_ table: sub-result A = u8 scaled /64 (volts), sub-result B = bitfield MASKE 0x01.
    let res = Table { name: "RES_0X5001".into(),
        columns: cols(&["RESULTNAME","EINHEIT","L/H","DATENTYP","MASKE","NAME","MUL","DIV","ADD","INFO"]),
        rows: vec![
            row(&["STAT_UBAT_WERT","V","-","unsigned char","-","-","1","64","0","Versorgungsspannung"]),
            row(&["STAT_KL15","-","-","BITFIELD","0x01","-","-","-","-","Klemme 15"]),
        ] };
    let raw = [0x80, 0x01]; // 128/64 = 2.0 V ; bit0 = 1
    let mut cursor = 0usize;
    let (n0, v0) = decode_res_row(&res.columns, &res.rows[0], &raw, &mut cursor);
    assert_eq!(n0, "STAT_UBAT_WERT");
    assert!(matches!(v0, ResultData::Real(v) if (v-2.0).abs()<1e-9));
    let (n1, v1) = decode_res_row(&res.columns, &res.rows[1], &raw, &mut cursor);
    assert_eq!(n1, "STAT_KL15");
    assert!(matches!(v1, ResultData::Int(1)));
}
```

- [ ] **Step 2-5:** run→fail, implement `decode_res_row` + the `tab*`/`atsp` handlers (`tabset` selects a table into `ExecCtx.current_table`; `tabseek` finds a row by key; `tabget`/`atsp` read a cell; `tabcols/tabrows` report dimensions), run→pass, commit (`feat(best): table ops + SG_FUNKTIONEN/RES_ measurement decode`).

---

### Task 12: `UdsExchange` trait + `MockExchange` + the comm bridge

**Files:**
- Create: `crates/best/src/exchange.rs`; Modify: `exec.rs` (route `xsend`/`xrequf`), `lib.rs`.

**Interfaces:**
- Produces:
  - `#[async_trait] trait UdsExchange { async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError>; }` (or a boxed-future trait to avoid the dep — match the workspace's async-trait convention; `crates/client` shows the pattern).
  - `struct MockExchange { map: HashMap<Vec<u8>, Vec<u8>> }` with `on(request, response)` and a default "unexpected request" error.
  - comm handlers for `0x2A xsend` and `0x2C xrequf` (Phase-1 subset): assemble the request bytes from the referenced `S` register, `await` `exchange.request(target, req)`, place the response into the destination `S` register. All other `OpClass::Comm` opcodes → `ExecError::Unimplemented` (Phase 2).

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn mock_exchange_returns_canned_response() {
    let mut mock = MockExchange::new();
    mock.on(vec![0x22, 0xF3, 0x03], vec![0x62, 0xF3, 0x03, 0x0E, 0x2F]);
    assert_eq!(mock.request(0x12, &[0x22, 0xF3, 0x03]).await.unwrap(),
               vec![0x62, 0xF3, 0x03, 0x0E, 0x2F]);
}
```

- [ ] **Step 2-5:** run→fail, implement (`cargo add -p klartext-best async-trait` only if the workspace already uses it; otherwise a manual `Pin<Box<dyn Future>>` return like `crates/client`), run→pass, commit (`feat(best): UdsExchange trait + mock + comm bridge`).

---

### Task 13: `Ecu` + `run_job` + the engine oracle (integration)

**Files:**
- Create: `crates/best/src/engine.rs` (or in `lib.rs`); Test: `crates/best/tests/oracle.rs`.

**Interfaces:**
- Consumes: everything above.
- Produces: `struct Ecu { prg: Prg }` with `Ecu::load(prg: Prg) -> Self`; `async fn run_job(&self, name: &str, args: &[JobArg], exchange: &dyn UdsExchange) -> Result<ResultSet, RunError>` — decode the job bytecode, run the `step` loop over the `Machine` until `EndOfJob`, threading `ExecCtx { args, results, exchange, tables: &self.prg }`.

- [ ] **Step 1: Write the failing oracle test**

```rust
// crates/best/tests/oracle.rs — offline, no car.
#[tokio::test]
async fn engine_temperature_matches_measurement_rs() {
    // Load the real F20 DDE (BYO data; ignore if absent).
    let path = "data/Testmodule(1)/Ecu/d72n47a0.prg";
    let Ok(prg) = klartext_sgbd::Prg::open(path) else { return; };
    let ecu = klartext_best::Ecu::load(prg);

    // Mock the 2C/22 selektiv-lesen sequence for STAT_MOTORTEMPERATUR (id 0x4BC3, u16).
    let mut mock = klartext_best::MockExchange::new();
    mock.on(vec![0x2C,0x03,0xF3,0x03], vec![0x6C,0x03,0xF3,0x03]);
    mock.on(vec![0x2C,0x01,0xF3,0x03,0x4B,0xC3,0x01,0x02], vec![0x6C,0x01,0xF3,0x03]);
    mock.on(vec![0x22,0xF3,0x03], vec![0x62,0xF3,0x03,0x0E,0x2F]); // raw 0x0E2F

    let rs = ecu.run_job("STATUS_MOTORTEMPERATUR", &[], &mock).await.unwrap();
    match rs.get("STAT_MOTORTEMPERATUR_WERT") {
        Some(klartext_best::ResultData::Real(v)) => assert!((v-89.96).abs()<0.01, "got {v}"),
        other => panic!("expected Real, got {other:?}"),
    }
}
```

Mark it `#[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]` (mirrors the existing `real_dde_*` tests) so CI without data is green; it is run manually with data present.

- [ ] **Step 2: Run to verify it fails** — Run: `cargo test -p klartext-best --test oracle -- --ignored`; Expected: FAIL (unimplemented opcode encountered, or `run_job` absent). The failure names the first missing opcode — implement it (loop back through Tasks 7-12 as the job demands; the measurement job uses ~43 opcodes).

- [ ] **Step 3: Implement `Ecu::run_job` + the step loop**, growing opcode coverage until the job runs to `eoj` and the oracle passes. Each newly-needed opcode gets its own unit test first (TDD), added to the relevant Task-7-12 module.

- [ ] **Step 4: Run to verify pass** — Run: `cargo test -p klartext-best --test oracle -- --ignored` (with data); Expected: PASS. Also add a **non-ignored** structured-decode proof test using synthetic `SG_FUNKTIONEN` + `RES_` tables (no BYO data) that exercises the `RES_`/`MASKE` path end-to-end through `run_job` with a mock.

- [ ] **Step 5: fmt + commit** — `git commit -m "feat(best): Ecu::run_job + offline engine oracle"`

---

### Task 14: Phase 1 polish — clippy, docs, cross-check

- [ ] **Step 1:** `cargo fmt --all` (via Bash) and `cargo clippy -p klartext-best -- -D warnings`; fix all.
- [ ] **Step 2:** `cargo doc -p klartext-best --no-deps` warning-free (the workspace keeps docs clean).
- [ ] **Step 3:** Confirm the whole suite: `cargo test -p klartext-best` (offline) and `cargo test -p klartext-best -- --ignored` (with BYO data) both green.
- [ ] **Step 4:** Commit (`chore(best): fmt + clippy + docs clean for Phase 1`).

---

## Self-Review

**Spec coverage (§ of the design doc → task):**
- §4 crate/layers → Tasks 1,3,4,5 (decode/machine), 7-12 (exec/comm), 13 (engine). ✅
- §4 sgbd seam (per-job bytecode + CI lookup) → Task 2. ✅
- §4 `UdsExchange` seam (mock) → Task 12. ✅
- §5 measurement path (SG_FUNKTIONEN + RES_ walk + MASKE) → Task 11. ✅
- §5 engine oracle vs `measurement.rs` → Task 13. ✅
- §7 no-degrade-inside-VM → enforced in Tasks 3,4 (hard errors) and Global Constraints. ✅
- §8 testing (per-opcode vectors, whole-job offline, structured-decode proof, real-`.prg` ignored) → Tasks 7-13. ✅
- §9 Phase-1 boundary (no comm family beyond the bridge, no live Session, no write-gate) → Global Constraints + Task 12 scope note. ✅
- §3 opcode table / result types → Tasks 3, 6. ✅

**Deferred to Phase 2/3 (correctly absent here):** full `x*` comm family + transport/timing opcodes, live `Session` exchange, `.grp`→variant IDENT dispatch, `GatedExchange`/write classification, actuation jobs.

**Placeholder scan:** opcode-handler tasks (7-12) specify the exact opcode *set*, semantics source, and representative TDD vectors rather than every handler body — deliberate right-sizing (a task = one opcode class), not a placeholder; each still ships tested code. No "TBD"/"handle edge cases" left.

**Type consistency:** `Op`, `Operand`, `Machine`, `Value`, `ResultData`, `ResultSet`, `UdsExchange`, `Ecu::run_job`, `MockExchange` names are used consistently across Tasks 4-13. Task 8 flags the one back-reference (add `Op.offset` in Task 4) explicitly.

**Open item carried from spec §10:** the `RES_` byte-offset mechanism is implemented as a sequential cursor (Task 11); if a real job proves non-sequential, revisit in Task 13 when the oracle exercises it.
