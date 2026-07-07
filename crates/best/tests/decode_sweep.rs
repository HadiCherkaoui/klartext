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
