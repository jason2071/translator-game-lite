pub mod extractor;
pub mod exporter;
pub mod parser;
pub mod rpa;

use std::path::Path;

use anyhow::Result;

use crate::core::engine::{ExportReport, ExtractionResult, GameEngine};
use crate::core::translation::TranslationEntry;

/// Ren'Py engine adapter — the only place where Ren'Py specifics exist.
pub struct RenpyEngine;

/// Stateless singleton, referenced by the engine registry.
pub static RENPY_ENGINE: RenpyEngine = RenpyEngine;

impl RenpyEngine {
    pub fn new() -> Self {
        Self
    }
}

impl Default for RenpyEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl GameEngine for RenpyEngine {
    fn id(&self) -> &'static str {
        "renpy"
    }

    fn name(&self) -> &'static str {
        "Ren'Py"
    }

    fn detect(&self, path: &Path) -> bool {
        let root = crate::core::engine::game_root(path);
        // Ren'Py projects either have loose .rpy scripts anywhere under the
        // game directory, or pack everything into .rpa archives.
        has_rpy_under(&root, 0) || has_rpa_at_top(&root)
    }

    fn extract(&self, path: &Path) -> Result<ExtractionResult> {
        let root = crate::core::engine::game_root(path);
        if !root.is_dir() {
            anyhow::bail!("game directory not found: {}", root.display());
        }
        extractor::extract(&root)
    }

    fn export(
        &self,
        path: &Path,
        translations: &[TranslationEntry],
        target_language: &str,
    ) -> Result<ExportReport> {
        let root = crate::core::engine::game_root(path);
        if !root.is_dir() {
            anyhow::bail!("game directory not found: {}", root.display());
        }
        exporter::export(&root, translations, target_language)
    }

    fn protected_tokens(&self, text: &str) -> Vec<String> {
        parser::protected_tokens(text)
    }
}

/// Any `.rpy` file under `dir` (bounded depth to keep scanning cheap).
fn has_rpy_under(dir: &Path, depth: u32) -> bool {
    if depth > 6 {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name == "saves" || name == "cache" || name == ".git" {
                continue;
            }
            if has_rpy_under(&path, depth + 1) {
                return true;
            }
        } else if path.extension().map(|e| e == "rpy").unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Any `.rpa` archive directly inside `dir`.
fn has_rpa_at_top(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|e| e.path().extension().map(|x| x == "rpa").unwrap_or(false))
        })
        .unwrap_or(false)
}
