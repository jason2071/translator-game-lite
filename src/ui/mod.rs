//! Slint <-> Rust glue.
//!
//! The UI thread only performs quick SQLite reads for the visible page and
//! receives results from background workers via
//! [`slint::Weak::upgrade_in_event_loop`]. Scan / translate / export all
//! run on plain threads and are cancelable.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::ai::{
    OpenAiCompatibleProvider, ProviderConfig, TranslationProvider as _,
};
use crate::core::context::ContextWindow;
use crate::core::engine::{detect_engine, engine_display_name, registry, GameEngine};
use crate::core::glossary;
use crate::core::project::Project;
use crate::core::scan::scan_project;
use crate::core::translation::TranslationStatus;
use crate::database::Db;
use crate::translation::pipeline;

slint::include_modules!();

/// How many entries the list loads per page (pagination keeps RAM flat).
const PAGE_SIZE: usize = 200;

pub fn run(db: Arc<Db>) -> Result<()> {
    let ui = AppWindow::new()?;
    let cancel = Arc::new(AtomicBool::new(false));

    load_settings(&ui, &db);
    if let Some(project) = current_project(&db) {
        apply_project(&ui, &project);
        if refresh_stats(&ui, &db, &project).is_ok() {
            refresh_entries(&ui, &db, &project);
        }
        refresh_glossary(&ui, &db);
    }
    wire_callbacks(&ui, db, cancel);
    ui.run()?;
    Ok(())
}

// ------------------------------------------------------------ project/state

fn current_project(db: &Db) -> Option<Project> {
    let id = db.setting_get("current_project_id").ok()??;
    db.project_get(&id).ok().flatten()
}

fn engine_for(project: &Project) -> Option<&'static dyn GameEngine> {
    registry().iter().copied().find(|e| e.id() == project.engine_id)
}

fn apply_project(ui: &AppWindow, project: &Project) {
    ui.set_project_path(project.path.clone().into());
    ui.set_engine_name(engine_display_name(&project.engine_id).into());
    ui.set_set_source_lang(project.source_language.clone().into());
    ui.set_set_target_lang(project.target_language.clone().into());
}

// ------------------------------------------------------------------ refresh

fn refresh_stats(ui: &AppWindow, db: &Db, project: &Project) -> Result<()> {
    let stats = db.stats(&project.id)?;
    ui.set_stat_total(stats.total as i32);
    ui.set_stat_translated(stats.translated as i32);
    ui.set_stat_pending(stats.pending as i32);
    ui.set_stat_failed(stats.failed as i32);
    let page = PAGE_SIZE as i64;
    let pages =
        ((stats.total + page - 1) / page).max(if stats.total > 0 { 1 } else { 0 });
    ui.set_page_count(pages as i32);
    Ok(())
}

fn refresh_entries(ui: &AppWindow, db: &Db, project: &Project) {
    let page = ui.get_page().clamp(0, ui.get_page_count().saturating_sub(1)) as usize;
    ui.set_page(page as i32);
    let rows = db
        .sources_page(&project.id, page * PAGE_SIZE, PAGE_SIZE)
        .unwrap_or_default()
        .into_iter()
        .map(|e| EntryRow {
            id: e.source.id.into(),
            original: e.source.source_text.into(),
            translation: e.translated_text.unwrap_or_default().into(),
            speaker: e.source.speaker.unwrap_or_default().into(),
            status: e.status.as_str().into(),
            file: format!("{}:{}", e.source.file_path, e.source.line).into(),
        })
        .collect::<Vec<_>>();
    ui.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn refresh_glossary(ui: &AppWindow, db: &Db) {
    let rows = current_project(db)
        .and_then(|p| db.glossary_list(&p.id, &ui.get_glossary_search()).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|g| GlossaryRow {
            id: g.id.into(),
            source: g.source.into(),
            target: g.target.into(),
            note: g.note.unwrap_or_default().into(),
            enabled: g.enabled,
        })
        .collect::<Vec<_>>();
    ui.set_glossary(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn load_entry_detail(ui: &AppWindow, db: &Db, id: &str) {
    let Some(project) = current_project(db) else { return };
    let Some(entry) = db.source_by_id(id).ok().flatten() else { return };
    ui.set_sel_id(id.into());
    ui.set_sel_original(entry.source.source_text.clone().into());
    ui.set_sel_status(entry.status.as_str().into());
    ui.set_sel_translation(entry.translated_text.clone().unwrap_or_default().into());

    let window = context_window(db);
    let (previous, next) = db
        .neighbors(
            &project.id,
            &entry.source.file_path,
            entry.source.line,
            window.before as usize,
            window.after as usize,
        )
        .unwrap_or_default();
    ui.set_sel_context(ContextInfo {
        speaker: entry.source.speaker.clone().unwrap_or_default().into(),
        scene: entry.source.context.clone().unwrap_or_default().into(),
        previous: previous.join(" / ").into(),
        next: next.join(" / ").into(),
    });

    let hint = db
        .glossary_enabled(&project.id)
        .map(|entries| {
            glossary::matches_for_text(&entries, &entry.source.source_text)
                .iter()
                .map(|h| format!("{} → {}", h.source, h.target))
                .collect::<Vec<_>>()
                .join(",  ")
        })
        .unwrap_or_default();
    ui.set_sel_glossary_hint(hint.into());
}

// ----------------------------------------------------------------- settings

fn setting_or(db: &Db, key: &str, default: &str) -> String {
    db.setting_get_or(key, default).unwrap_or_else(|_| default.to_string())
}

fn parse_or<T: std::str::FromStr>(db: &Db, key: &str, default: T) -> T {
    db.setting_get(key)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn load_settings(ui: &AppWindow, db: &Db) {
    let endpoint = setting_or(db, "api_endpoint", "https://api.openai.com/v1");
    ui.set_provider_preset(preset_index_for(&endpoint));
    ui.set_set_endpoint(endpoint.clone().into());
    ui.set_set_api_key(setting_or(db, "api_key", "").into());

    // The remembered model is per provider.
    let key = provider_key_for(&endpoint);
    let model = db
        .setting_get(&format!("model:{key}"))
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            db.setting_get("model")
                .ok()
                .flatten()
                .filter(|m| !m.trim().is_empty())
        })
        .unwrap_or_else(|| default_model_for(key).to_string());
    set_model_dropdown(ui, &model);

    ui.set_set_batch_size(parse_or::<usize>(db, "batch_size", 20).to_string().into());
    ui.set_set_concurrency(parse_or::<usize>(db, "concurrency", 2).to_string().into());
    ui.set_set_context_before(parse_or::<u32>(db, "context_before", 1).to_string().into());
    ui.set_set_context_after(parse_or::<u32>(db, "context_after", 1).to_string().into());
    // An empty stored prompt falls back to the built-in default.
    let prompt = db
        .setting_get("prompt_template")
        .ok()
        .flatten()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| pipeline::default_prompt_template().to_string());
    ui.set_set_prompt(prompt.into());
    if let Some(project) = current_project(db) {
        ui.set_set_source_lang(project.source_language.clone().into());
        ui.set_set_target_lang(project.target_language.clone().into());
    } else {
        ui.set_set_source_lang(setting_or(db, "source_language", "English").into());
        ui.set_set_target_lang(setting_or(db, "target_language", "Thai").into());
    }
}

/// Fill the model dropdown with one item (the remembered model). The full
/// list arrives via the refresh button.
fn set_model_dropdown(ui: &AppWindow, model: &str) {
    ui.set_model_items(ModelRc::from(Rc::new(VecModel::from(vec![SharedString::from(
        model.to_string(),
    )]))));
    ui.set_model_index(0);
    ui.set_set_model(model.to_string().into());
}

/// Stable settings-key suffix per provider, so each provider remembers its
/// own model: `model:<key>`.
fn provider_key_for(endpoint: &str) -> &'static str {
    let e = endpoint.to_lowercase();
    if e.contains("ollama.com") {
        "ollama-cloud"
    } else if e.contains(":11434") {
        "ollama-local"
    } else if e.contains(":1234") {
        "lmstudio"
    } else if e.contains("openrouter") {
        "openrouter"
    } else if e.contains("api.openai.com") {
        "openai"
    } else {
        "custom"
    }
}

fn default_model_for(provider_key: &str) -> &'static str {
    match provider_key {
        "ollama-cloud" => "gpt-oss:120b-cloud",
        "ollama-local" | "lmstudio" => "llama3.1",
        "openrouter" => "openai/gpt-4o-mini",
        _ => "gpt-4o-mini",
    }
}

/// Store the model both as the global last-used value and per provider.
fn remember_model(db: &Db, endpoint: &str, model: &str) {
    if model.trim().is_empty() {
        return;
    }
    let _ = db.setting_set(&format!("model:{}", provider_key_for(endpoint)), model);
    let _ = db.setting_set("model", model);
}

/// Index into the Settings ComboBox model for a known endpoint.
fn preset_index_for(endpoint: &str) -> i32 {
    let e = endpoint.to_lowercase();
    if e.contains("ollama.com") {
        1 // Ollama Cloud
    } else if e.contains(":11434") {
        2 // Ollama local
    } else if e.contains(":1234") {
        3 // LM Studio
    } else if e.contains("openrouter") {
        4
    } else if e.contains("api.openai.com") {
        0
    } else {
        5 // Custom
    }
}

/// Cloud endpoints that genuinely require a key; local servers do not.
fn needs_api_key(endpoint: &str) -> bool {
    let e = endpoint.to_lowercase();
    e.contains("api.openai.com") || e.contains("ollama.com") || e.contains("openrouter.ai")
}

fn context_window(db: &Db) -> ContextWindow {
    ContextWindow {
        before: parse_or::<u32>(db, "context_before", 1).clamp(0, 10),
        after: parse_or::<u32>(db, "context_after", 1).clamp(0, 10),
    }
}

fn pipeline_config(ui: &AppWindow, project: &Project) -> pipeline::PipelineConfig {
    fn nonempty_or(value: &str, fallback: &str) -> String {
        let v = value.trim();
        if v.is_empty() { fallback.to_string() } else { v.to_string() }
    }
    pipeline::PipelineConfig {
        source_language: nonempty_or(&ui.get_set_source_lang(), &project.source_language),
        target_language: nonempty_or(&ui.get_set_target_lang(), &project.target_language),
        batch_size: ui.get_set_batch_size().trim().parse().unwrap_or(20),
        concurrency: ui.get_set_concurrency().trim().parse().unwrap_or(2),
        context: ContextWindow {
            before: ui.get_set_context_before().trim().parse().unwrap_or(1),
            after: ui.get_set_context_after().trim().parse().unwrap_or(1),
        },
        prompt_template: nonempty_or(&ui.get_set_prompt(), pipeline::default_prompt_template()),
    }
    .clamped()
}

/// Provider settings are read from the live Settings fields, so changing
/// the endpoint/model takes effect on the next run without saving.
/// "Save Settings" persists them for the next app start.
fn build_provider(ui: &AppWindow) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(ProviderConfig {
        endpoint: ui.get_set_endpoint().trim().to_string(),
        api_key: ui.get_set_api_key().trim().to_string(),
        model: ui.get_set_model().trim().to_string(),
        temperature: 0.3,
        timeout_secs: 180,
    })
}

// ---------------------------------------------------------------- callbacks

/// Every callback follows the same shape: capture a `Weak<AppWindow>`,
/// upgrade it on entry, and push heavy work to a background thread whose
/// results come back through `upgrade_in_event_loop`.
fn wire_callbacks(ui: &AppWindow, db: Arc<Db>, cancel: Arc<AtomicBool>) {
    // --- Browse: pick folder -> detect engine -> create/load project -> scan
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_browse_project(move || {
            let ui_weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let picked = rfd::FileDialog::new()
                    .set_title("Select the game folder")
                    .pick_folder();
                let Some(path) = picked else { return };
                let engine = match detect_engine(&path) {
                    Some(engine) => engine,
                    None => {
                        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_status_message(
                                format!(
                                    "No supported game engine found in {}",
                                    path.display()
                                )
                                .into(),
                            );
                        });
                        return;
                    }
                };
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Game".to_string());
                let project = Project::new(name, path.to_string_lossy(), engine.id());

                let scan =
                    (|| -> anyhow::Result<(Project, crate::database::ScanReport)> {
                        let project = db.project_upsert(&project)?;
                        db.setting_set("current_project_id", &project.id)?;
                        let report = scan_project(&db, &project, engine)?;
                        Ok((project, report))
                    })();

                let _ = ui_weak.upgrade_in_event_loop(move |ui| match scan {
                    Ok((project, report)) => {
                        apply_project(&ui, &project);
                        ui.set_page(0);
                        let _ = refresh_stats(&ui, &db, &project);
                        refresh_entries(&ui, &db, &project);
                        ui.set_current_tab(0);
                        ui.set_status_message(
                            format!(
                                "Scanned: {} entries (+{} new, {} changed, {} removed, {} from memory)",
                                report.total,
                                report.added,
                                report.changed,
                                report.removed,
                                report.from_memory
                            )
                            .into(),
                        );
                    }
                    Err(e) => {
                        ui.set_status_message(format!("Scan failed: {e:#}").into());
                    }
                });
            });
        });
    }

    // --- Scan current project
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_scan_project(move || {
            let ui_weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let Some(project) = current_project(&db) else { return };
                let Some(engine) = engine_for(&project) else { return };
                let report = scan_project(&db, &project, engine);
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_page(0);
                    let _ = refresh_stats(&ui, &db, &project);
                    refresh_entries(&ui, &db, &project);
                    match report {
                        Ok(r) => ui.set_status_message(
                            format!(
                                "Scanned: {} entries (+{} new, {} changed, {} removed, {} from memory)",
                                r.total, r.added, r.changed, r.removed, r.from_memory
                            )
                            .into(),
                        ),
                        Err(e) => ui.set_status_message(format!("Scan failed: {e:#}").into()),
                    }
                });
            });
        });
    }

    // --- Pagination
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_prev_page(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            ui.set_page((ui.get_page() - 1).max(0));
            refresh_entries(&ui, &db, &project);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_next_page(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            ui.set_page((ui.get_page() + 1).min(ui.get_page_count().saturating_sub(1)));
            refresh_entries(&ui, &db, &project);
        });
    }

    // --- Entry selection / editing
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_select_entry(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            load_entry_detail(&ui, &db, &id);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_save_edit(move || {
            let Some(ui) = weak.upgrade() else { return };
            let id = ui.get_sel_id();
            if id.is_empty() {
                return;
            }
            let text = ui.get_sel_translation();
            if text.trim().is_empty() {
                ui.set_status_message("Translation is empty.".into());
                return;
            }
            if let Err(e) = db.set_translation(&id, Some(&text), TranslationStatus::Edited) {
                ui.set_status_message(format!("Save failed: {e:#}").into());
                return;
            }
            if let Some(entry) = db.source_by_id(&id).ok().flatten() {
                let lang = project_language(&db);
                let _ = crate::translation::memory::store(
                    &db,
                    &[(entry.source.source_hash, entry.source.source_text, text.to_string())],
                    &lang,
                );
                ui.set_sel_status("edited".into());
            }
            if let Some(project) = current_project(&db) {
                let _ = refresh_stats(&ui, &db, &project);
                refresh_entries(&ui, &db, &project);
            }
            ui.set_status_message("Saved (status: Edited).".into());
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let cancel = cancel.clone();
        ui.on_retranslate_entry(move || {
            let Some(ui) = weak.upgrade() else { return };
            let id = ui.get_sel_id();
            if id.is_empty() || ui.get_running() {
                return;
            }
            let Some(project) = current_project(&db) else { return };
            let endpoint = ui.get_set_endpoint().trim().to_string();
            let key = ui.get_set_api_key().trim().to_string();
            if needs_api_key(&endpoint) && key.is_empty() {
                ui.set_status_message(
                    "API key is not configured — set one in Settings, or pick the Ollama preset for local models."
                        .into(),
                );
                return;
            }
            if db.set_translation(&id, None, TranslationStatus::Pending).is_err() {
                return;
            }
            ui.set_running(true);
            ui.set_progress(0.0);
            cancel.store(false, Ordering::Relaxed);
            let config = pipeline_config(&ui, &project);
            let provider = build_provider(&ui);
            let ui_weak = ui.as_weak();
            let db = db.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let entry = db.source_by_id(&id).ok().flatten();
                let summary = match (engine_for(&project), entry) {
                    (Some(engine), Some(entry)) => pipeline::translate_entries(
                        pipeline::PipelineParams {
                            db: &db,
                            project: &project,
                            provider: &provider,
                            engine,
                            config: &config,
                            cancel: &cancel,
                            on_progress: &|_| {},
                        },
                        vec![entry],
                    ),
                    _ => pipeline::Summary::default(),
                };
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_running(false);
                    let _ = refresh_stats(&ui, &db, &project);
                    refresh_entries(&ui, &db, &project);
                    load_entry_detail(&ui, &db, &id);
                    ui.set_status_message(
                        if summary.translated > 0 {
                            "Re-translated.".into()
                        } else {
                            "Re-translate failed — see the status column.".into()
                        },
                    );
                });
            });
        });
    }

    // --- Translate all pending
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let cancel = cancel.clone();
        ui.on_start_translate(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_running() {
                return;
            }
            let Some(project) = current_project(&db) else {
                ui.set_status_message("Open a game project first.".into());
                return;
            };
            let endpoint = ui.get_set_endpoint().trim().to_string();
            let key = ui.get_set_api_key().trim().to_string();
            if needs_api_key(&endpoint) && key.is_empty() {
                ui.set_status_message(
                    "API key is not configured — set one in Settings, or pick the Ollama preset for local models."
                        .into(),
                );
                return;
            }
            ui.set_running(true);
            ui.set_progress(0.0);
            cancel.store(false, Ordering::Relaxed);

            // Snapshot the Settings fields now: the run uses exactly what
            // the tab shows — pressing Save Settings is not required.
            let config = pipeline_config(&ui, &project);
            let provider = build_provider(&ui);

            let ui_weak = ui.as_weak();
            let db = db.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let engine = engine_for(&project);

                let progress_ui = ui_weak.clone();
                let progress = move |p: pipeline::Progress| {
                    let _ = progress_ui.upgrade_in_event_loop(move |ui| {
                        let frac = if p.total == 0 { 1.0 } else { p.done as f32 / p.total as f32 };
                        ui.set_progress(frac);
                        ui.set_status_message(
                            format!(
                                "Translating… {}/{} (memory {}, translated {}, failed {})",
                                p.done, p.total, p.from_memory, p.translated, p.failed
                            )
                            .into(),
                        );
                    });
                };

                let summary = match engine {
                    Some(engine) => pipeline::translate_pending(pipeline::PipelineParams {
                        db: &db,
                        project: &project,
                        provider: &provider,
                        engine,
                        config: &config,
                        cancel: &cancel,
                        on_progress: &progress,
                    })
                    .unwrap_or_default(),
                    None => {
                        let _ = ui_weak.upgrade_in_event_loop(|ui| {
                            ui.set_status_message("Unknown engine for project.".into());
                        });
                        pipeline::Summary::default()
                    }
                };

                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_running(false);
                    ui.set_progress(1.0);
                    let _ = refresh_stats(&ui, &db, &project);
                    refresh_entries(&ui, &db, &project);
                    ui.set_status_message(
                        if summary.cancelled {
                            format!(
                                "Cancelled — {} translated, {} failed, {} from memory (remaining entries stay pending).",
                                summary.translated, summary.failed, summary.from_memory
                            )
                        } else {
                            format!(
                                "Done: {} translated, {} failed, {} from memory, {} request errors.",
                                summary.translated, summary.failed, summary.from_memory, summary.provider_errors
                            )
                        }
                        .into(),
                    );
                    if summary.provider_errors > 0 {
                        if let Some(err) = &summary.last_error {
                            let short: String = err.chars().take(150).collect();
                            ui.set_status_message(
                                format!("{} Last error: {}", ui.get_status_message(), short).into(),
                            );
                        }
                    }
                });
            });
        });
    }
    {
        let weak = ui.as_weak();
        let cancel = cancel.clone();
        ui.on_cancel_translate(move || {
            cancel.store(true, Ordering::Relaxed);
            if let Some(ui) = weak.upgrade() {
                ui.set_status_message("Cancelling after the current batch…".into());
            }
        });
    }

    // --- Export
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_do_export(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_running() {
                return;
            }
            let Some(project) = current_project(&db) else { return };
            ui.set_running(true);
            let ui_weak = ui.as_weak();
            let db = db.clone();
            std::thread::spawn(move || {
                let result =
                    (|| -> anyhow::Result<(usize, crate::core::engine::ExportReport)> {
                        let entries = db.export_entries(&project.id)?;
                        let engine = engine_for(&project)
                            .ok_or_else(|| anyhow::anyhow!("unknown engine"))?;
                        let report = engine.export(
                            Path::new(&project.path),
                            &entries,
                            &project.target_language,
                        )?;
                        Ok((entries.len(), report))
                    })();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_running(false);
                    match result {
                        Ok((count, report)) => ui.set_status_message(
                            format!(
                                "Exported {count} translations: {} entries written into {} files ({} skipped).",
                                report.entries_written, report.files_written, report.entries_skipped
                            )
                            .into(),
                        ),
                        Err(e) => ui.set_status_message(format!("Export failed: {e:#}").into()),
                    }
                });
            });
        });
    }

    // --- Glossary
    {
        let weak = ui.as_weak();
        ui.on_glossary_new(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_gl_id("".into());
            ui.set_gl_source("".into());
            ui.set_gl_target("".into());
            ui.set_gl_note("".into());
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_glossary_load(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(g) = db.glossary_get(&id).ok().flatten() {
                ui.set_gl_id(g.id.into());
                ui.set_gl_source(g.source.into());
                ui.set_gl_target(g.target.into());
                ui.set_gl_note(g.note.unwrap_or_default().into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_glossary_save(move || {
            let Some(ui) = weak.upgrade() else { return };
            let source = ui.get_gl_source().trim().to_string();
            let target = ui.get_gl_target().trim().to_string();
            let note = ui.get_gl_note().trim().to_string();
            if source.is_empty() || target.is_empty() {
                ui.set_status_message("Glossary needs a source term and a translation.".into());
                return;
            }
            let id = ui.get_gl_id().to_string();
            let saved = if id.is_empty() {
                current_project(&db).and_then(|p| {
                    db.glossary_add(&p.id, &source, &target, Some(&note)).ok()
                })
            } else {
                db.glossary_get(&id).ok().flatten().map(|mut g| {
                    g.source = source.clone();
                    g.target = target.clone();
                    g.note = if note.is_empty() { None } else { Some(note.clone()) };
                    db.glossary_update(&g).ok();
                    g
                })
            };
            if saved.is_some() {
                ui.set_status_message("Glossary saved.".into());
            } else {
                ui.set_status_message(
                    "Could not save glossary entry (open a project first).".into(),
                );
            }
            refresh_glossary(&ui, &db);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_glossary_delete(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let _ = db.glossary_delete(&id);
            if ui.get_gl_id() == id {
                ui.set_gl_id("".into());
            }
            refresh_glossary(&ui, &db);
        });
    }
    {
        let db = db.clone();
        ui.on_glossary_toggle(move |id, checked| {
            if let Some(mut g) = db.glossary_get(&id).ok().flatten() {
                g.enabled = checked;
                let _ = db.glossary_update(&g);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_apply_glossary_search(move |text| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_glossary_search(text);
            refresh_glossary(&ui, &db);
        });
    }

    // --- Provider preset (Settings tab)
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_apply_provider_preset(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let endpoint = match name.as_str() {
                "OpenAI (cloud)" => "https://api.openai.com/v1",
                "Ollama Cloud (API key)" => "https://ollama.com/v1",
                "Ollama (local)" => "http://localhost:11434/v1",
                "LM Studio (local)" => "http://localhost:1234/v1",
                "OpenRouter" => "https://openrouter.ai/api/v1",
                _ => return, // Custom: keep the current endpoint
            };
            ui.set_set_endpoint(endpoint.into());

            // Swap to the model remembered for this provider (or a sensible
            // default); the user's choice per provider stays remembered.
            let key = provider_key_for(endpoint);
            let model = db
                .setting_get(&format!("model:{key}"))
                .ok()
                .flatten()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| default_model_for(key).to_string());
            set_model_dropdown(&ui, &model);
            remember_model(&db, endpoint, &model);

            match name.as_str() {
                "Ollama Cloud (API key)" => {
                    ui.set_status_message(
                        "Ollama Cloud selected — paste your API key from ollama.com, pick a model (⟳ lists them), then Save Settings. Thinking is disabled automatically."
                            .into(),
                    );
                }
                "Ollama (local)" | "LM Studio (local)" => {
                    ui.set_status_message(
                        "Local provider selected — the API key can stay empty. Pick a model (⟳ refreshes the list), then Save Settings."
                            .into(),
                    );
                }
                _ => {}
            }
        });
    }

    // --- Model dropdown / refresh (Settings tab)
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_apply_model(move |model| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_set_model(model.clone());
            let endpoint = ui.get_set_endpoint().trim().to_string();
            remember_model(&db, &endpoint, &model);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_refresh_models(move || {
            let Some(ui) = weak.upgrade() else { return };
            let provider = build_provider(&ui);
            let current = ui.get_set_model().trim().to_string();
            let endpoint = ui.get_set_endpoint().trim().to_string();
            let ui_weak = ui.as_weak();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = provider.list_models();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| match result {
                    Ok(mut models) => {
                        if models.is_empty() {
                            ui.set_status_message(
                                "The provider returned no models — check the endpoint/API key."
                                    .into(),
                            );
                            return;
                        }
                        models.sort();
                        let mut items: Vec<SharedString> =
                            models.into_iter().map(SharedString::from).collect();
                        // Keep a remembered model that the server didn't list.
                        if !current.is_empty()
                            && !items.iter().any(|m| m.as_str() == current)
                        {
                            items.insert(0, current.clone().into());
                        }
                        let index =
                            items.iter().position(|m| m.as_str() == current).unwrap_or(0);
                        let selected = items[index].clone();
                        ui.set_model_items(ModelRc::from(Rc::new(VecModel::from(items))));
                        ui.set_model_index(index as i32);
                        ui.set_set_model(selected.clone());
                        remember_model(&db, &endpoint, &selected);
                        let count = ui.get_model_items().row_count();
                        ui.set_status_message(
                            format!("Model list refreshed — {count} models.").into(),
                        );
                    }
                    Err(e) => {
                        ui.set_status_message(format!("Refresh models failed: {e:#}").into());
                    }
                });
            });
        });
    }

    // --- Settings
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_save_settings(move || {
            let Some(ui) = weak.upgrade() else { return };
            let values: [(&str, String); 10] = [
                ("api_endpoint", ui.get_set_endpoint().to_string()),
                ("api_key", ui.get_set_api_key().to_string()),
                ("model", ui.get_set_model().to_string()),
                ("source_language", ui.get_set_source_lang().to_string()),
                ("target_language", ui.get_set_target_lang().to_string()),
                ("batch_size", ui.get_set_batch_size().to_string()),
                ("concurrency", ui.get_set_concurrency().to_string()),
                ("context_before", ui.get_set_context_before().to_string()),
                ("context_after", ui.get_set_context_after().to_string()),
                ("prompt_template", ui.get_set_prompt().to_string()),
            ];
            for (key, value) in values {
                let _ = db.setting_set(key, &value);
            }
            // Remember the model per provider as well as globally.
            remember_model(&db, &ui.get_set_endpoint(), &ui.get_set_model());
            if let Some(project) = current_project(&db) {
                let _ = db.project_update_languages(
                    &project.id,
                    &ui.get_set_source_lang(),
                    &ui.get_set_target_lang(),
                );
            }
            ui.set_status_message("Settings saved.".into());
        });
    }
}

fn project_language(db: &Db) -> String {
    match current_project(db) {
        Some(p) => p.target_language,
        None => setting_or(db, "target_language", "Thai"),
    }
}
