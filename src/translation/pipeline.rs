//! Translation pipeline (spec §10–§17): translation memory pass → batched
//! AI requests with context and glossary → response validation → SQLite.
//!
//! Runs on a worker thread; `concurrency` inner workers drain the batch
//! queue; `cancel` stops work between batches (untouched entries simply
//! stay Pending).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use anyhow::Result;

use crate::ai::{RequestItem, TranslationProvider, TranslationRequest};
use crate::core::context::{ContextWindow, DialogueContext};
use crate::core::engine::GameEngine;
use crate::core::glossary::{self, GlossaryEntry};
use crate::core::project::Project;
use crate::core::translation::{TranslationEntry, TranslationStatus};
use crate::database::Db;

use super::memory;

// ------------------------------------------------------------------ config

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub source_language: String,
    pub target_language: String,
    pub batch_size: usize,
    pub concurrency: usize,
    pub context: ContextWindow,
    pub prompt_template: String,
    /// When set, translation memory is skipped for this run so every entry
    /// is freshly translated (bulk re-translate).
    pub ignore_memory: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            source_language: "English".into(),
            target_language: "Thai".into(),
            batch_size: 20,
            concurrency: 2,
            context: ContextWindow::default(),
            prompt_template: default_prompt_template().to_string(),
            ignore_memory: false,
        }
    }
}

impl PipelineConfig {
    /// Settings values are user-supplied; clamp them to sane bounds.
    pub fn clamped(mut self) -> Self {
        self.batch_size = self.batch_size.clamp(1, 50);
        self.concurrency = self.concurrency.clamp(1, 8);
        self.context.before = self.context.before.clamp(0, 10);
        self.context.after = self.context.after.clamp(0, 10);
        if self.prompt_template.trim().is_empty() {
            self.prompt_template = default_prompt_template().to_string();
        }
        self
    }
}

/// User-editable system prompt (Settings → Translation Prompt).
pub fn default_prompt_template() -> &'static str {
    "You are a professional video game localizer.\n\
     Translate the game text below from {source_language} to {target_language}.\n\
     \n\
     Style:\n\
     - Use a casual, friendly, conversational tone — like friends talking in everyday life.\n\
     - Keep it natural to say out loud; avoid stiff, literal, or formal phrasing unless the original is clearly formal.\n\
     \n\
     Rules:\n\
     - Preserve every placeholder and tag exactly as written, for example [player_name], {b}, {/b}, {color=#fff}, {w}, [[, {{.\n\
     - Obey the glossary whenever a glossary term appears.\n\
     - Do not add explanations, quotes, or extra formatting.\n\
     \n\
     Respond with JSON only, in exactly this shape:\n\
     {\"translations\":[{\"id\":\"<item id>\",\"text\":\"<translation>\"}]}\n\
     \n\
     {glossary}"
}

// ---------------------------------------------------------------- progress

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub translated: usize,
    pub failed: usize,
    pub from_memory: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub requested: usize,
    pub from_memory: usize,
    pub translated: usize,
    pub failed: usize,
    pub provider_errors: usize,
    /// Message from the most recent provider failure (network, 401, 404
    /// model-not-found, ...). Shown in the status bar.
    pub last_error: Option<String>,
    pub cancelled: bool,
}

/// Everything the pipeline needs. All borrowed; call it from one worker
/// thread (the UI never calls this directly).
pub struct PipelineParams<'a> {
    pub db: &'a Db,
    pub project: &'a Project,
    pub provider: &'a dyn TranslationProvider,
    pub engine: &'a dyn GameEngine,
    pub config: &'a PipelineConfig,
    pub cancel: &'a AtomicBool,
    pub on_progress: &'a (dyn Fn(Progress) + Sync),
}

/// Translate all Pending/Failed entries of the project.
pub fn translate_pending(params: PipelineParams<'_>) -> Result<Summary> {
    let entries = params.db.pending_entries(&params.project.id)?;
    Ok(translate_entries(params, entries))
}

/// Translate an explicit list of entries (used for "Re-translate" too).
pub fn translate_entries(params: PipelineParams<'_>, entries: Vec<TranslationEntry>) -> Summary {
    let config = params.config;
    let lang = params.project.target_language.as_str();
    let total = entries.len();
    let mut summary = Summary { requested: total, ..Summary::default() };

    if params.cancel.load(Ordering::Relaxed) {
        summary.cancelled = true;
        return summary;
    }

    // ---- Pass 1: translation memory (Glossary > TM > AI; exact hash hit
    // means the AI is never called for repeated text). Bulk re-translate
    // skips this pass so entries are freshly translated.
    let mut remaining: Vec<TranslationEntry> = Vec::new();
    for entry in entries {
        if params.cancel.load(Ordering::Relaxed) {
            summary.cancelled = true;
            break;
        }
        let memory_hit = if params.config.ignore_memory {
            None
        } else {
            memory::lookup(params.db, &entry.source.source_hash, lang).ok().flatten()
        };
        match memory_hit {
            Some(text) if !text.trim().is_empty() => {
                let _ = params.db.set_translation(
                    &entry.source.id,
                    Some(&text),
                    TranslationStatus::Translated,
                );
                summary.from_memory += 1;
            }
            _ => remaining.push(entry),
        }
    }
    report(params.on_progress, &summary, total);

    // ---- Pass 2: batched AI translation with bounded concurrency.
    let glossary = params
        .db
        .glossary_enabled(&params.project.id)
        .unwrap_or_default();
    let batches: VecDeque<Vec<TranslationEntry>> =
        remaining.chunks(config.batch_size.max(1)).map(<[TranslationEntry]>::to_vec).collect();
    let queue = Mutex::new(batches);
    let outcome = Mutex::new(summary.clone());

    std::thread::scope(|scope| {
        for _ in 0..config.concurrency {
            scope.spawn(|| loop {
                let Some(batch) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                if params.cancel.load(Ordering::Relaxed) {
                    // Leave the rest untouched (still Pending).
                    continue;
                }
                let produced = process_batch(&params, &glossary, &batch);
                let mut o = outcome.lock().unwrap();
                o.translated += produced.translated;
                o.failed += produced.failed;
                o.provider_errors += produced.provider_errors as usize;
                if let Some(message) = produced.error {
                    o.last_error = Some(message);
                }
                report(params.on_progress, &o, total);
            });
        }
    });

    let mut final_summary = outcome.lock().unwrap().clone();
    final_summary.from_memory = summary.from_memory;
    final_summary.requested = total;
    final_summary.cancelled = params.cancel.load(Ordering::Relaxed);
    final_summary
}

fn report(on_progress: &(dyn Fn(Progress) + Sync), summary: &Summary, total: usize) {
    on_progress(Progress {
        done: summary.from_memory + summary.translated + summary.failed,
        total,
        translated: summary.translated,
        failed: summary.failed,
        from_memory: summary.from_memory,
    });
}

struct BatchOutcome {
    translated: usize,
    failed: usize,
    provider_errors: bool,
    error: Option<String>,
}

fn process_batch(
    params: &PipelineParams<'_>,
    glossary: &[GlossaryEntry],
    batch: &[TranslationEntry],
) -> BatchOutcome {
    let config = params.config;
    let lang = params.project.target_language.as_str();

    // Build the batch request: context per item, glossary filtered to the
    // terms that actually occur in this batch (no blind dumps).
    let mut items = Vec::with_capacity(batch.len());
    for entry in batch {
        let context = build_context(params, entry);
        let context_str = {
            let formatted = context.format_prompt();
            if formatted.is_empty() { None } else { Some(formatted) }
        };
        items.push(RequestItem {
            id: entry.source.id.clone(),
            text: entry.source.source_text.clone(),
            context: context_str,
        });
    }
    let batch_text = items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>().join("\n");
    let active_glossary: Vec<&GlossaryEntry> = glossary::matches_for_text(glossary, &batch_text);
    let system_prompt = build_system_prompt(
        &config.prompt_template,
        &config.source_language,
        &config.target_language,
        &active_glossary,
    );

    let request = TranslationRequest { system_prompt, items };
    let response = match params.provider.translate(&request) {
        Ok(response) => response,
        Err(e) => {
            // Provider/network failure: entries stay Pending for a retry.
            return BatchOutcome {
                translated: 0,
                failed: 0,
                provider_errors: true,
                error: Some(format!("{e:#}")),
            };
        }
    };

    let by_id: HashMap<String, String> = response
        .translations
        .into_iter()
        .map(|t| (t.id, t.text))
        .collect();

    let mut outcome = BatchOutcome { translated: 0, failed: 0, provider_errors: false, error: None };
    let mut memory_rows: Vec<(String, String, String)> = Vec::new();
    for entry in batch {
        match by_id.get(&entry.source.id) {
            // Unknown response ids are dropped: only requested ids are read.
            Some(text) => {
                let tokens = params.engine.protected_tokens(&entry.source.source_text);
                let hits = glossary::matches_for_text(glossary, &entry.source.source_text);
                let violations = validate_item(&entry.source.source_text, text, &tokens, &hits);
                if violations.is_empty() {
                    let _ = params.db.set_translation(
                        &entry.source.id,
                        Some(text),
                        TranslationStatus::Translated,
                    );
                    memory_rows.push((
                        entry.source.source_hash.clone(),
                        entry.source.source_text.clone(),
                        text.clone(),
                    ));
                    outcome.translated += 1;
                } else {
                    // Validation failure: keep the attempt visible, status Failed.
                    let _ = params.db.set_translation(
                        &entry.source.id,
                        Some(text),
                        TranslationStatus::Failed,
                    );
                    outcome.failed += 1;
                }
            }
            _ => {
                // Missing id in the response (or an unexpected one that was
                // dropped): mark failed so the user can retry deliberately.
                let _ = params
                    .db
                    .set_translation(&entry.source.id, None, TranslationStatus::Failed);
                outcome.failed += 1;
            }
        }
    }
    let _ = memory::store(params.db, &memory_rows, lang);
    outcome
}

/// Context for one entry (spec §7/§8): speaker, scene, previous N / next N
/// texts from the same file.
pub fn build_context(params: &PipelineParams<'_>, entry: &TranslationEntry) -> DialogueContext {
    let window = params.config.context;
    let (previous, next) = params
        .db
        .neighbors(
            &params.project.id,
            &entry.source.file_path,
            entry.source.line,
            window.before as usize,
            window.after as usize,
        )
        .unwrap_or_default();
    DialogueContext {
        speaker: entry.source.speaker.clone(),
        scene: entry.source.context.clone(),
        previous,
        next,
    }
}

/// Fill the user template: `{source_language}`, `{target_language}`,
/// `{glossary}`. When the template has no `{glossary}` placeholder the
/// glossary block is appended, so terms are always sent.
pub fn build_system_prompt(
    template: &str,
    source_language: &str,
    target_language: &str,
    glossary_hits: &[&GlossaryEntry],
) -> String {
    let glossary_block = if glossary_hits.is_empty() {
        String::new()
    } else {
        let mut block = String::from("Glossary:\n");
        for entry in glossary_hits {
            block.push_str(&format!("{} = {}\n", entry.source, entry.target));
        }
        block.trim_end().to_string()
    };

    let mut prompt = template
        .replace("{source_language}", source_language)
        .replace("{target_language}", target_language);
    if prompt.contains("{glossary}") {
        prompt = prompt.replace("{glossary}", &glossary_block);
    } else if !glossary_block.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&glossary_block);
    }
    prompt
}

/// Validate one AI translation (spec §16). Returns the list of violations;
/// empty means the translation is accepted.
pub fn validate_item(
    _source_text: &str,
    translated: &str,
    protected_tokens: &[String],
    glossary_hits: &[&GlossaryEntry],
) -> Vec<String> {
    let mut violations = Vec::new();
    if translated.trim().is_empty() {
        violations.push("empty translation".to_string());
        return violations;
    }
    let lowered = translated.to_lowercase();
    for token in protected_tokens {
        if !lowered.contains(&token.to_lowercase()) {
            violations.push(format!("missing protected token {token}"));
        }
    }
    for hit in glossary_hits {
        if !lowered.contains(&hit.target.to_lowercase()) {
            violations.push(format!(
                "glossary term \"{}\" should be translated as \"{}\"",
                hit.source, hit.target
            ));
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::provider::{TranslatedItem, TranslationResponse};
    use crate::core::source::SourceEntry;
    use crate::database::Db;
    use crate::engine::renpy::RENPY_ENGINE;
    use std::sync::atomic::AtomicBool;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn seeded_db(texts: &[&str]) -> (Db, Project) {
        let db = db();
        let project = db
            .project_upsert(&Project::new("Game", "C:/games/x", "renpy"))
            .unwrap();
        let sources: Vec<SourceEntry> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| SourceEntry {
                id: format!("script.rpy|{}", i + 1),
                engine_id: "renpy".into(),
                file_path: "script.rpy".into(),
                line: (i + 1) as u32,
                speaker: Some("Eileen".into()),
                source_hash: crate::core::source::hash_text(text),
                source_text: text.to_string(),
                context: None,
            })
            .collect();
        db.scan_apply(
            &project,
            &crate::core::engine::ExtractionResult {
                sources,
                existing_translations: vec![],
            },
        )
        .unwrap();
        (db, project)
    }

    fn pending(db: &Db, project: &Project) -> Vec<TranslationEntry> {
        db.pending_entries(&project.id).unwrap()
    }

    fn params<'a>(
        db: &'a Db,
        project: &'a Project,
        provider: &'a dyn TranslationProvider,
        on_progress: &'a (dyn Fn(Progress) + Sync),
    ) -> PipelineParams<'a> {
        static CONFIG: std::sync::OnceLock<PipelineConfig> = std::sync::OnceLock::new();
        let config = CONFIG.get_or_init(|| PipelineConfig {
            batch_size: 2,
            concurrency: 2,
            ..PipelineConfig::default()
        });
        PipelineParams {
            db,
            project,
            provider,
            engine: &RENPY_ENGINE,
            config,
            cancel: &CANCEL,
            on_progress,
        }
    }

    static CANCEL: AtomicBool = AtomicBool::new(false);

    struct FakeProvider {
        respond: Box<dyn Fn(&TranslationRequest) -> Result<TranslationResponse> + Send + Sync>,
    }

    impl FakeProvider {
        fn ok_all() -> Self {
            Self {
                respond: Box::new(|req: &TranslationRequest| {
                    Ok(TranslationResponse {
                        translations: req
                            .items
                            .iter()
                            .map(|i| TranslatedItem {
                                id: i.id.clone(),
                                text: format!("แปล: {}", i.text),
                            })
                            .collect(),
                    })
                }),
            }
        }
    }

    impl TranslationProvider for FakeProvider {
        fn translate(&self, request: &TranslationRequest) -> Result<TranslationResponse> {
            (self.respond)(request)
        }

        fn list_models(&self) -> Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    fn noop(_: Progress) {}

    #[test]
    fn validation_catches_tokens_glossary_and_empty() {
        let glossary = vec![GlossaryEntry {
            id: "g".into(),
            project_id: "p".into(),
            source: "Guild".into(),
            target: "กิลด์".into(),
            note: None,
            enabled: true,
        }];
        let hits = glossary::matches_for_text(&glossary, "I go to the guild.");

        // Happy path.
        assert!(validate_item("I go to the guild.", "ฉันไปที่กิลด์", &[], &hits).is_empty());

        // Missing glossary term.
        let v = validate_item("I go to the guild.", "ฉันไปที่สมาคม", &[], &hits);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("glossary"));

        // Missing protected token.
        let tokens = vec!["[player_name]".to_string()];
        let v = validate_item("Hi [player_name]", "สวัสดี [ชื่อผู้เล่น]", &tokens, &[]);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("protected token"));

        // Empty.
        assert_eq!(validate_item("x", "   ", &[], &[]), vec!["empty translation"]);
    }

    #[test]
    fn system_prompt_fills_placeholders_and_glossary() {
        let g = GlossaryEntry {
            id: "g".into(),
            project_id: "p".into(),
            source: "Alice".into(),
            target: "อลิซ".into(),
            note: None,
            enabled: true,
        };
        let prompt = build_system_prompt(
            default_prompt_template(),
            "English",
            "Thai",
            &[&g],
        );
        assert!(prompt.contains("from English to Thai"));
        assert!(prompt.contains("Alice = อลิซ"));

        // Template without {glossary}: block is appended.
        let prompt = build_system_prompt("Translate to {target_language}.", "en", "ja", &[&g]);
        assert!(prompt.starts_with("Translate to ja."));
        assert!(prompt.contains("Alice = อลิซ"));
    }

    #[test]
    fn batch_translation_with_context_glossary_and_memory() {
        let (db, project) = seeded_db(&["I go to the guild.", "Hello", "Guild master"]);
        db.glossary_add(&project.id, "Guild", "กิลด์", None).unwrap();

        // Capture the last request so the context block can be asserted.
        let last_request = std::sync::Arc::new(Mutex::new(None::<TranslationRequest>));
        let capture = last_request.clone();
        let provider = FakeProvider {
            respond: Box::new(move |req: &TranslationRequest| {
                *capture.lock().unwrap() = Some(req.clone());
                Ok(TranslationResponse {
                    translations: req
                        .items
                        .iter()
                        .map(|i| TranslatedItem {
                            id: i.id.clone(),
                            text: if i.text.contains("Guild master") {
                                // Missing glossary term -> validation failure.
                                "หัวหน้า".to_string()
                            } else {
                                format!("ไทย: {} (Guild = กิลด์)", i.text)
                            },
                        })
                        .collect(),
                })
            }),
        };

        // One single batch so the captured request contains every item.
        let config = PipelineConfig {
            batch_size: 10,
            concurrency: 1,
            ..PipelineConfig::default()
        };
        let summary = translate_entries(
            PipelineParams {
                db: &db,
                project: &project,
                provider: &provider,
                engine: &RENPY_ENGINE,
                config: &config,
                cancel: &CANCEL,
                on_progress: &noop,
            },
            pending(&db, &project),
        );
        assert_eq!(
            (summary.requested, summary.translated, summary.failed, summary.from_memory),
            (3, 2, 1, 0)
        );

        // Context window contains the previous line.
        let req = last_request.lock().unwrap();
        let req = req.as_ref().unwrap();
        let second_item = req.items.iter().find(|i| i.text == "Hello").unwrap();
        let context = second_item.context.as_deref().unwrap();
        assert!(context.contains("Speaker: Eileen"));
        assert!(context.contains("Previous: I go to the guild."));
        assert!(context.contains("Next: Guild master"));

        // Failed entry keeps the attempt but is marked failed.
        let failed_id = format!("{}|script.rpy|3", project.id);
        let failed = db.source_by_id(&failed_id).unwrap().unwrap();
        assert_eq!(failed.status, TranslationStatus::Failed);
        assert_eq!(failed.translated_text.as_deref(), Some("หัวหน้า"));

        // Successful translations landed in memory: a brand-new entry with
        // the same text is prefilled by the next scan without AI.
        let report = db
            .scan_apply(
                &project,
                &crate::core::engine::ExtractionResult {
                    sources: vec![SourceEntry {
                        id: "other.rpy|1".into(),
                        engine_id: "renpy".into(),
                        file_path: "other.rpy".into(),
                        line: 1,
                        speaker: None,
                        source_hash: crate::core::source::hash_text("Hello"),
                        source_text: "Hello".into(),
                        context: None,
                    }],
                    existing_translations: vec![],
                },
            )
            .unwrap();
        assert_eq!(report.from_memory, 1);
    }

    #[test]
    fn provider_error_leaves_entries_pending() {
        let (db, project) = seeded_db(&["A", "B"]);
        let provider = FakeProvider {
            respond: Box::new(|_| Err(anyhow::anyhow!("network down"))),
        };
        let summary = translate_entries(params(&db, &project, &provider, &noop), pending(&db, &project));
        assert_eq!(summary.provider_errors, 1); // one batch of size 2
        assert_eq!(summary.translated + summary.failed, 0);
        assert_eq!(db.pending_entries(&project.id).unwrap().len(), 2);
    }

    #[test]
    fn translation_memory_pass_never_calls_the_provider() {
        let (db, project) = seeded_db(&["Hello"]);
        db.memory_put_many(
            &[(
                crate::core::source::hash_text("Hello"),
                "Hello".into(),
                "สวัสดี".into(),
            )],
            "Thai",
        )
        .unwrap();

        let provider = FakeProvider {
            respond: Box::new(|_| panic!("provider must not be called on TM hit")),
        };
        let summary = translate_entries(params(&db, &project, &provider, &noop), pending(&db, &project));
        assert_eq!((summary.from_memory, summary.translated, summary.failed), (1, 0, 0));
        let entry = db.source_by_id(&format!("{}|script.rpy|1", project.id)).unwrap().unwrap();
        assert_eq!(entry.translated_text.as_deref(), Some("สวัสดี"));
    }

    #[test]
    fn cancelled_run_touches_nothing() {
        let (db, project) = seeded_db(&["A", "B"]);
        let cancel = AtomicBool::new(true);
        let config = PipelineConfig::default();
        let provider = FakeProvider::ok_all();
        let p = PipelineParams {
            db: &db,
            project: &project,
            provider: &provider,
            engine: &RENPY_ENGINE,
            config: &config,
            cancel: &cancel,
            on_progress: &noop,
        };
        let summary = translate_entries(p, pending(&db, &project));
        assert!(summary.cancelled);
        assert_eq!(summary.requested, 2);
        assert_eq!(db.pending_entries(&project.id).unwrap().len(), 2);
    }

    #[test]
    fn config_clamping() {
        let config = PipelineConfig {
            batch_size: 500,
            concurrency: 99,
            prompt_template: "  ".into(),
            ..PipelineConfig::default()
        }
        .clamped();
        assert_eq!(config.batch_size, 50);
        assert_eq!(config.concurrency, 8);
        assert_eq!(config.prompt_template, default_prompt_template());
    }
}
