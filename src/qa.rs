//! Local QA scan: finds suspicious translations before export.
//! Deterministic checks only — no AI involved.

use crate::core::engine::GameEngine;
use crate::core::glossary;
use crate::database::Db;
use crate::translation::pipeline;

#[derive(Debug, Clone)]
pub struct QaIssue {
    pub source_id: String,
    pub file: String,
    pub line: u32,
    /// Short, user-facing description of the problem.
    pub label: String,
}

/// Scan every entry of a project for problems. Ordered by file/line.
pub fn scan(db: &Db, project_id: &str, engine: &dyn GameEngine) -> Vec<QaIssue> {
    let entries = db.all_entries(project_id).unwrap_or_default();
    let glossary_entries = db.glossary_enabled(project_id).unwrap_or_default();

    let mut issues = Vec::new();
    for entry in entries {
        let status = entry.status.as_str();
        let loc_file = entry.source.file_path.clone();
        let loc_line = entry.source.line;
        let mut push = |label: String| {
            issues.push(QaIssue {
                source_id: entry.source.id.clone(),
                file: loc_file.clone(),
                line: loc_line,
                label,
            });
        };

        if status == "failed" {
            push("translation failed — retry or edit".into());
            continue;
        }
        if status != "translated" && status != "edited" {
            continue; // pending: nothing to check
        }
        let Some(translation) = entry.translated_text.as_deref() else {
            continue;
        };

        // Token / glossary violations (same rules the pipeline enforces).
        let tokens = engine.protected_tokens(&entry.source.source_text);
        let hits = glossary::matches_for_text(&glossary_entries, &entry.source.source_text);
        let violations = pipeline::validate_item(
            &entry.source.source_text,
            translation,
            &tokens,
            &hits,
        );
        if !violations.is_empty() {
            push(format!("translation problem: {}", violations.join("; ")));
        }

        // Untranslated suspicion: identical to the source after trimming.
        if translation.trim() == entry.source.source_text.trim() {
            push("translation is identical to the source".into());
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::engine::ExtractionResult;
    use crate::core::project::Project;
    use crate::core::source::SourceEntry;
    use crate::database::Db;
    use crate::engine::renpy::RENPY_ENGINE;
    use crate::core::translation::TranslationStatus;

    fn db_with_entries() -> (Db, String) {
        let d = Db::open_in_memory().unwrap();
        let p = d.project_upsert(&Project::new("T", "C:/t", "renpy")).unwrap();
        let sources: Vec<SourceEntry> = vec![
            SourceEntry {
                id: "a.rpy|1".into(),
                engine_id: "renpy".into(),
                file_path: "a.rpy".into(),
                line: 1,
                speaker: None,
                source_text: "Hello [player_name]!".into(),
                source_hash: "h1".into(),
                context: None,
            },
            SourceEntry {
                id: "a.rpy|2".into(),
                engine_id: "renpy".into(),
                file_path: "a.rpy".into(),
                line: 2,
                speaker: None,
                source_text: "Just plain text.".into(),
                source_hash: "h2".into(),
                context: None,
            },
        ];
        d.scan_apply(&p, &ExtractionResult { sources, existing_translations: vec![] })
            .unwrap();
        (d, p.id)
    }

    #[test]
    fn flags_missing_tokens_and_same_as_source() {
        let (d, pid) = db_with_entries();
        d.set_translation(
            &format!("{pid}|a.rpy|1"),
            Some("Hello!"), // lost the [player_name] token
            TranslationStatus::Translated,
        )
        .unwrap();
        d.set_translation(
            &format!("{pid}|a.rpy|2"),
            Some("Just plain text."), // identical to source
            TranslationStatus::Translated,
        )
        .unwrap();

        let issues = scan(&d, &pid, &RENPY_ENGINE);
        let labels: Vec<&str> = issues.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.contains("player_name")), "{labels:?}");
        assert!(labels.iter().any(|l| l.contains("identical")), "{labels:?}");
        // Ordered by file/line.
        assert!(issues.windows(2).all(|w| w[0].line <= w[1].line));
    }

    #[test]
    fn failed_translations_are_reported() {
        let (d, pid) = db_with_entries();
        d.set_translation(
            &format!("{pid}|a.rpy|1"),
            None,
            TranslationStatus::Failed,
        )
        .unwrap();
        let issues = scan(&d, &pid, &RENPY_ENGINE);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].label.contains("failed"));
    }
}
