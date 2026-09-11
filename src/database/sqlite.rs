//! SQLite persistence: schema, repositories, and the incremental scan
//! apply-step. One `Connection` behind a `Mutex` — no global mutable state.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::core::engine::ExtractionResult;
use crate::core::glossary::GlossaryEntry;
use crate::core::project::Project;
use crate::core::source::SourceEntry;
use crate::core::translation::{TranslationEntry, TranslationStatus};

/// A named, saveable AI configuration (endpoint + key + model + temperature).
#[derive(Debug, Clone, PartialEq)]
pub struct AiProfile {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Result of a project scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport {
    pub total: usize,
    pub added: usize,
    pub changed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Prefilled from translation memory during the scan.
    pub from_memory: usize,
}

/// Live counts for the project screen.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProjectStats {
    pub total: i64,
    pub translated: i64,
    pub pending: i64,
    pub failed: i64,
}

// ----------------------------------------------------------- ai profiles

/// Settings keys that assign a profile to a purpose.
pub const PURPOSE_TRANSLATION: &str = "translation_profile_id";
pub const PURPOSE_GLOSSARY: &str = "glossary_profile_id";

pub struct Db {
    conn: Mutex<Connection>,
    #[allow(dead_code)]
    path: PathBuf,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), path: path.to_path_buf() })
    }

    /// Default per-user location: `<config>/translator-game-lite/data.db`.
    pub fn open_default() -> Result<Self> {
        let dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("translator-game-lite");
        Self::open(&dir.join("data.db"))
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn: Mutex::new(conn), path: PathBuf::from(":memory:") })
    }

    // ------------------------------------------------------------- projects

    /// Insert `project` unless a project with the same path exists; in that
    /// case the stored project is returned.
    pub fn project_upsert(&self, project: &Project) -> Result<Project> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<Project> = conn
            .query_row(
                "SELECT id, name, path, engine_id, source_language, target_language, created_at, updated_at
                 FROM projects WHERE path = ?1",
                params![project.path],
                row_to_project,
            )
            .optional()?;
        if let Some(stored) = existing {
            return Ok(stored);
        }
        conn.execute(
            "INSERT INTO projects (id, name, path, engine_id, source_language, target_language, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                project.id,
                project.name,
                project.path,
                project.engine_id,
                project.source_language,
                project.target_language,
                project.created_at,
                project.updated_at,
            ],
        )?;
        Ok(project.clone())
    }

    pub fn project_get(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, path, engine_id, source_language, target_language, created_at, updated_at
             FROM projects WHERE id = ?1",
            params![id],
            row_to_project,
        )
        .optional()
        .map_err(Into::into)
    }

    // -------------------------------------------------------- scan (sources)

    /// Incremental scan apply (spec §20/§21): match extracted sources against
    /// the DB by stable id, reuse translations for unchanged hashes, reset
    /// changed ones to Pending, prefill new ones from translation memory.
    pub fn scan_apply(&self, project: &Project, extraction: &ExtractionResult) -> Result<ScanReport> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let pid = &project.id;
        let lang = &project.target_language;
        let now = crate::core::project::now_unix();

        let mut existing_hash: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT id, source_hash FROM sources WHERE project_id = ?1")?;
            let mut rows = stmt.query(params![pid])?;
            while let Some(row) = rows.next()? {
                existing_hash.insert(row.get::<_, String>(0)?, row.get::<_, String>(1)?);
            }
        }

        let mut report = ScanReport { total: extraction.sources.len(), ..Default::default() };
        let mut seen: HashSet<String> = HashSet::with_capacity(extraction.sources.len());

        {
            let mut tm_stmt = tx.prepare(
                "SELECT translated_text FROM translation_memory WHERE source_hash = ?1 AND target_language = ?2",
            )?;
            let mut upsert_source = tx.prepare(
                "INSERT INTO sources (id, project_id, engine_id, file_path, line, speaker, source_text, source_hash, context)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    engine_id=excluded.engine_id, file_path=excluded.file_path, line=excluded.line,
                    speaker=excluded.speaker, source_text=excluded.source_text,
                    source_hash=excluded.source_hash, context=excluded.context",
            )?;
            let mut insert_translation = tx.prepare(
                "INSERT OR REPLACE INTO translations (source_id, translated_text, status, updated_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;

            for source in &extraction.sources {
                let id = format!("{}|{}", pid, source.id);
                seen.insert(id.clone());

                match existing_hash.get(&id) {
                    Some(old_hash) if old_hash == &source.source_hash => {
                        // Unchanged text: keep location/speaker/context fresh,
                        // keep the existing translation untouched.
                        upsert_source.execute(params![
                            id, pid, source.engine_id, source.file_path, source.line,
                            source.speaker, source.source_text, source.source_hash, source.context,
                        ])?;
                        report.unchanged += 1;
                    }
                    _ => {
                        upsert_source.execute(params![
                            id, pid, source.engine_id, source.file_path, source.line,
                            source.speaker, source.source_text, source.source_hash, source.context,
                        ])?;
                        // Changed or brand-new: reset, then try translation memory.
                        let remembered: Option<String> = tm_stmt
                            .query_row(params![source.source_hash, lang], |row| row.get(0))
                            .optional()?;
                        let (text, status) = match remembered {
                            Some(text) => {
                                report.from_memory += 1;
                                (Some(text), TranslationStatus::Translated)
                            }
                            None => (None, TranslationStatus::Pending),
                        };
                        insert_translation.execute(params![id, text, status.as_str(), now])?;
                        if existing_hash.contains_key(&id) {
                            report.changed += 1;
                        } else {
                            report.added += 1;
                        }
                    }
                }
            }
        }

        // Sources that disappeared from the game are removed (with their
        // translation rows, via ON DELETE CASCADE).
        let removed: Vec<String> = existing_hash
            .keys()
            .filter(|id| !seen.contains(*id))
            .cloned()
            .collect();
        report.removed = removed.len();
        {
            let mut stmt = tx.prepare("DELETE FROM sources WHERE id = ?1")?;
            for id in &removed {
                stmt.execute(params![id])?;
            }
        }

        // Engine-provided translations (Ren'Py `new "..."` lines) fill any
        // entry that is still pending.
        {
            let mut stmt = tx.prepare(
                "UPDATE translations SET translated_text = ?2, status = 'edited', updated_at = ?3
                 WHERE source_id = ?1 AND status = 'pending'",
            )?;
            for existing in &extraction.existing_translations {
                let id = format!("{}|{}", pid, existing.source_id);
                stmt.execute(params![id, existing.text, now])?;
            }
        }

        tx.commit()?;
        Ok(report)
    }

    // ------------------------------------------------- sources & translations

    /// One page of the entry list (joined source + translation), ordered by
    /// file and line.
    pub fn sources_page(
        &self,
        project_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TranslationEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.engine_id, s.file_path, s.line, s.speaker, s.source_text, s.source_hash, s.context,
                    t.translated_text, t.status, t.updated_at
             FROM sources s LEFT JOIN translations t ON t.source_id = s.id
             WHERE s.project_id = ?1
             ORDER BY s.file_path, s.line
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map(params![project_id, limit as i64, offset as i64], row_to_translation_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn source_by_id(&self, id: &str) -> Result<Option<TranslationEntry>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT s.id, s.engine_id, s.file_path, s.line, s.speaker, s.source_text, s.source_hash, s.context,
                    t.translated_text, t.status, t.updated_at
             FROM sources s LEFT JOIN translations t ON t.source_id = s.id
             WHERE s.id = ?1",
            params![id],
            row_to_translation_entry,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Entries eligible for automatic translation (Pending or Failed).
    pub fn pending_entries(&self, project_id: &str) -> Result<Vec<TranslationEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.engine_id, s.file_path, s.line, s.speaker, s.source_text, s.source_hash, s.context,
                    t.translated_text, t.status, t.updated_at
             FROM sources s LEFT JOIN translations t ON t.source_id = s.id
             WHERE s.project_id = ?1 AND (t.status IS NULL OR t.status IN ('pending', 'failed'))
             ORDER BY s.file_path, s.line",
        )?;
        let rows = stmt
            .query_map(params![project_id], row_to_translation_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All entries with an accepted translation — the export input.
    pub fn export_entries(&self, project_id: &str) -> Result<Vec<TranslationEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.engine_id, s.file_path, s.line, s.speaker, s.source_text, s.source_hash, s.context,
                    t.translated_text, t.status, t.updated_at
             FROM sources s LEFT JOIN translations t ON t.source_id = s.id
             WHERE s.project_id = ?1 AND t.status IN ('translated', 'edited')
               AND t.translated_text IS NOT NULL AND t.translated_text != ''
             ORDER BY s.file_path, s.line",
        )?;
        let rows = stmt
            .query_map(params![project_id], row_to_translation_entry)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn stats(&self, project_id: &str) -> Result<ProjectStats> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(s.id),
                    COALESCE(SUM(CASE WHEN t.status IN ('translated','edited') THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.status = 'pending' OR t.status IS NULL THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN t.status = 'failed' THEN 1 ELSE 0 END), 0)
             FROM sources s LEFT JOIN translations t ON t.source_id = s.id
             WHERE s.project_id = ?1",
            params![project_id],
            |row| {
                Ok(ProjectStats {
                    total: row.get(0)?,
                    translated: row.get(1)?,
                    pending: row.get(2)?,
                    failed: row.get(3)?,
                })
            },
        )
        .map_err(Into::into)
    }

    /// Store a translation with an explicit status (manual edit, AI result,
    /// failed attempt, ...).
    pub fn set_translation(
        &self,
        source_id: &str,
        text: Option<&str>,
        status: TranslationStatus,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO translations (source_id, translated_text, status, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(source_id) DO UPDATE SET
                translated_text=excluded.translated_text, status=excluded.status,
                updated_at=excluded.updated_at",
            params![source_id, text, status.as_str(), crate::core::project::now_unix()],
        )?;
        Ok(())
    }

    /// Previous/next source texts of the same file for the context window.
    pub fn neighbors(
        &self,
        project_id: &str,
        file_path: &str,
        line: u32,
        before: usize,
        after: usize,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let conn = self.conn.lock().unwrap();
        let mut prev_stmt = conn.prepare(
            "SELECT source_text FROM sources
             WHERE project_id = ?1 AND file_path = ?2 AND line < ?3
             ORDER BY line DESC LIMIT ?4",
        )?;
        let mut previous: Vec<String> = prev_stmt
            .query_map(params![project_id, file_path, line, before as i64], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        previous.reverse();

        let mut next_stmt = conn.prepare(
            "SELECT source_text FROM sources
             WHERE project_id = ?1 AND file_path = ?2 AND line > ?3
             ORDER BY line ASC LIMIT ?4",
        )?;
        let next: Vec<String> = next_stmt
            .query_map(params![project_id, file_path, line, after as i64], |r| r.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok((previous, next))
    }

    // -------------------------------------------------------------- glossary

    pub fn glossary_add(
        &self,
        project_id: &str,
        source: &str,
        target: &str,
        note: Option<&str>,
    ) -> Result<GlossaryEntry> {
        let entry = GlossaryEntry {
            id: crate::core::project::new_id(),
            project_id: project_id.to_string(),
            source: source.trim().to_string(),
            target: target.trim().to_string(),
            note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            enabled: true,
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO glossary (id, project_id, source_term, target_term, note, enabled)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![entry.id, entry.project_id, entry.source, entry.target, entry.note, entry.enabled],
        )?;
        Ok(entry)
    }

    pub fn glossary_update(&self, entry: &GlossaryEntry) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE glossary SET source_term = ?2, target_term = ?3, note = ?4, enabled = ?5 WHERE id = ?1",
            params![entry.id, entry.source, entry.target, entry.note, entry.enabled],
        )?;
        Ok(())
    }

    pub fn glossary_delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM glossary WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn glossary_get(&self, id: &str) -> Result<Option<GlossaryEntry>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, project_id, source_term, target_term, note, enabled
             FROM glossary WHERE id = ?1",
            params![id],
            row_to_glossary,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn glossary_list(&self, project_id: &str, search: &str) -> Result<Vec<GlossaryEntry>> {
        let conn = self.conn.lock().unwrap();
        let pattern = format!("%{}%", search.trim());
        let mut stmt = conn.prepare(
            "SELECT id, project_id, source_term, target_term, note, enabled
             FROM glossary
             WHERE project_id = ?1
               AND (?2 = '%%' OR source_term LIKE ?2 COLLATE NOCASE OR target_term LIKE ?2 COLLATE NOCASE)
             ORDER BY source_term COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map(params![project_id, pattern], row_to_glossary)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn glossary_enabled(&self, project_id: &str) -> Result<Vec<GlossaryEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, project_id, source_term, target_term, note, enabled
             FROM glossary WHERE project_id = ?1 AND enabled = 1
             ORDER BY source_term COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map(params![project_id], row_to_glossary)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---------------------------------------------------- translation memory

    pub fn memory_get(&self, source_hash: &str, target_language: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT translated_text FROM translation_memory WHERE source_hash = ?1 AND target_language = ?2",
            params![source_hash, target_language],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    /// Batch store of finished translations (prepared statement + transaction).
    pub fn memory_put_many(
        &self,
        rows: &[(String, String, String)], // (source_hash, source_text, translated_text)
        target_language: &str,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO translation_memory (source_hash, source_text, target_language, translated_text)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (hash, source_text, translated) in rows {
                stmt.execute(params![hash, source_text, target_language, translated])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------- settings

    pub fn setting_get(&self, key: &str) -> Result<Option<String>> {        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn setting_set(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn setting_get_or(&self, key: &str, default: &str) -> Result<String> {
        Ok(self.setting_get(key)?.unwrap_or_else(|| default.to_string()))
    }

    // ----------------------------------------------------------- ai profiles

    pub fn ai_profile_list(&self) -> Result<Vec<AiProfile>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, endpoint, api_key, model, temperature, created_at, updated_at
             FROM ai_profiles ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt
            .query_map([], row_to_ai_profile)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    #[allow(dead_code)] // repository API; currently used by tests
    pub fn ai_profile_get(&self, id: &str) -> Result<Option<AiProfile>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, name, endpoint, api_key, model, temperature, created_at, updated_at
             FROM ai_profiles WHERE id = ?1",
            params![id],
            row_to_ai_profile,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Insert or update a profile. Duplicate names (on a *different* id)
    /// return an error the UI can show.
    pub fn ai_profile_upsert(&self, profile: &AiProfile) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ai_profiles (id, name, endpoint, api_key, model, temperature, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO UPDATE SET
                name=excluded.name, endpoint=excluded.endpoint, api_key=excluded.api_key,
                model=excluded.model, temperature=excluded.temperature, updated_at=excluded.updated_at",
            params![
                profile.id,
                profile.name,
                profile.endpoint,
                profile.api_key,
                profile.model,
                profile.temperature,
                profile.created_at,
                profile.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn ai_profile_delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM ai_profiles WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// One-time-per-launch cleanup: normalize whitespace in stored
    /// translations (older runs could save stray/filler spaces).
    pub fn cleanup_translations(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT source_id, translated_text FROM translations WHERE translated_text IS NOT NULL",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        for (id, text) in rows {
            let clean = crate::core::source::clean_spaces(&text);
            if clean != text {
                conn.execute(
                    "UPDATE translations SET translated_text = ?2 WHERE source_id = ?1",
                    params![id, clean],
                )?;
            }
        }
        Ok(())
    }

    /// First-run migration: with no profiles saved, turn the legacy flat
    /// settings (api_endpoint/api_key/model) into a "Default" profile and
    /// assign it to every purpose. Also migrates the old
    /// `active_profile_translation` pointer.
    pub fn ai_profile_ensure_default(&self) -> Result<()> {
        if self.ai_profile_list()?.is_empty() {
            let now = crate::core::project::now_unix();
            let profile = AiProfile {
                id: crate::core::project::new_id(),
                name: "Default".to_string(),
                endpoint: self.setting_get_or("api_endpoint", "https://api.openai.com/v1")?,
                api_key: self.setting_get("api_key")?.unwrap_or_default(),
                model: self.setting_get("model")?.unwrap_or_default(),
                temperature: 0.3,
                created_at: now,
                updated_at: now,
            };
            self.ai_profile_upsert(&profile)?;
        }
        // Purpose pointers: migrate the legacy active pointer, else point at
        // the first profile.
        let first = self.ai_profile_list()?.first().map(|p| p.id.clone());
        if let Some(first) = first {
            if self.setting_get(PURPOSE_TRANSLATION)?.is_none() {
                let legacy =
                    self.setting_get("active_profile_translation")?.unwrap_or_default();
                let id = if legacy.is_empty() { first.clone() } else { legacy };
                self.setting_set(PURPOSE_TRANSLATION, &id)?;
            }
            if self.setting_get(PURPOSE_GLOSSARY)?.is_none() {
                self.setting_set(PURPOSE_GLOSSARY, &first)?;
            }
        }
        Ok(())
    }

    /// The profile assigned to a purpose ("translation_profile_id" /
    /// "glossary_profile_id"), falling back to the first one.
    pub fn ai_profile_for_purpose(&self, purpose_key: &str) -> Result<AiProfile> {
        self.ai_profile_ensure_default()?;
        let profiles = self.ai_profile_list()?;
        let want = self.setting_get(purpose_key)?.unwrap_or_default();
        profiles
            .iter()
            .find(|p| p.id == want)
            .or_else(|| profiles.first())
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no AI profile exists"))
    }
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        engine_id: row.get(3)?,
        source_language: row.get(4)?,
        target_language: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn row_to_glossary(row: &rusqlite::Row<'_>) -> rusqlite::Result<GlossaryEntry> {
    Ok(GlossaryEntry {
        id: row.get(0)?,
        project_id: row.get(1)?,
        source: row.get(2)?,
        target: row.get(3)?,
        note: row.get(4)?,
        enabled: row.get::<_, i64>(5)? != 0,
    })
}

fn row_to_translation_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<TranslationEntry> {
    let source = SourceEntry {
        id: row.get(0)?,
        engine_id: row.get(1)?,
        file_path: row.get(2)?,
        line: row.get::<_, i64>(3)? as u32,
        speaker: row.get(4)?,
        source_text: row.get(5)?,
        source_hash: row.get(6)?,
        context: row.get(7)?,
    };
    Ok(TranslationEntry {
        source,
        translated_text: row.get(8)?,
        status: row
            .get::<_, Option<String>>(9)?
            .map(|s| TranslationStatus::from_str(&s))
            .unwrap_or(TranslationStatus::Pending),
        updated_at: row.get::<_, Option<i64>>(10)?.unwrap_or(0),
    })
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // journal_mode returns a row, so it must go through query_row.
    let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            path TEXT NOT NULL,
            engine_id TEXT NOT NULL,
            source_language TEXT NOT NULL,
            target_language TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_projects_path ON projects(path);

        CREATE TABLE IF NOT EXISTS sources (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            engine_id TEXT NOT NULL,
            file_path TEXT NOT NULL,
            line INTEGER NOT NULL,
            speaker TEXT,
            source_text TEXT NOT NULL,
            source_hash TEXT NOT NULL,
            context TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_sources_project_file ON sources(project_id, file_path, line);
        CREATE INDEX IF NOT EXISTS idx_sources_hash ON sources(source_hash);

        CREATE TABLE IF NOT EXISTS translations (
            source_id TEXT PRIMARY KEY REFERENCES sources(id) ON DELETE CASCADE,
            translated_text TEXT,
            status TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS glossary (
            id TEXT PRIMARY KEY,
            project_id TEXT,
            source_term TEXT NOT NULL,
            target_term TEXT NOT NULL,
            note TEXT,
            enabled INTEGER NOT NULL DEFAULT 1
        );
        CREATE INDEX IF NOT EXISTS idx_glossary_project ON glossary(project_id);

        CREATE TABLE IF NOT EXISTS translation_memory (
            source_hash TEXT NOT NULL,
            source_text TEXT NOT NULL,
            target_language TEXT NOT NULL,
            translated_text TEXT NOT NULL,
            PRIMARY KEY (source_hash, target_language)
        );

        CREATE TABLE IF NOT EXISTS ai_profiles (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            endpoint TEXT NOT NULL,
            api_key TEXT NOT NULL DEFAULT '',
            model TEXT NOT NULL DEFAULT '',
            temperature REAL NOT NULL DEFAULT 0.3,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    // Older databases created ai_profiles before the temperature column.
    let _ = conn.execute(
        "ALTER TABLE ai_profiles ADD COLUMN temperature REAL NOT NULL DEFAULT 0.3",
        [],
    );
    Ok(())
}

fn row_to_ai_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiProfile> {
    Ok(AiProfile {
        id: row.get(0)?,
        name: row.get(1)?,
        endpoint: row.get(2)?,
        api_key: row.get(3)?,
        model: row.get(4)?,
        temperature: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::translation::TranslationStatus;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn project() -> Project {
        Project::new("Test Game", "C:/games/test", "renpy")
    }

    fn source(local_id: &str, text: &str, line: u32) -> SourceEntry {
        SourceEntry {
            id: local_id.to_string(),
            engine_id: "renpy".into(),
            file_path: "script.rpy".into(),
            line,
            speaker: Some("Eileen".into()),
            source_hash: crate::core::source::hash_text(text),
            source_text: text.into(),
            context: Some("start".into()),
        }
    }

    #[test]
    fn schema_migrates_twice() {
        let d = db();
        // Any query exercising the migrated schema.
        assert!(d.setting_get("nope").is_ok());
    }

    #[test]
    fn project_upsert_is_by_path() {
        let d = db();
        let p = project();
        let inserted = d.project_upsert(&p).unwrap();
        let again = d.project_upsert(&Project::new("Other Name", &p.path, "renpy")).unwrap();
        assert_eq!(inserted.id, again.id);
        assert_eq!(again.name, "Test Game");
    }

    #[test]
    fn scan_apply_full_lifecycle() {
        let d = db();
        let p = d.project_upsert(&project()).unwrap();

        // Initial scan: 3 entries.
        let v1 = ExtractionResult {
            sources: vec![source("script.rpy|1", "Hello", 1), source("script.rpy|2", "Bye", 2), source("script.rpy|5", "Fine.", 5)],
            existing_translations: vec![],
        };
        let report = d.scan_apply(&p, &v1).unwrap();
        assert_eq!(
            (report.total, report.added, report.unchanged, report.changed, report.removed),
            (3, 3, 0, 0, 0)
        );
        let stats = d.stats(&p.id).unwrap();
        assert_eq!((stats.total, stats.pending), (3, 3));

        // Second identical scan: everything unchanged, translations kept.
        let report = d.scan_apply(&p, &v1).unwrap();
        assert_eq!((report.added, report.unchanged, report.changed, report.removed), (0, 3, 0, 0));

        // Manual edit survives a rescan of the same content.
        let id1 = format!("{}|script.rpy|1", p.id);
        d.set_translation(&id1, Some("สวัสดี"), TranslationStatus::Edited).unwrap();
        d.scan_apply(&p, &v1).unwrap();
        let entry = d.source_by_id(&id1).unwrap().unwrap();
        assert_eq!(entry.status, TranslationStatus::Edited);
        assert_eq!(entry.translated_text.as_deref(), Some("สวัสดี"));

        // Text changes at the same location: reset to Pending.
        let v2 = ExtractionResult {
            sources: vec![source("script.rpy|1", "Hello there", 1), source("script.rpy|2", "Bye", 2), source("script.rpy|5", "Fine.", 5)],
            existing_translations: vec![],
        };
        let report = d.scan_apply(&p, &v2).unwrap();
        assert_eq!(report.changed, 1);
        let entry = d.source_by_id(&id1).unwrap().unwrap();
        assert_eq!(entry.status, TranslationStatus::Pending);
        assert!(entry.translated_text.is_none());

        // Removed sources are deleted.
        let v3 = ExtractionResult {
            sources: vec![source("script.rpy|2", "Bye", 2)],
            existing_translations: vec![],
        };
        let report = d.scan_apply(&p, &v3).unwrap();
        assert_eq!(report.removed, 2);
        assert_eq!(d.stats(&p.id).unwrap().total, 1);
    }

    #[test]
    fn scan_prefills_from_translation_memory_and_engine_new_lines() {
        let d = db();
        let p = d.project_upsert(&project()).unwrap();

        d.memory_put_many(
            &[(
                crate::core::source::hash_text("Hello"),
                "Hello".to_string(),
                "สวัสดี".to_string(),
            )],
            &p.target_language,
        )
        .unwrap();

        let v1 = ExtractionResult {
            sources: vec![source("tl/x.rpy|3", "Hello", 3), source("tl/x.rpy|4", "Good night", 4)],
            existing_translations: vec![crate::core::engine::ExistingTranslation {
                source_id: "tl/x.rpy|4".into(),
                text: "ราตรีสวัสดิ์".into(),
            }],
        };
        let report = d.scan_apply(&p, &v1).unwrap();
        assert_eq!(report.from_memory, 1);

        let mem = d
            .source_by_id(&format!("{}|tl/x.rpy|3", p.id))
            .unwrap()
            .unwrap();
        assert_eq!(mem.status, TranslationStatus::Translated);
        assert_eq!(mem.translated_text.as_deref(), Some("สวัสดี"));

        let engine_import = d
            .source_by_id(&format!("{}|tl/x.rpy|4", p.id))
            .unwrap()
            .unwrap();
        assert_eq!(engine_import.status, TranslationStatus::Edited);
        assert_eq!(engine_import.translated_text.as_deref(), Some("ราตรีสวัสดิ์"));
    }

    #[test]
    fn pagination_and_pending_queries() {
        let d = db();
        let p = d.project_upsert(&project()).unwrap();
        let sources: Vec<SourceEntry> = (0..10)
            .map(|i| source(&format!("script.rpy|{}", i + 1), &format!("Line {}", i), i + 1))
            .collect();
        d.scan_apply(&p, &ExtractionResult { sources, existing_translations: vec![] })
            .unwrap();

        let page0 = d.sources_page(&p.id, 0, 4).unwrap();
        let page2 = d.sources_page(&p.id, 8, 4).unwrap();
        assert_eq!(page0.len(), 4);
        assert_eq!(page2.len(), 2);
        assert_eq!(page0[0].source.source_text, "Line 0");

        d.set_translation(
            &format!("{}|script.rpy|1", p.id),
            Some("แปลแล้ว"),
            TranslationStatus::Translated,
        )
        .unwrap();
        d.set_translation(
            &format!("{}|script.rpy|2", p.id),
            Some("พลาด"),
            TranslationStatus::Failed,
        )
        .unwrap();

        let pending = d.pending_entries(&p.id).unwrap();
        // 10 entries minus the one translated; the Failed entry is still
        // retryable, so it counts as pending too.
        assert_eq!(pending.len(), 9);
        assert!(pending.iter().all(|e| e.source.source_text != "Line 0"));

        let stats = d.stats(&p.id).unwrap();
        assert_eq!(
            (stats.total, stats.translated, stats.pending, stats.failed),
            (10, 1, 8, 1)
        );
    }

    #[test]
    fn neighbors_respect_window_and_order() {
        let d = db();
        let p = d.project_upsert(&project()).unwrap();
        let sources: Vec<SourceEntry> = (0..5)
            .map(|i| source(&format!("script.rpy|{}", i + 1), &format!("L{}", i), i + 1))
            .collect();
        d.scan_apply(&p, &ExtractionResult { sources, existing_translations: vec![] })
            .unwrap();

        let (prev, next) = d.neighbors(&p.id, "script.rpy", 3, 2, 1).unwrap();
        assert_eq!(prev, vec!["L0", "L1"]);
        assert_eq!(next, vec!["L3"]);
    }

    #[test]
    fn glossary_crud_and_search() {
        let d = db();
        let p = d.project_upsert(&project()).unwrap();

        let alice = d.glossary_add(&p.id, "  Alice ", " อลิซ ", Some("Character")).unwrap();
        assert_eq!(alice.source, "Alice");
        assert_eq!(alice.target, "อลิซ");
        let guild = d.glossary_add(&p.id, "Guild", "กิลด์", None).unwrap();

        let mut master = d.glossary_add(&p.id, "Master", "นายท่าน", None).unwrap();
        master.enabled = false;
        master.target = "ท่านอาจารย์".into();
        d.glossary_update(&master).unwrap();

        assert_eq!(d.glossary_list(&p.id, "").unwrap().len(), 3);
        assert_eq!(d.glossary_list(&p.id, "guild").unwrap().len(), 1);
        assert_eq!(d.glossary_enabled(&p.id).unwrap().len(), 2);

        d.glossary_delete(&guild.id).unwrap();
        assert_eq!(d.glossary_list(&p.id, "").unwrap().len(), 2);
        let disabled = d.glossary_list(&p.id, "Master").unwrap().remove(0);
        assert!(!disabled.enabled);
        assert_eq!(disabled.target, "ท่านอาจารย์");
    }

    #[test]
    fn memory_roundtrip() {
        let d = db();
        assert_eq!(d.memory_get("h1", "Thai").unwrap(), None);
        d.memory_put_many(&[("h1".into(), "Hello".into(), "สวัสดี".into())], "Thai").unwrap();
        assert_eq!(d.memory_get("h1", "Thai").unwrap().as_deref(), Some("สวัสดี"));
        // Different language does not hit.
        assert_eq!(d.memory_get("h1", "Japanese").unwrap(), None);
        // Upsert overwrites.
        d.memory_put_many(&[("h1".into(), "Hello".into(), "ฮัลโหล".into())], "Thai").unwrap();
        assert_eq!(d.memory_get("h1", "Thai").unwrap().as_deref(), Some("ฮัลโหล"));
    }

    #[test]
    fn ai_profiles_crud_and_default_migration() {
        let d = db();
        d.setting_set("api_endpoint", "https://ollama.com/v1").unwrap();
        d.setting_set("api_key", "sk-x").unwrap();
        d.setting_set("model", "gemma4:31b").unwrap();

        // First run: legacy settings become the "Default" profile.
        d.ai_profile_ensure_default().unwrap();
        let list = d.ai_profile_list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Default");
        assert_eq!(list[0].endpoint, "https://ollama.com/v1");
        assert_eq!(list[0].api_key, "sk-x");
        assert_eq!(list[0].model, "gemma4:31b");
        assert_eq!(
            d.setting_get("translation_profile_id").unwrap().as_deref(),
            Some(list[0].id.as_str())
        );
        assert_eq!(
            d.setting_get("glossary_profile_id").unwrap().as_deref(),
            Some(list[0].id.as_str())
        );

        // Idempotent.
        d.ai_profile_ensure_default().unwrap();
        assert_eq!(d.ai_profile_list().unwrap().len(), 1);

        // Edit / rename via upsert.
        let mut p = list[0].clone();
        p.name = "Ollama".into();
        p.model = "gpt-oss:120b-cloud".into();
        d.ai_profile_upsert(&p).unwrap();
        let got = d.ai_profile_get(&p.id).unwrap().unwrap();
        assert_eq!(got.name, "Ollama");
        assert_eq!(got.model, "gpt-oss:120b-cloud");

        // Duplicate name on a different id is rejected.
        let dup = AiProfile {
            id: "other-id".into(),
            name: "Ollama".into(),
            endpoint: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: String::new(),
            temperature: 0.3,
            created_at: 0,
            updated_at: 0,
        };
        assert!(d.ai_profile_upsert(&dup).is_err());

        // Purpose lookup falls back to the first profile.
        let for_translation = d.ai_profile_for_purpose("translation_profile_id").unwrap();
        assert_eq!(for_translation.id, p.id);

        let second = AiProfile {
            id: "second".into(),
            name: "Second".into(),
            endpoint: "https://api.openai.com/v1".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            temperature: 0.3,
            created_at: 0,
            updated_at: 0,
        };
        d.ai_profile_upsert(&second).unwrap();

        // Assigning a purpose picks exactly that profile.
        d.setting_set("glossary_profile_id", "second").unwrap();
        assert_eq!(
            d.ai_profile_for_purpose("glossary_profile_id").unwrap().id,
            "second"
        );
        assert_eq!(
            d.ai_profile_for_purpose("translation_profile_id").unwrap().id,
            p.id
        );

        // Delete; the purpose pointer falls back to a remaining profile.
        d.ai_profile_delete(&p.id).unwrap();
        assert!(d.ai_profile_get(&p.id).unwrap().is_none());
        assert_eq!(
            d.ai_profile_for_purpose("translation_profile_id").unwrap().id,
            "second"
        );
    }

    #[test]
    fn settings_roundtrip() {
        let d = db();
        assert_eq!(d.setting_get("model").unwrap(), None);
        assert_eq!(d.setting_get_or("model", "gpt-4o-mini").unwrap(), "gpt-4o-mini");
        d.setting_set("model", "gpt-4.1-mini").unwrap();
        d.setting_set("model", "gpt-4o").unwrap();
        assert_eq!(d.setting_get("model").unwrap().as_deref(), Some("gpt-4o"));
    }
}
