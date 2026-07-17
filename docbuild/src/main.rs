//! klartext-docbuild — build the compact repair-doc store (`klartext-docs.db`)
//! from the plaintext semantic DB (FKB pointers) and ISTA's plaintext
//! `xmlvalueprimitive_DEDE.sqlite` (bodies), and — when the language-neutral
//! `xmlvalueprimitive_OTHER.sqlite` is given — the ISTA ECU-tree topology
//! tables (`ecu_tree`/`ecu_housing`) back into the semantic DB. Build-only,
//! BYO-data: reads the user's own decrypted data, writes gitignored artifacts,
//! embeds nothing.
mod bordnet;
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
    /// ISTA's plaintext language-neutral DB (xmlvalueprimitive_OTHER.sqlite);
    /// when given, the BNT-XML bordnets are parsed into the semantic DB's
    /// `ecu_tree`/`ecu_housing` tables.
    #[arg(long)]
    xmlvalue_other_db: Option<PathBuf>,
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
    if let Some(other) = &args.xmlvalue_other_db {
        let (platforms, rows) = bordnet::build_ecu_tree(&args.semantic_db, other)?;
        eprintln!(
            "ecu_tree: wrote {rows} ECU rows over {platforms} platforms → {}",
            args.semantic_db.display()
        );
    }
    Ok(())
}
