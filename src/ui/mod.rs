//! Slint <-> Rust glue.
//!
//! Windows:
//! - `AppWindow` — main window with the Project / Glossary / Settings tabs
//!   (heavy work on background threads, results pushed back via
//!   `upgrade_in_event_loop`), plus in-app overlays: the delete-confirmation
//!   dialog and the AI-profile add/edit form (endpoint / key / model /
//!   temperature), applied on Confirm.
//!
//! Settings fields auto-save into SQLite on every edit, so the translate
//! flows always read the current values from the DB.

use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::ai::{OpenAiCompatibleProvider, ProviderConfig, TranslationProvider as _};
use crate::core::context::ContextWindow;
use crate::core::engine::{detect_engine, engine_display_name, registry, GameEngine};
use crate::core::glossary;
use crate::core::project::{new_id, now_unix, Project};
use crate::core::scan::scan_project;
use crate::core::translation::TranslationStatus;
use crate::database::{AiProfile, Db};
use crate::translation::pipeline;

slint::include_modules!();

/// How many entries the list loads per page (pagination keeps RAM flat).
const PAGE_SIZE: usize = 200;

pub fn run(db: Arc<Db>) -> Result<()> {
    let app = AppWindow::new()?;
    let cancel = Arc::new(AtomicBool::new(false));

    // Repair stray spaces older runs may have saved.
    let _ = db.cleanup_translations();
    load_settings(&app, &db);
    if let Some(project) = current_project(&db) {
        apply_project(&app, &project);
        if refresh_stats(&app, &db, &project).is_ok() {
            refresh_entries(&app, &db, &project);
        }
        refresh_glossary(&app, &db);
    }
    wire_app_callbacks(&app, db.clone(), cancel);
    wire_settings_callbacks(&app, db);
    let _ = app.show();
    let _ = app.run()?;
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

/// Map the status-filter combo index to a DB status key (None = All).
fn status_filter(ui: &AppWindow) -> Option<&'static str> {
    match ui.get_status_filter_index() {
        1 => Some("pending"),
        2 => Some("translated"),
        3 => Some("failed"),
        _ => None,
    }
}

fn refresh_entries(ui: &AppWindow, db: &Db, project: &Project) {
    let term = ui.get_entry_search().trim().to_string();
    let status = status_filter(ui);
    // One page of rows plus the total the page counter should show.
    // With an active search the filter runs in Rust (for match-case /
    // whole-word support), producing an id list paginated here.
    let (rows, total) = if term.is_empty() {
        let total = db.sources_count(&project.id, status).unwrap_or(0);
        let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
        let page = (ui.get_page() as usize).min(pages - 1);
        ui.set_page(page as i32);
        let rows = db
            .sources_page(&project.id, status, page * PAGE_SIZE, PAGE_SIZE)
            .unwrap_or_default();
        (rows, total)
    } else {
        let ids = db
            .search_entry_ids(
                &project.id,
                &term,
                ui.get_entry_search_case(),
                ui.get_entry_search_word(),
                status,
            )
            .unwrap_or_default();
        let total = ids.len();
        let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1);
        let page = (ui.get_page() as usize).min(pages - 1);
        ui.set_page(page as i32);
        let slice: Vec<String> = ids
            .iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .cloned()
            .collect();
        (db.entries_by_ids(&slice).unwrap_or_default(), total)
    };
    let pages = ((total + PAGE_SIZE - 1) / PAGE_SIZE).max(1) as i32;
    ui.set_page_count(pages);
    let rows = rows.into_iter().map(entry_to_row).collect::<Vec<_>>();
    ui.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn entry_to_row(e: crate::core::translation::TranslationEntry) -> EntryRow {
    EntryRow {
        id: e.source.id.into(),
        original: e.source.source_text.into(),
        // Older rows may contain stray spaces from early runs —
        // normalize for display.
        translation: e
            .translated_text
            .as_deref()
            .map(crate::core::source::clean_spaces)
            .unwrap_or_default()
            .into(),
        speaker: e.source.speaker.unwrap_or_default().into(),
        status: e.status.as_str().into(),
        file: format!("{}:{}", e.source.file_path, e.source.line).into(),
    }
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

fn refresh_proposals(ui: &AppWindow, db: &Db) {
    let rows = current_project(db)
        .and_then(|p| db.glossary_proposals_list(&p.id).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|p| GlossaryProposalRow {
            id: p.id.into(),
            source: p.source.into(),
            target: p.target.into(),
            occurrences: p.occurrences as i32,
        })
        .collect::<Vec<_>>();
    ui.set_gp_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn load_entry_detail(ui: &AppWindow, db: &Db, id: &str) {
    let Some(project) = current_project(db) else { return };
    let Some(entry) = db.source_by_id(id).ok().flatten() else { return };
    ui.set_sel_id(id.into());
    ui.set_sel_original(entry.source.source_text.clone().into());
    ui.set_sel_status(entry.status.as_str().into());
    // Normalize stray spaces so the editor matches the cleaned list view.
    let translation = entry
        .translated_text
        .map(|t| crate::core::source::clean_spaces(&t));
    ui.set_sel_translation(translation.unwrap_or_default().into());

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

// ------------------------------------------------------- settings utilities

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

fn context_window(db: &Db) -> ContextWindow {
    ContextWindow {
        before: parse_or::<u32>(db, "context_before", 1).clamp(0, 10),
        after: parse_or::<u32>(db, "context_after", 1).clamp(0, 10),
    }
}

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

/// Best-effort remembered model for a provider, used when a profile has no
/// model of its own.
fn model_for_endpoint(db: &Db, endpoint: &str) -> String {
    let key = provider_key_for(endpoint);
    db.setting_get(&format!("model:{key}"))
        .ok()
        .flatten()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            db.setting_get("model")
                .ok()
                .flatten()
                .filter(|m| !m.trim().is_empty())
        })
        .unwrap_or_else(|| default_model_for(key).to_string())
}

/// Settings live in SQLite and are auto-saved on every edit, so the run
/// flows can simply read the current values back.
fn pipeline_config(db: &Db, project: &Project) -> pipeline::PipelineConfig {
    pipeline::PipelineConfig {
        source_language: setting_or(db, "source_language", &project.source_language),
        target_language: setting_or(db, "target_language", &project.target_language),
        batch_size: parse_or(db, "batch_size", 20),
        concurrency: parse_or(db, "concurrency", 2),
        context: context_window(db),
        prompt_template: db
            .setting_get("prompt_template")
            .ok()
            .flatten()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| pipeline::default_prompt_template().to_string()),
        ignore_memory: false,
    }
    .clamped()
}

fn build_provider(db: &Db, purpose_key: &str) -> OpenAiCompatibleProvider {
    let profile = db.ai_profile_for_purpose(purpose_key).ok();
    let endpoint = profile
        .as_ref()
        .map(|p| p.endpoint.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let api_key = profile
        .as_ref()
        .map(|p| p.api_key.trim().to_string())
        .unwrap_or_default();
    let model = profile
        .as_ref()
        .map(|p| p.model.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| model_for_endpoint(db, &endpoint));
    let temperature = profile.as_ref().map(|p| p.temperature).unwrap_or(0.3);
    OpenAiCompatibleProvider::new(ProviderConfig {
        endpoint,
        api_key,
        model,
        temperature,
        timeout_secs: 180,
    })
}

fn project_language(db: &Db) -> String {
    match current_project(db) {
        Some(p) => p.target_language,
        None => setting_or(db, "target_language", "Thai"),
    }
}

// ------------------------------------------------------- settings tab

/// Load every settings field from the database.
fn load_settings(ui: &AppWindow, db: &Db) {
    let _ = db.ai_profile_ensure_default();
    refresh_profiles(ui, db);
    refresh_profile_pickers(ui, db);

    ui.set_set_source_lang(setting_or(db, "source_language", "English").into());
    ui.set_set_target_lang(setting_or(db, "target_language", "Thai").into());
    ui.set_set_batch_size(parse_or::<usize>(db, "batch_size", 20).to_string().into());
    ui.set_set_concurrency(parse_or::<usize>(db, "concurrency", 2).to_string().into());
    ui.set_set_context_before(parse_or::<u32>(db, "context_before", 1).to_string().into());
    ui.set_set_context_after(parse_or::<u32>(db, "context_after", 1).to_string().into());
    let prompt = db
        .setting_get("prompt_template")
        .ok()
        .flatten()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| pipeline::default_prompt_template().to_string());
    ui.set_set_prompt(prompt.into());
}

/// Rebuild the profile list.
fn refresh_profiles(ui: &AppWindow, db: &Db) {
    let rows: Vec<ProfileRow> = db
        .ai_profile_list()
        .unwrap_or_default()
        .into_iter()
        .map(|p| ProfileRow {
            id: p.id.into(),
            name: p.name.into(),
            model: p.model.into(),
            endpoint: p.endpoint.into(),
        })
        .collect();
    ui.set_profile_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
}

/// Fill the per-page provider dropdowns (Project = translation,
/// Glossary = extraction).
fn refresh_profile_pickers(ui: &AppWindow, db: &Db) {
    let profiles = db.ai_profile_list().unwrap_or_default();
    let names: Vec<SharedString> =
        profiles.iter().map(|p| SharedString::from(p.name.clone())).collect();
    let model = ModelRc::from(Rc::new(VecModel::from(names)));

    ui.set_translation_profile_items(model.clone());
    ui.set_glossary_profile_items(model);

    let index_for = |purpose: &str| {
        let want = db.setting_get(purpose).ok().flatten().unwrap_or_default();
        profiles
            .iter()
            .position(|p| p.id == want)
            .unwrap_or(0) as i32
    };
    ui.set_translation_profile_index(index_for(crate::database::PURPOSE_TRANSLATION));
    ui.set_glossary_profile_index(index_for(crate::database::PURPOSE_GLOSSARY));
}

fn perform_profile_delete(ui: &AppWindow, db: &Db, id: &str) {
    let _ = db.ai_profile_delete(id);
    // Repoint any purpose that referenced the deleted profile.
    let remaining = db.ai_profile_list().unwrap_or_default();
    if let Some(first) = remaining.first() {
        for key in [
            crate::database::PURPOSE_TRANSLATION,
            crate::database::PURPOSE_GLOSSARY,
        ] {
            let cur = db.setting_get(key).ok().flatten().unwrap_or_default();
            if cur.as_str() == id {
                let _ = db.setting_set(key, &first.id);
            }
        }
    }
    refresh_profiles(ui, db);
    refresh_profile_pickers(ui, db);
    ui.set_status_message("Profile deleted.".into());
}

fn wire_settings_callbacks(ui: &AppWindow, db: Arc<Db>) {
    // Shared state: which profile the editor modal is working on
    // (None = creating a new one).
    let editing: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // ---------------------------------------------------- profile list
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_profile_delete(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let name = db
                .ai_profile_get(&id)
                .ok()
                .flatten()
                .map(|p| p.name)
                .unwrap_or_else(|| "this profile".into());
            ui.set_confirm_kind("profile".into());
            ui.set_confirm_pending_id(id.clone());
            ui.set_confirm_title("Delete profile".into());
            ui.set_confirm_message(
                format!("Delete \u{201c}{name}\u{201d}? Pages using it will fall back to another profile.")
                    .into(),
            );
            ui.set_confirm_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_confirm_delete(move || {
            let Some(ui) = weak.upgrade() else { return };
            let id = ui.get_confirm_pending_id();
            match ui.get_confirm_kind().as_str() {
                "glossary" => {
                    let _ = db.glossary_delete(&id);
                    if ui.get_gl_id() == id {
                        ui.set_gl_id("".into());
                    }
                    refresh_glossary(&ui, &db);
                    ui.set_status_message("Glossary entry deleted.".into());
                }
                _ => perform_profile_delete(&ui, &db, &id),
            }
            ui.set_confirm_visible(false);
            ui.set_confirm_pending_id("".into());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_cancel_delete(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_confirm_visible(false);
                ui.set_confirm_pending_id("".into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let editing_weak = Arc::clone(&editing);
        ui.on_profile_add(move || {
            let Some(ui) = weak.upgrade() else { return };
            let profiles = db.ai_profile_list().unwrap_or_default();
            let mut n = profiles.len() + 1;
            while profiles
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(&format!("Profile {n}")))
            {
                n += 1;
            }
            let endpoint = "https://api.openai.com/v1";
            ui.set_ed_name(format!("Profile {n}").into());
            ui.set_ed_preset(0);
            ui.set_ed_endpoint(endpoint.into());
            ui.set_ed_api_key(String::new().into());
            let model = default_model_for("openai").to_string();
            ui.set_ed_model_items(ModelRc::from(Rc::new(VecModel::from(vec![SharedString::from(
                model.clone(),
            )]))));
            ui.set_ed_model_index(0);
            ui.set_ed_model(model.into());
            ui.set_ed_temperature("0.3".into());
            ui.set_ed_error(String::new().into());
            *editing_weak.lock().unwrap() = None;
            ui.set_ed_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let editing_weak = Arc::clone(&editing);
        ui.on_profile_edit(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(p) = db.ai_profile_get(&id).ok().flatten() else { return };
            ui.set_ed_name(p.name.clone().into());
            ui.set_ed_preset(preset_index_for(&p.endpoint));
            ui.set_ed_endpoint(p.endpoint.clone().into());
            ui.set_ed_api_key(p.api_key.clone().into());
            let model = if p.model.trim().is_empty() {
                model_for_endpoint(&db, &p.endpoint)
            } else {
                p.model.clone()
            };
            ui.set_ed_model_items(ModelRc::from(Rc::new(VecModel::from(vec![SharedString::from(
                model.clone(),
            )]))));
            ui.set_ed_model_index(0);
            ui.set_ed_model(model.into());
            ui.set_ed_temperature(format!("{:.2}", p.temperature).into());
            ui.set_ed_error(String::new().into());
            *editing_weak.lock().unwrap() = Some(p.id.clone());
            ui.set_ed_visible(true);
        });
    }

    // ---------------------------------------------------- profile modal
    {
        let weak = ui.as_weak();
        ui.on_ed_apply_preset(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let endpoint = match name.as_str() {
                "OpenAI (cloud)" => "https://api.openai.com/v1",
                "Ollama Cloud (API key)" => "https://ollama.com/v1",
                "Ollama (local)" => "http://localhost:11434/v1",
                "LM Studio (local)" => "http://localhost:1234/v1",
                "OpenRouter" => "https://openrouter.ai/api/v1",
                _ => return,
            };
            ui.set_ed_endpoint(endpoint.into());
            ui.set_ed_model(default_model_for(provider_key_for(endpoint)).into());
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_ed_refresh_models(move || {
            let Some(ui) = weak.upgrade() else { return };
            let provider = OpenAiCompatibleProvider::new(ProviderConfig {
                endpoint: ui.get_ed_endpoint().trim().to_string(),
                api_key: ui.get_ed_api_key().trim().to_string(),
                model: ui.get_ed_model().trim().to_string(),
                temperature: 0.3,
                timeout_secs: 60,
            });
            let current = ui.get_ed_model().trim().to_string();
            let ui_weak = weak.clone();
            std::thread::spawn(move || {
                let result = provider.list_models();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| match result {
                    Ok(mut models) => {
                        models.sort();
                        models.dedup();
                        if models.is_empty() {
                            ui.set_ed_error("The provider returned no models — check the endpoint/API key.".into());
                            return;
                        }
                        if !current.is_empty() && !models.iter().any(|m| *m == current) {
                            models.insert(0, current.clone());
                        }
                        let index =
                            models.iter().position(|m| *m == current).unwrap_or(0);
                        let items: Vec<SharedString> =
                            models.into_iter().map(SharedString::from).collect();
                        ui.set_ed_model_items(ModelRc::from(Rc::new(VecModel::from(items))));
                        ui.set_ed_model_index(index as i32);
                        ui.set_ed_error(String::new().into());
                    }
                    Err(e) => ui.set_ed_error(format!("{e:#}").into()),
                });
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_ed_apply_model(move |model| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_ed_model(model);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_ed_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_ed_visible(false);
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let editing_weak = Arc::clone(&editing);
        ui.on_ed_confirm(move || {
            let Some(ui) = weak.upgrade() else { return };
            let name = ui.get_ed_name().trim().to_string();
            let endpoint = ui.get_ed_endpoint().trim().to_string();
            let api_key = ui.get_ed_api_key().trim().to_string();
            let model = ui.get_ed_model().trim().to_string();
            let temperature = ui
                .get_ed_temperature()
                .trim()
                .parse::<f32>()
                .unwrap_or(f32::NAN);

            if name.is_empty() {
                ui.set_ed_error("Profile name is empty.".into());
                return;
            }
            if endpoint.is_empty() {
                ui.set_ed_error("API endpoint is empty.".into());
                return;
            }
            if temperature.is_nan() || !(0.0..=2.0).contains(&temperature) {
                ui.set_ed_error("Temperature must be a number between 0.0 and 2.0.".into());
                return;
            }
            let editing_id = editing_weak.lock().unwrap().clone();
            let dup = db
                .ai_profile_list()
                .unwrap_or_default()
                .iter()
                .any(|p| {
                    p.name.eq_ignore_ascii_case(&name)
                        && editing_id.as_deref() != Some(p.id.as_str())
                });
            if dup {
                ui.set_ed_error(format!("A profile named \"{name}\" already exists.").into());
                return;
            }

            let now = now_unix();
            let profile = AiProfile {
                id: editing_id.clone().unwrap_or_else(new_id),
                name,
                endpoint,
                api_key,
                model,
                temperature,
                created_at: now,
                updated_at: now,
            };
            if db.ai_profile_upsert(&profile).is_err() {
                ui.set_ed_error("Could not save the profile.".into());
                return;
            }
            // First profile becomes the active one.
            if db.setting_get("active_profile_translation").ok().flatten().is_none() {
                let _ = db.setting_set("active_profile_translation", &profile.id);
            }
            ui.set_ed_visible(false);
            refresh_profiles(&ui, &db);
            refresh_profile_pickers(&ui, &db);
            ui.set_status_message(format!("Profile \"{}\" saved.", profile.name).into());
        });
    }

    // ------------------------------------------ per-page provider pickers
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_pick_translation_profile(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let found = db
                .ai_profile_list()
                .unwrap_or_default()
                .into_iter()
                .find(|p| p.name.as_str() == name.as_str());
            if let Some(p) = found {
                let _ = db.setting_set(crate::database::PURPOSE_TRANSLATION, &p.id);
                ui.set_status_message(
                    format!("Translation provider: {}", p.name).into(),
                );
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_pick_glossary_profile(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let found = db
                .ai_profile_list()
                .unwrap_or_default()
                .into_iter()
                .find(|p| p.name.as_str() == name.as_str());
            if let Some(p) = found {
                let _ = db.setting_set(crate::database::PURPOSE_GLOSSARY, &p.id);
                ui.set_status_message(format!("Glossary provider: {}", p.name).into());
            }
        });
    }

    // ------------------------------------------ translation (auto-saved)
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_save_source_lang(move |v| {
            if let Some(ui) = weak.upgrade() {
                ui.set_set_source_lang(v.clone());
            }
            let _ = db.setting_set("source_language", v.trim());
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_save_target_lang(move |v| {
            if let Some(ui) = weak.upgrade() {
                ui.set_set_target_lang(v.clone());
            }
            let _ = db.setting_set("target_language", v.trim());
        });
    }
    {
        let db = db.clone();
        ui.on_save_batch_size(move |v| {
            let _ = db.setting_set("batch_size", v.trim());
        });
    }
    {
        let db = db.clone();
        ui.on_save_concurrency(move |v| {
            let _ = db.setting_set("concurrency", v.trim());
        });
    }
    {
        let db = db.clone();
        ui.on_save_context_before(move |v| {
            let _ = db.setting_set("context_before", v.trim());
        });
    }
    {
        let db = db.clone();
        ui.on_save_context_after(move |v| {
            let _ = db.setting_set("context_after", v.trim());
        });
    }
    {
        let db = db.clone();
        ui.on_save_prompt(move |v| {
            let _ = db.setting_set("prompt_template", v.trim());
        });
    }
}

// --------------------------------------------------------- find & replace

/// Regex for the Find & Replace dialog. The term is always capture group 1;
/// in whole-word mode the boundary characters are groups 1 and 3 so they
/// survive replacement.
fn fr_build_regex(find: &str, match_case: bool, whole_word: bool) -> anyhow::Result<regex::Regex> {
    let escaped = regex::escape(find);
    let ci = if match_case { "" } else { "(?i)" };
    let pattern = if whole_word {
        format!("{ci}(^|\\W)({escaped})(?:$|\\W)")
    } else {
        format!("{ci}({escaped})")
    };
    Ok(regex::Regex::new(&pattern)?)
}

/// Replace every match inside one translation, keeping whole-word boundary
/// characters intact.
fn fr_replace_text(text: &str, re: &regex::Regex, replace: &str) -> String {
    re.replace_all(text, |caps: &regex::Captures| {
        if caps.len() >= 4 {
            format!("{}{}{}", &caps[1], replace, &caps[3])
        } else {
            replace.to_string()
        }
    })
    .into_owned()
}

/// Compute (match_count, first ten old→new previews) for the dialog.
fn fr_compute(
    ui: &AppWindow,
    db: &Db,
    project: &Project,
) -> anyhow::Result<(usize, Vec<(String, String)>)> {
    let find = ui.get_fr_find().trim().to_string();
    if find.is_empty() {
        return Ok((0, Vec::new()));
    }
    let re = fr_build_regex(&find, ui.get_fr_case(), ui.get_fr_word())?;
    let replace = ui.get_fr_replace().to_string();
    let entries = db.all_entries(&project.id)?;
    let mut total = 0;
    let mut previews = Vec::new();
    for e in entries {
        let Some(text) = e.translated_text.as_deref() else { continue };
        if !re.is_match(text) {
            continue;
        }
        total += 1;
        if previews.len() < 10 {
            let new_text = fr_replace_text(text, &re, &replace);
            previews.push((text.to_string(), new_text));
        }
    }
    Ok((total, previews))
}

// --------------------------------------------------------- app callbacks

/// Trim a trailing separator (drive roots like `C:\` keep theirs).
fn fp_normalize(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    let trimmed = s.trim_end_matches(['/', '\\']);
    if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
        format!("{trimmed}\\")
    } else if trimmed.is_empty() {
        s
    } else {
        trimmed.to_string()
    }
}

/// Read the directory listing shown by the folder picker: subdirectories
/// only, ".." first. An empty path lists available drives.
fn fp_load_dir(ui: &AppWindow, target: &str) {
    let target = target.trim();
    let (path_text, mut names, error) = if target.is_empty() {
        let drives: Vec<String> = (b'A'..=b'Z')
            .map(|b| format!("{}:\\", b as char))
            .filter(|d| Path::new(d).is_dir())
            .collect();
        (String::new(), drives, String::new())
    } else {
        match std::fs::read_dir(target) {
            Ok(entries) => {
                let mut dirs: Vec<String> = entries
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| !n.starts_with('.'))
                    .collect();
                dirs.sort_by_key(|a| a.to_lowercase());
                (target.to_string(), dirs, String::new())
            }
            Err(e) => (
                target.to_string(),
                Vec::new(),
                format!("Cannot open folder: {e}"),
            ),
        }
    };
    if !target.is_empty() && error.is_empty() {
        names.insert(0, "..".to_string());
    }
    let items: Vec<SharedString> = names.into_iter().map(SharedString::from).collect();
    ui.set_fp_path(path_text.into());
    ui.set_fp_items(ModelRc::from(Rc::new(VecModel::from(items))));
    ui.set_fp_status(error.into());
}

fn wire_app_callbacks(ui: &AppWindow, db: Arc<Db>, cancel: Arc<AtomicBool>) {
    // Shared runner for "Translate pending" and bulk re-translate: spawns
    // the pipeline on a background thread and streams progress back.
    fn spawn_translation_run(
        ui: &AppWindow,
        db: &Arc<Db>,
        cancel: &Arc<AtomicBool>,
        project: &Project,
        ignore_memory: bool,
    ) {
        ui.set_running(true);
        ui.set_progress(0.0);
        cancel.store(false, Ordering::Relaxed);

        // Snapshot settings (auto-saved) on the UI thread.
        let config = pipeline::PipelineConfig {
            ignore_memory,
            ..pipeline_config(db, project)
        };
        let provider = build_provider(db, crate::database::PURPOSE_TRANSLATION);

        let ui_weak = ui.as_weak();
        let db = db.clone();
        let cancel = cancel.clone();
        let project = project.clone();
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
    }

    // --- Browse: open the in-app folder picker; picking a folder then
    //     detects the engine, creates/loads the project and scans it.
    {
        let weak = ui.as_weak();
        ui.on_browse_project(move || {
            let Some(ui) = weak.upgrade() else { return };
            // Start next to the current project, else in the home directory.
            let start = match ui.get_project_path().to_string() {
                p if !p.is_empty() => Path::new(&p)
                    .parent()
                    .map(fp_normalize)
                    .unwrap_or_default(),
                _ => dirs::home_dir().map(|p| fp_normalize(&p)).unwrap_or_default(),
            };
            fp_load_dir(&ui, &start);
            ui.set_fp_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_fp_navigate(move |name| {
            let Some(ui) = weak.upgrade() else { return };
            let cur = ui.get_fp_path().to_string();
            let target = if name.as_str() == ".." {
                Path::new(&cur).parent().map(fp_normalize).unwrap_or_default()
            } else if cur.is_empty() {
                name.to_string() // a drive root like "C:\"
            } else {
                fp_normalize(&Path::new(&cur).join(name.as_str()))
            };
            fp_load_dir(&ui, &target);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_fp_go(move || {
            let Some(ui) = weak.upgrade() else { return };
            let text = ui.get_fp_path().trim().to_string();
            fp_load_dir(&ui, &text);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_fp_confirm(move || {
            let Some(ui) = weak.upgrade() else { return };
            let dir = ui.get_fp_path().trim().to_string();
            if dir.is_empty() {
                ui.set_fp_status("Choose a drive first.".into());
                return;
            }
            if !Path::new(&dir).is_dir() {
                ui.set_fp_status("That path is not a folder.".into());
                return;
            }
            ui.set_fp_visible(false);
            let ui_weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let engine = match detect_engine(Path::new(&dir)) {
                    Some(engine) => engine,
                    None => {
                        let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                            ui.set_status_message(
                                format!("No supported game engine found in {dir}").into(),
                            );
                        });
                        return;
                    }
                };
                let name = Path::new(&dir)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Game".to_string());
                let project = Project::new(name, dir, engine.id());

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
    {
        let weak = ui.as_weak();
        ui.on_fp_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_fp_visible(false);
            }
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
        ui.on_close_editor(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_sel_id("".into());
            ui.set_sel_original("".into());
            ui.set_sel_translation("".into());
            ui.set_sel_glossary_hint("".into());
            ui.set_sel_status("".into());
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
            let profile = db
                .ai_profile_for_purpose(crate::database::PURPOSE_TRANSLATION)
                .ok();
            let endpoint = profile.as_ref().map(|p| p.endpoint.clone()).unwrap_or_default();
            let key = profile.as_ref().map(|p| p.api_key.clone()).unwrap_or_default();
            if needs_api_key(&endpoint) && key.trim().is_empty() {
                ui.set_status_message(
                    "API key is not configured — set one in Settings for this profile."
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
            let config = pipeline_config(&db, &project);
            let provider = build_provider(&db, crate::database::PURPOSE_TRANSLATION);
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
            let profile = db
                .ai_profile_for_purpose(crate::database::PURPOSE_TRANSLATION)
                .ok();
            let endpoint = profile.as_ref().map(|p| p.endpoint.clone()).unwrap_or_default();
            let key = profile.as_ref().map(|p| p.api_key.clone()).unwrap_or_default();
            if needs_api_key(&endpoint) && key.trim().is_empty() {
                ui.set_status_message(
                    "API key is not configured — set one in Settings, or pick the Ollama preset for local models."
                        .into(),
                );
                return;
            }
            spawn_translation_run(&ui, &db, &cancel, &project, false);
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

    // --- AI glossary extraction
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_extract_glossary(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            ui.set_gp_busy(true);
            ui.set_gp_status("".into());
            ui.set_status_message("Extracting glossary: mining candidate terms…".into());
            let ui_weak = weak.clone();
            let db = db.clone();
            std::thread::spawn(move || {
                let result = (|| -> anyhow::Result<usize> {
                    let texts = db.all_source_texts(&project.id)?;
                    let candidates = crate::glossary_ai::mine_candidates(&texts, 120);
                    if candidates.is_empty() {
                        return Ok(0);
                    }
                    let provider = build_provider(&db, crate::database::PURPOSE_GLOSSARY);
                    let lang = project_language(&db);
                    let proposals = crate::glossary_ai::propose_glossary(
                        &provider, &lang, &candidates, 50,
                    )?;
                    db.glossary_proposals_replace(&project.id, &proposals)
                })();
                let _ = ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.set_gp_busy(false);
                    match result {
                        Ok(count) if count > 0 => {
                            refresh_proposals(&ui, &db);
                            ui.set_gp_visible(true);
                            ui.set_status_message(
                                format!("{count} glossary proposals ready for review.").into(),
                            );
                        }
                        Ok(_) => ui.set_status_message(
                            "AI found no new glossary terms.".into(),
                        ),
                        Err(e) => ui.set_status_message(
                            format!("Glossary extraction failed: {e:#}").into(),
                        ),
                    }
                });
            });
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_gp_accept(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let proposals = db.glossary_proposals_list(&project.id).unwrap_or_default();
            if let Some(p) = proposals.iter().find(|p| p.id == id.as_str()) {
                let _ = db.glossary_add(&project.id, &p.source, &p.target, None);
                let _ = db.glossary_proposals_delete(&id);
                refresh_glossary(&ui, &db);
                refresh_proposals(&ui, &db);
                ui.set_status_message(format!("Added \"{}\" to glossary.", p.source).into());
            }
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_gp_reject(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let _ = db.glossary_proposals_delete(&id);
            refresh_proposals(&ui, &db);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_gp_accept_all(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let proposals = db.glossary_proposals_list(&project.id).unwrap_or_default();
            let mut added = 0;
            for p in &proposals {
                if db.glossary_add(&project.id, &p.source, &p.target, None).is_ok() {
                    added += 1;
                }
            }
            let _ = db.glossary_proposals_clear(&project.id);
            refresh_glossary(&ui, &db);
            refresh_proposals(&ui, &db);
            ui.set_gp_visible(false);
            ui.set_status_message(format!("Added {added} terms to glossary.").into());
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_gp_discard(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let _ = db.glossary_proposals_clear(&project.id);
            refresh_proposals(&ui, &db);
            ui.set_gp_visible(false);
            ui.set_status_message("Glossary proposals discarded.".into());
        });
    }

    // --- Bulk re-translate
    {
        let weak = ui.as_weak();
        ui.on_retranslate_open(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_running() {
                return;
            }
            ui.set_rt_scope(0);
            ui.set_rt_ignore_tm(true);
            ui.set_rt_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        let cancel = cancel.clone();
        ui.on_rt_confirm(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let scope_all = ui.get_rt_scope() == 1;
            let ignore_tm = ui.get_rt_ignore_tm();

            // Resolve the id set for the "current search / filter" scope.
            let ids: Option<Vec<String>> = if scope_all {
                None
            } else {
                let term = ui.get_entry_search().trim().to_string();
                Some(
                    db.search_entry_ids(
                        &project.id,
                        &term,
                        ui.get_entry_search_case(),
                        ui.get_entry_search_word(),
                        status_filter(&ui),
                    )
                    .unwrap_or_default(),
                )
            };
            let scope_note = match &ids {
                Some(ids) => format!("{} entries", ids.len()),
                None => "whole project".into(),
            };
            let reset = db.translations_reset_pending(&project.id, ids.as_deref());
            if let Ok(n) = reset {
                ui.set_status_message(format!("Re-translate: {n} entries reset ({scope_note}).").into());
            }
            ui.set_rt_visible(false);
            spawn_translation_run(&ui, &db, &cancel, &project, ignore_tm);
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_rt_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_rt_visible(false);
            }
        });
    }

    // --- QA check
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_qa_check(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let Some(engine) = engine_for(&project) else { return };
            let issues = crate::qa::scan(&db, &project.id, engine);
            let total = issues.len();
            let rows: Vec<QaRow> = issues
                .into_iter()
                .take(300)
                .map(|i| QaRow {
                    id: i.source_id.into(),
                    loc: format!("{}:{}", i.file, i.line).into(),
                    label: i.label.into(),
                })
                .collect();
            ui.set_qa_summary(
                if total > rows.len() {
                    format!("{} issues — showing the first {}", total, rows.len())
                } else if total == 1 {
                    "1 issue found".into()
                } else {
                    format!("{} issues found", total)
                }
                .into(),
            );
            ui.set_qa_rows(ModelRc::from(Rc::new(VecModel::from(rows))));
            ui.set_qa_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_qa_jump(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let Some(entry) = db.source_by_id(&id).ok().flatten() else { return };
            let Ok(rank) = db.entry_rank(&project.id, &entry.source.file_path, entry.source.line)
            else {
                return;
            };
            let pages = ((rank + PAGE_SIZE - 1) / PAGE_SIZE).max(1) as i32;
            ui.set_page(((rank as i32 - 1).max(0)) / PAGE_SIZE as i32);
            ui.set_page_count(pages);
            refresh_entries(&ui, &db, &project);
            ui.set_sel_id(id);
            load_entry_detail(&ui, &db, &ui.get_sel_id().to_string());
            ui.set_qa_visible(false);
        });
    }

    // --- Find & Replace (translations only)
    {
        let weak = ui.as_weak();
        ui.on_fr_open(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_running() {
                return;
            }
            let find = ui.get_entry_search().trim().to_string();
            if !find.is_empty() {
                ui.set_fr_find(find.into());
            }
            ui.set_fr_case(ui.get_entry_search_case());
            ui.set_fr_word(ui.get_entry_search_word());
            ui.set_fr_count("".into());
            ui.set_fr_preview(ModelRc::from(Rc::new(VecModel::from(Vec::new()))));
            ui.set_fr_visible(true);
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_fr_preview_run(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let (total, previews) = match fr_compute(&ui, &db, &project) {
                Ok(v) => v,
                Err(e) => {
                    ui.set_fr_count(format!("Invalid search: {e:#}").into());
                    return;
                }
            };
            let rows: Vec<SharedString> = previews
                .into_iter()
                .map(|(old, new)| SharedString::from(format!("{old}  →  {new}")))
                .collect();
            ui.set_fr_count(
                format!("{total} translations match — showing the first {}", rows.len()).into(),
            );
            ui.set_fr_preview(ModelRc::from(Rc::new(VecModel::from(rows))));
        });
    }
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_fr_replace_all(move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(project) = current_project(&db) else { return };
            let find = ui.get_fr_find().trim().to_string();
            if find.is_empty() {
                ui.set_fr_count("Enter text to find first.".into());
                return;
            }
            let Ok(re) = fr_build_regex(&find, ui.get_fr_case(), ui.get_fr_word()) else {
                ui.set_fr_count("Invalid search text.".into());
                return;
            };
            let replace = ui.get_fr_replace().to_string();
            let Some(engine) = engine_for(&project) else { return };
            let entries = db.all_entries(&project.id).unwrap_or_default();
            let mut updates: Vec<(String, String)> = Vec::new();
            let mut skipped = 0;
            for e in entries {
                let Some(text) = e.translated_text.as_deref() else { continue };
                if !re.is_match(text) {
                    continue;
                }
                let new_text = fr_replace_text(text, &re, &replace);
                if new_text == text {
                    continue;
                }
                // Never let a replacement destroy protected tokens.
                let tokens = engine.protected_tokens(&e.source.source_text);
                let lowered = new_text.to_lowercase();
                if tokens
                    .iter()
                    .any(|t| !lowered.contains(&t.to_lowercase()))
                {
                    skipped += 1;
                    continue;
                }
                updates.push((e.source.id.clone(), new_text));
            }
            match db.translations_replace_texts(&updates) {
                Ok(()) => {
                    ui.set_fr_visible(false);
                    if let Some(project) = current_project(&db) {
                        refresh_entries(&ui, &db, &project);
                    }
                    ui.set_status_message(
                        format!(
                            "Replaced in {} translations{}.",
                            updates.len(),
                            if skipped > 0 {
                                format!(", skipped {skipped} (would lose protected tokens)")
                            } else {
                                String::new()
                            }
                        )
                        .into(),
                    );
                }
                Err(e) => ui.set_fr_count(format!("Replace failed: {e:#}").into()),
            }
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_fr_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_fr_visible(false);
            }
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
            let label = db
                .glossary_get(&id)
                .ok()
                .flatten()
                .map(|g| format!("{} \u{2192} {}", g.source, g.target))
                .unwrap_or_else(|| "this entry".into());
            ui.set_confirm_kind("glossary".into());
            ui.set_confirm_pending_id(id.clone());
            ui.set_confirm_title("Delete glossary entry".into());
            ui.set_confirm_message(format!("Delete \u{201c}{label}\u{201d}?").into());
            ui.set_confirm_visible(true);
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
    {
        let weak = ui.as_weak();
        let db = db.clone();
        ui.on_apply_entry_search(move |_text| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_page(0);
            if let Some(project) = current_project(&db) {
                refresh_entries(&ui, &db, &project);
            }
        });
    }
}
