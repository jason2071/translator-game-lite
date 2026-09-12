use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::core::source::SourceEntry;
use crate::core::translation::TranslationEntry;

/// What an engine returns when scanning a game directory.
///
/// `existing_translations` carries translations found inside the game itself
/// (e.g. Ren'Py `new "..."` lines) so the importer can prefill the database.
#[derive(Debug, Default, Clone)]
pub struct ExtractionResult {
    pub sources: Vec<SourceEntry>,
    pub existing_translations: Vec<ExistingTranslation>,
}

#[derive(Debug, Clone)]
pub struct ExistingTranslation {
    pub source_id: String,
    pub text: String,
}

/// Report produced by [`GameEngine::export`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub files_written: usize,
    pub entries_written: usize,
    /// Entries skipped because the game file no longer matches the scanned text.
    pub entries_skipped: usize,
}

/// Engine-independent interface. One implementation per game engine lives in
/// `src/engine/<engine>/`; the translation core never sees engine specifics.
pub trait GameEngine: Sync {
    fn id(&self) -> &'static str;

    fn name(&self) -> &'static str;

    fn detect(&self, path: &Path) -> bool;

    fn extract(&self, path: &Path) -> Result<ExtractionResult>;

    /// Write translations back into the game files. Only entries with a
    /// translation are applied; untranslatable text must be left untouched.
    ///
    /// `target_language` is needed for engines that export through
    /// generated translation files (Ren'Py `tl/<lang>` for scripts that
    /// live inside .rpa archives). `to_default_language` writes those
    /// files into `tl/None` instead, applying them to the game's original
    /// language — for games that ship no language selector.
    /// Thai-capable font exports may use separate scales for dialogue and
    /// general UI text.
    fn export(
        &self,
        path: &Path,
        translations: &[TranslationEntry],
        target_language: &str,
        to_default_language: bool,
        thai_dialogue_font_scale_percent: f32,
        thai_ui_font_scale_percent: f32,
    ) -> Result<ExportReport>;

    /// Protected tokens (placeholders/tags) that must survive translation.
    /// Default: none. Engines override this.
    fn protected_tokens(&self, text: &str) -> Vec<String> {
        let _ = text;
        Vec::new()
    }
}

/// The engines compiled into the application. Registration point for future
/// engines (Unity, RPG Maker, ...): add one line here.
pub fn registry() -> &'static [&'static dyn GameEngine] {
    static REGISTRY: OnceLock<Vec<&'static dyn GameEngine>> = OnceLock::new();
    REGISTRY.get_or_init(|| vec![&crate::engine::renpy::RENPY_ENGINE])
}

/// Find the engine that recognizes `path`, if any.
pub fn detect_engine(path: &Path) -> Option<&'static dyn GameEngine> {
    registry().iter().copied().find(|e| e.detect(path))
}

/// Display name for an engine id.
pub fn engine_display_name(engine_id: &str) -> &str {
    registry()
        .iter()
        .find(|e| e.id() == engine_id)
        .map(|e| e.name())
        .unwrap_or(engine_id)
}

/// Root directory that contains the engine's script files.
pub fn game_root(path: &Path) -> PathBuf {
    let game = path.join("game");
    if game.is_dir() {
        game
    } else {
        path.to_path_buf()
    }
}
