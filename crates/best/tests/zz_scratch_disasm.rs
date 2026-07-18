//! SCRATCH research disassembler — DELETE AFTER USE.
use klartext_best::{IndexArg, Op, Operand, decode_job, info};
use klartext_sgbd::Prg;

fn fmt_operand(o: &Operand) -> String {
    match o {
        Operand::None => String::new(),
        Operand::Imm(v) => format!("#0x{v:X}"),
        Operand::Str(s) => {
            let txt: String = s
                .iter()
                .map(|&b| {
                    if (0x20..0x7F).contains(&b) {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!(
                "\"{txt}\"[{}]",
                s.iter()
                    .map(|b| format!("{b:02X}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
        Operand::Reg { bank, idx } => format!("{bank:?}{idx}"),
        Operand::Indexed { base, index, len } => {
            let i = match index {
                IndexArg::Imm(v) => format!("{v}"),
                IndexArg::Reg(r) => format!("{:?}{}", r.bank, r.idx),
            };
            let l = match len {
                Some(IndexArg::Imm(v)) => format!(",{v}"),
                Some(IndexArg::Reg(r)) => format!(",{:?}{}", r.bank, r.idx),
                None => String::new(),
            };
            format!("{:?}{}[{i}{l}]", base.bank, base.idx)
        }
    }
}

fn fmt_op(op: &Op) -> String {
    let m = info(op.byte).map(|i| i.mnemonic).unwrap_or("???");
    let a0 = fmt_operand(&op.arg0);
    let a1 = fmt_operand(&op.arg1);
    let args = match (a0.is_empty(), a1.is_empty()) {
        (true, true) => String::new(),
        (false, true) => a0,
        (true, false) => a1,
        (false, false) => format!("{a0}, {a1}"),
    };
    format!("{:06X}: {:<8} {}", op.offset, m, args)
}

#[test]
fn scratch_dump() {
    let dir = std::path::Path::new("../../data/Testmodule(1)/Ecu");
    let file = std::env::var("SCRATCH_PRG").unwrap_or_else(|_| "f01.prg".into());
    let job = std::env::var("SCRATCH_JOB").unwrap_or_else(|_| "FS_LOESCHEN_FUNKTIONAL".into());
    let prg = Prg::open(dir.join(&file)).expect("SGBD parses");

    if std::env::var("SCRATCH_LIST").is_ok() {
        println!("=== JOBS in {file} ({}) ===", prg.job_names().len());
        for j in prg.job_names() {
            println!("  {j}");
        }
        println!("=== TABLES in {file} ({}) ===", prg.tables().len());
        for t in prg.tables() {
            println!("  {}", t.name);
        }
        return;
    }

    if let Ok(t) = std::env::var("SCRATCH_TABLE") {
        let tbl = prg.table_ci(&t).unwrap_or_else(|| panic!("no table {t}"));
        println!("=== TABLE {} cols={:?} ===", tbl.name, tbl.columns);
        for r in &tbl.rows {
            println!("  {r:?}");
        }
        return;
    }

    let code = prg
        .job_bytecode(&job)
        .unwrap_or_else(|| panic!("no job {job} in {file}"));
    let ops = decode_job(code).expect("decodes");
    println!(
        "=== {file} / {job} : {} bytes, {} ops ===",
        code.len(),
        ops.len()
    );
    for op in &ops {
        println!("{}", fmt_op(op));
    }
}
