//! Project scan orchestration (spec §20/§21):
//! extract → hash → compare against SQLite → New / Changed / Existing.

use anyhow::Result;
use std::path::Path;

use crate::core::engine::GameEngine;
use crate::core::project::Project;
use crate::database::{Db, ScanReport};

/// Extract all sources from the game directory and apply them to the
/// project incrementally.
pub fn scan_project(db: &Db, project: &Project, engine: &dyn GameEngine) -> Result<ScanReport> {
    let extraction = engine.extract(Path::new(&project.path))?;
    db.scan_apply(project, &extraction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::engine::detect_engine;
    use crate::core::translation::TranslationStatus;
    use std::fs;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gtl-scan-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(dir.join("game")).unwrap();
        dir
    }

    #[test]
    fn incremental_scan_reuses_and_resets() {
        let root = temp_root("incr");
        let script = root.join("game/script.rpy");
        fs::write(&script, "e \"Hello\"\n").unwrap();

        let db = Db::open_in_memory().unwrap();
        let project = db
            .project_upsert(&Project::new("Game", root.to_string_lossy(), "renpy"))
            .unwrap();
        let engine = detect_engine(&root).unwrap();

        // First scan: 1 new entry.
        let r1 = scan_project(&db, &project, engine).unwrap();
        assert_eq!((r1.total, r1.added), (1, 1));

        // A translation is stored (pipeline writes both the translation and
        // the translation-memory row).
        let id = format!("{}|script.rpy|1", project.id);
        db.set_translation(&id, Some("สวัสดี"), TranslationStatus::Translated)
            .unwrap();
        db.memory_put_many(
            &[(
                crate::core::source::hash_text("Hello"),
                "Hello".to_string(),
                "สวัสดี".to_string(),
            )],
            &project.target_language,
        )
        .unwrap();
        // Rescan with no changes: unchanged, translation kept.
        let r2 = scan_project(&db, &project, engine).unwrap();
        assert_eq!(r2.unchanged, 1);
        assert_eq!(
            db.source_by_id(&id)
                .unwrap()
                .unwrap()
                .translated_text
                .as_deref(),
            Some("สวัสดี")
        );

        // Game update changes the line: reset to Pending.
        fs::write(&script, "e \"Hello again\"\n").unwrap();
        let r3 = scan_project(&db, &project, engine).unwrap();
        assert_eq!(r3.changed, 1);
        let entry = db.source_by_id(&id).unwrap().unwrap();
        assert_eq!(entry.source.source_text, "Hello again");
        assert_eq!(entry.status, TranslationStatus::Pending);

        // The old translation is still in memory: a new entry with the old
        // text (e.g. moved to another line) is prefilled automatically.
        fs::write(&script, "e \"Greeting\"\ne \"Hello\"\n").unwrap();
        let r4 = scan_project(&db, &project, engine).unwrap();
        assert_eq!(r4.from_memory, 1);
        let moved_id = format!("{}|script.rpy|2", project.id);
        let moved = db.source_by_id(&moved_id).unwrap().unwrap();
        assert_eq!(moved.translated_text.as_deref(), Some("สวัสดี"));
        assert_eq!(moved.status, TranslationStatus::Translated);
    }
}
