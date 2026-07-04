//! klartext-docbuild — build the compact repair-doc store (`klartext-docs.db`)
//! from the plaintext semantic DB (FKB pointers) and ISTA's plaintext
//! `xmlvalueprimitive_DEDE.sqlite` (bodies). Build-only, BYO-data: reads the
//! user's own decrypted data, writes a gitignored artifact, embeds nothing.
mod build;
mod fkb;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

/// Build klartext-docs.db (Phase 1: FKB fault-description bodies).
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The plaintext semantic DB (from scripts/build-semantic-db.sh).
    #[arg(long, default_value = "data/klartext-semantic.db")]
    semantic_db: PathBuf,
    /// ISTA's plaintext German prose DB (xmlvalueprimitive_DEDE.sqlite).
    #[arg(long)]
    xmlvalue_db: PathBuf,
    /// Output doc-store DB (sibling of the semantic DB at runtime).
    #[arg(long, default_value = "data/klartext-docs.db")]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let n = build::build_fkb(&args.semantic_db, &args.xmlvalue_db, &args.out)?;
    eprintln!(
        "klartext-docs.db: wrote {n} FKB bodies → {}",
        args.out.display()
    );
    Ok(())
}
