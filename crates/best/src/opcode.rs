//! Authoritative BEST/2 opcode metadata: byte, mnemonic, class, jump flag.
//!
//! One [`OpInfo`] per opcode covers the contiguous BEST/2 opcode space
//! `0x00..=0xB7` (184 entries); [`info`] looks an entry up by its leading
//! instruction byte. The decoder ([`crate`]'s `decode`, later task) reads the
//! mnemonic and [`OpClass`] to drive operand decoding and executor dispatch,
//! and reads `is_jump` to know an operand is a PC-relative code offset.
//!
//! ## Where the facts come from
//! The byte-to-mnemonic assignment and the jump flag are **facts** about BMW's
//! binary format, transcribed from EDIABAS's opcode list and reimplemented in
//! our own types — no source code or test vectors are copied (klartext is
//! AGPL-3.0; the reference is read as an offline oracle only).
//!
//! ## The class scheme
//! [`OpClass`] is a coarse semantic grouping (arithmetic, control flow, float,
//! table, result-store, …) used to organize the decoder and executor. It
//! follows the opcode-class boundaries of the byte space; [`OpClass::Misc`] is
//! the catch-all for opcodes that fall outside a named range.
//!
//! Two subtleties, both load-bearing:
//! - [`OpClass::Unimplemented`] marks the extended-communication opcodes that
//!   EDIABAS itself leaves unhandled (no handler in the reference). They are
//!   not real machine operations; the decoder fails loud on them rather than
//!   guessing. Exactly thirteen bytes: `0x70, 0x78, 0x84, 0x85, 0x86, 0x8D,
//!   0xA0, 0xAF, 0xB0, 0xB1, 0xB2, 0xB3, 0xB4`.
//! - `jtsr` (`0x0C`), `ret` (`0x0D`), and `tosp` (`0x6F`) also lack a handler
//!   in the reference list, but they are **real** core control/stack operations
//!   special-cased in the execute loop. They are classed [`OpClass::Control`]
//!   and [`OpClass::Stack`], never `Unimplemented`.

/// Semantic family of a BEST/2 opcode, used to group decode/execute handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    /// Integer arithmetic, logic, and register move (`move`, `adds`, `and`, …).
    Arith,
    /// Branches, calls, and returns (`jump`, `jc`, `jtsr`, `ret`, …).
    Control,
    /// Carry/flag manipulation and bit shifts (`clrc`, `setc`, `asr`, `lsl`, …).
    Flag,
    /// Stack pushes and pops (`push`, `pop`, `tosp`).
    Stack,
    /// String-buffer operations (`scmp`, `scat`, `slen`, …).
    StringOp,
    /// External communication with the ECU (`xconnect`, `xsend`, `xrequf`, …).
    Comm,
    /// Named result-set store and set control (`ergb`..`ergs`, `ergy`, `etag`).
    Result,
    /// IEEE-754 float arithmetic and conversion into float (`fadd`, `fmul`, …).
    Float,
    /// Table selection and cell/dimension access (`atsp`, `tabset`, `tabrows`, …).
    Table,
    /// Reads of the job's input arguments (`parb`, `parw`, `parl`, `pary`, …).
    Param,
    /// Byte-buffer to/from BCD/hex representation conversions (`a2y`, `y2hex`, …).
    ByteConv,
    /// Everything outside a named range: timers, files, config, misc conversions.
    Misc,
    /// Extended-comm opcodes EDIABAS leaves unhandled; the decoder fails loud.
    Unimplemented,
}

/// Static metadata for one BEST/2 opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpInfo {
    /// EDIABAS mnemonic for the opcode (e.g. `"move"`, `"fmul"`, `"tabset"`).
    pub mnemonic: &'static str,
    /// Coarse semantic family used to route decode and execution.
    pub class: OpClass,
    /// Whether the instruction's operand is a PC-relative jump/call target.
    pub is_jump: bool,
}

/// Shorthand constructor for a static table entry.
const fn op(mnemonic: &'static str, class: OpClass, is_jump: bool) -> OpInfo {
    OpInfo {
        mnemonic,
        class,
        is_jump,
    }
}

/// The 184 opcodes `0x00..=0xB7`, indexed by their leading instruction byte.
const OPCODES: [OpInfo; 184] = {
    use OpClass::{
        Arith, ByteConv, Comm, Control, Flag, Float, Misc, Param, Result, Stack, StringOp, Table,
        Unimplemented,
    };
    [
        op("move", Arith, false),             // 0x00
        op("clear", Arith, false),            // 0x01
        op("comp", Arith, false),             // 0x02
        op("subb", Arith, false),             // 0x03
        op("adds", Arith, false),             // 0x04
        op("mult", Arith, false),             // 0x05
        op("divs", Arith, false),             // 0x06
        op("and", Arith, false),              // 0x07
        op("or", Arith, false),               // 0x08
        op("xor", Arith, false),              // 0x09
        op("not", Arith, false),              // 0x0A
        op("jump", Control, true),            // 0x0B
        op("jtsr", Control, true),            // 0x0C  null handler; core control op
        op("ret", Control, false),            // 0x0D  null handler; core control op
        op("jc", Control, true),              // 0x0E
        op("jae", Control, true),             // 0x0F
        op("jz", Control, true),              // 0x10
        op("jnz", Control, true),             // 0x11
        op("jv", Control, true),              // 0x12
        op("jnv", Control, true),             // 0x13
        op("jmi", Control, true),             // 0x14
        op("jpl", Control, true),             // 0x15
        op("clrc", Flag, false),              // 0x16
        op("setc", Flag, false),              // 0x17
        op("asr", Flag, false),               // 0x18
        op("lsl", Flag, false),               // 0x19
        op("lsr", Flag, false),               // 0x1A
        op("asl", Flag, false),               // 0x1B
        op("nop", Misc, false),               // 0x1C
        op("eoj", Misc, false),               // 0x1D
        op("push", Stack, false),             // 0x1E
        op("pop", Stack, false),              // 0x1F
        op("scmp", StringOp, false),          // 0x20
        op("scat", StringOp, false),          // 0x21
        op("scut", StringOp, false),          // 0x22
        op("slen", StringOp, false),          // 0x23
        op("spaste", StringOp, false),        // 0x24
        op("serase", StringOp, false),        // 0x25
        op("xconnect", Comm, false),          // 0x26
        op("xhangup", Comm, false),           // 0x27
        op("xsetpar", Comm, false),           // 0x28
        op("xawlen", Comm, false),            // 0x29
        op("xsend", Comm, false),             // 0x2A
        op("xsendf", Comm, false),            // 0x2B
        op("xrequf", Comm, false),            // 0x2C
        op("xstopf", Comm, false),            // 0x2D
        op("xkeyb", Comm, false),             // 0x2E
        op("xstate", Comm, false),            // 0x2F
        op("xboot", Comm, false),             // 0x30
        op("xreset", Comm, false),            // 0x31
        op("xtype", Comm, false),             // 0x32
        op("xvers", Comm, false),             // 0x33
        op("ergb", Result, false),            // 0x34
        op("ergw", Result, false),            // 0x35
        op("ergd", Result, false),            // 0x36
        op("ergi", Result, false),            // 0x37
        op("ergr", Result, false),            // 0x38
        op("ergs", Result, false),            // 0x39
        op("a2flt", Float, false),            // 0x3A
        op("fadd", Float, false),             // 0x3B
        op("fsub", Float, false),             // 0x3C
        op("fmul", Float, false),             // 0x3D
        op("fdiv", Float, false),             // 0x3E
        op("ergy", Result, false),            // 0x3F
        op("enewset", Result, false),         // 0x40
        op("etag", Result, true),             // 0x41
        op("xreps", Misc, false),             // 0x42
        op("gettmr", Misc, false),            // 0x43
        op("settmr", Misc, false),            // 0x44
        op("sett", Misc, false),              // 0x45
        op("clrt", Misc, false),              // 0x46
        op("jt", Misc, true),                 // 0x47
        op("jnt", Misc, true),                // 0x48
        op("addc", Misc, false),              // 0x49
        op("subc", Misc, false),              // 0x4A
        op("break", Misc, false),             // 0x4B
        op("clrv", Misc, false),              // 0x4C
        op("eerr", Misc, false),              // 0x4D
        op("popf", Misc, false),              // 0x4E
        op("pushf", Misc, false),             // 0x4F
        op("atsp", Table, false),             // 0x50
        op("swap", Misc, false),              // 0x51
        op("setspc", Misc, false),            // 0x52
        op("srevrs", Misc, false),            // 0x53
        op("stoken", Misc, false),            // 0x54
        op("parb", Param, false),             // 0x55
        op("parw", Param, false),             // 0x56
        op("parl", Param, false),             // 0x57
        op("pars", Param, false),             // 0x58
        op("fclose", Misc, false),            // 0x59
        op("jg", Misc, true),                 // 0x5A
        op("jge", Misc, true),                // 0x5B
        op("jl", Misc, true),                 // 0x5C
        op("jle", Misc, true),                // 0x5D
        op("ja", Misc, true),                 // 0x5E
        op("jbe", Misc, true),                // 0x5F
        op("fopen", Misc, false),             // 0x60
        op("fread", Misc, false),             // 0x61
        op("freadln", Misc, false),           // 0x62
        op("fseek", Misc, false),             // 0x63
        op("fseekln", Misc, false),           // 0x64
        op("ftell", Misc, false),             // 0x65
        op("ftellln", Misc, false),           // 0x66
        op("a2fix", Misc, false),             // 0x67
        op("fix2flt", Misc, false),           // 0x68
        op("parr", Param, false),             // 0x69
        op("test", Misc, false),              // 0x6A
        op("wait", Misc, false),              // 0x6B
        op("date", Misc, false),              // 0x6C
        op("time", Misc, false),              // 0x6D
        op("xbatt", Misc, false),             // 0x6E
        op("tosp", Stack, false),             // 0x6F  null handler; core stack op
        op("xdownl", Unimplemented, false),   // 0x70  null handler; extended-comm
        op("xgetport", Misc, false),          // 0x71
        op("xignit", Misc, false),            // 0x72
        op("xloopt", Misc, false),            // 0x73
        op("xprog", Misc, false),             // 0x74
        op("xraw", Misc, false),              // 0x75
        op("xsetport", Misc, false),          // 0x76
        op("xsireset", Misc, false),          // 0x77
        op("xstoptr", Unimplemented, false),  // 0x78  null handler; extended-comm
        op("fix2hex", Misc, false),           // 0x79
        op("fix2dez", Misc, false),           // 0x7A
        op("tabset", Table, false),           // 0x7B
        op("tabseek", Table, false),          // 0x7C
        op("tabget", Table, false),           // 0x7D
        op("strcat", Misc, false),            // 0x7E
        op("pary", Param, false),             // 0x7F
        op("parn", Param, false),             // 0x80
        op("ergc", Misc, false),              // 0x81
        op("ergl", Misc, false),              // 0x82
        op("tabline", Table, false),          // 0x83
        op("xsendr", Unimplemented, false),   // 0x84  null handler; extended-comm
        op("xrecv", Unimplemented, false),    // 0x85  null handler; extended-comm
        op("xinfo", Unimplemented, false),    // 0x86  null handler; extended-comm
        op("flt2a", Misc, false),             // 0x87
        op("setflt", Misc, false),            // 0x88
        op("cfgig", Misc, false),             // 0x89
        op("cfgsg", Misc, false),             // 0x8A
        op("cfgis", Misc, false),             // 0x8B
        op("a2y", ByteConv, false),           // 0x8C
        op("xparraw", Unimplemented, false),  // 0x8D  null handler; extended-comm
        op("hex2y", ByteConv, false),         // 0x8E
        op("strcmp", Misc, false),            // 0x8F
        op("strlen", Misc, false),            // 0x90
        op("y2bcd", ByteConv, false),         // 0x91
        op("y2hex", ByteConv, false),         // 0x92
        op("shmset", Misc, false),            // 0x93
        op("shmget", Misc, false),            // 0x94
        op("ergsysi", Misc, false),           // 0x95
        op("flt2fix", Misc, false),           // 0x96
        op("iupdate", Misc, false),           // 0x97
        op("irange", Misc, false),            // 0x98
        op("iincpos", Misc, false),           // 0x99
        op("tabseeku", Table, false),         // 0x9A
        op("flt2y4", Misc, false),            // 0x9B
        op("flt2y8", Misc, false),            // 0x9C
        op("y42flt", Misc, false),            // 0x9D
        op("y82flt", Misc, false),            // 0x9E
        op("plink", Misc, false),             // 0x9F
        op("pcall", Unimplemented, false),    // 0xA0  null handler; extended-comm
        op("fcomp", Misc, false),             // 0xA1
        op("plinkv", Misc, false),            // 0xA2
        op("ppush", Misc, false),             // 0xA3
        op("ppop", Misc, false),              // 0xA4
        op("ppushflt", Misc, false),          // 0xA5
        op("ppopflt", Misc, false),           // 0xA6
        op("ppushy", Misc, false),            // 0xA7
        op("ppopy", Misc, false),             // 0xA8
        op("pjtsr", Misc, false),             // 0xA9  non-jump in OcList (no target operand)
        op("tabsetex", Table, false),         // 0xAA
        op("ufix2dez", Misc, false),          // 0xAB
        op("generr", Misc, false),            // 0xAC
        op("ticks", Misc, false),             // 0xAD
        op("waitex", Misc, false),            // 0xAE
        op("xopen", Unimplemented, false),    // 0xAF  null handler; extended-comm
        op("xclose", Unimplemented, false),   // 0xB0  null handler; extended-comm
        op("xcloseex", Unimplemented, false), // 0xB1  null handler; extended-comm
        op("xswitch", Unimplemented, false),  // 0xB2  null handler; extended-comm
        op("xsendex", Unimplemented, false),  // 0xB3  null handler; extended-comm
        op("xrecvex", Unimplemented, false),  // 0xB4  null handler; extended-comm
        op("ssize", Misc, false),             // 0xB5
        op("tabcols", Table, false),          // 0xB6
        op("tabrows", Table, false),          // 0xB7
    ]
};

/// Looks up opcode metadata for `byte`, or `None` when `byte > 0xB7`.
///
/// The table is the contiguous opcode space `0x00..=0xB7`; any higher byte is
/// not a defined BEST/2 opcode. Returning `None` lets the decoder treat an
/// out-of-range byte as a hard error instead of guessing a meaning.
pub fn info(byte: u8) -> Option<&'static OpInfo> {
    OPCODES.get(byte as usize)
}

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
