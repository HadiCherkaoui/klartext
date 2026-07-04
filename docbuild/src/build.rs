//! Extract FKB bodies → render → gzip → write klartext-docs.db.
use std::path::Path;

use anyhow::Result;

/// Build the `fkb_body` table. Returns the number of bodies written.
pub fn build_fkb(_semantic_db: &Path, _xmlvalue_db: &Path, _out: &Path) -> Result<usize> {
    Ok(0)
}
