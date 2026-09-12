//! Translation memory (spec §12): stores every finished translation and
//! reuses it by `(source_hash, target_language)` so repeated text never
//! hits the AI twice.
//!
//! v1 policy: exact-text reuse. Context-sensitive reuse would slot in here
//! without touching the pipeline's call sites.

use anyhow::Result;

use crate::database::Db;

pub fn lookup(db: &Db, source_hash: &str, target_language: &str) -> Result<Option<String>> {
    db.memory_get(source_hash, target_language)
}

/// Store finished translations in one transaction.
/// `rows`: (source_hash, source_text, translated_text).
pub fn store(db: &Db, rows: &[(String, String, String)], target_language: &str) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    db.memory_put_many(rows, target_language)
}
