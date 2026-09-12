//! Walks a Ren'Py game directory and turns parsed script lines into
//! engine-independent [`SourceEntry`] values.
//!
//! Sources come from two places:
//! - loose `.rpy` files under the game directory,
//! - `.rpy` members inside `.rpa` archives (virtual paths like
//!   `archive.rpa!script.rpy`).
//!
//! Entries get *local* ids (`"{rel_path}|{line}"`). The project scan layer
//! prefixes the project id to make them globally unique in SQLite.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::engine::{ExistingTranslation, ExtractionResult};
use crate::core::source::{hash_text, SourceEntry};

use super::parser::{self, LineKind};
use super::rpa::RpaIndex;

/// Extract every translatable string from the scripts under `game_root`.
pub fn extract(game_root: &Path) -> Result<ExtractionResult> {
    let mut files = Vec::new();
    collect_rpy_files(game_root, &mut files)?;
    files.sort();

    // Archive members are logical "files" with virtual paths.
    let mut archives: Vec<(PathBuf, RpaIndex, Vec<String>)> = Vec::new();
    for rpa_path in collect_rpa_files(game_root)? {
        let index =
            RpaIndex::open(&rpa_path).with_context(|| format!("reading {}", rpa_path.display()))?;
        let mut scripts = index.names_with_extension(".rpy");
        scripts.sort();
        archives.push((rpa_path, index, scripts));
    }

    // Pass 1: character definitions (`define e = Character("Eileen")`),
    // accumulated across everything so any script can use any character.
    let mut speakers: HashMap<String, String> = HashMap::new();
    for file in &files {
        let content = read_lossy(file);
        for (var, name) in character_definitions(&content) {
            speakers.insert(var, name);
        }
    }
    for (rpa_path, index, scripts) in &archives {
        for name in scripts {
            let Ok(content) = index.read_member(rpa_path, name) else {
                continue;
            };
            for (var, name) in character_definitions(&content) {
                speakers.insert(var, name);
            }
        }
    }

    // Pass 2: extraction.
    let mut result = ExtractionResult::default();
    for file in &files {
        let rel = file
            .strip_prefix(game_root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let content = read_lossy(file);
        extract_content(&content, &rel, &speakers, &mut result);
    }
    for (rpa_path, index, scripts) in &archives {
        let archive_name = rpa_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        for name in scripts {
            let Ok(content) = index.read_member(rpa_path, name) else {
                continue;
            };
            let rel = format!("{}!{}", archive_name, name.replace('\\', "/"));
            extract_content(&content, &rel, &speakers, &mut result);
        }
    }

    result
        .sources
        .sort_by(|a, b| (&a.file_path, a.line).cmp(&(&b.file_path, b.line)));
    Ok(result)
}

/// Classify one script file's content and append its entries.
fn extract_content(
    content: &str,
    rel: &str,
    speakers: &HashMap<String, String>,
    result: &mut ExtractionResult,
) {
    let mut scene: Option<String> = None;
    // `old` waiting for its `new` line: (lineno, text, scene at old line)
    let mut pending_old: Option<(u32, String, Option<String>)> = None;

    for parsed in parser::parse(content) {
        match parsed.kind {
            LineKind::Label { name } => scene = Some(name),
            LineKind::Say { speaker, text } => {
                flush_old(&mut pending_old, result, rel);
                if text.trim().is_empty() {
                    // Translation skeletons (`translate ...:` blocks) contain
                    // empty say lines to be filled in — nothing to extract.
                    continue;
                }
                let display = speaker.and_then(|v| speakers.get(&v).cloned());
                result
                    .sources
                    .push(entry(rel, parsed.lineno, display, text, scene.clone()));
            }
            LineKind::Choice { text } => {
                flush_old(&mut pending_old, result, rel);
                if text.trim().is_empty() {
                    continue;
                }
                result
                    .sources
                    .push(entry(rel, parsed.lineno, None, text, scene.clone()));
            }
            LineKind::ScriptText { text, string_index } => {
                flush_old(&mut pending_old, result, rel);
                if text.trim().is_empty() {
                    continue;
                }
                result.sources.push(script_entry(
                    rel,
                    parsed.lineno,
                    None,
                    text,
                    scene.clone(),
                    string_index,
                ));
            }
            LineKind::Old { text } => {
                flush_old(&mut pending_old, result, rel);
                pending_old = Some((parsed.lineno, text, scene.clone()));
            }
            LineKind::New { text } => {
                if let Some((lineno, old_text, old_scene)) = pending_old.take() {
                    let entry = entry(rel, lineno, None, old_text, old_scene);
                    // Empty `new` lines in translation skeletons are not
                    // translations.
                    if !text.trim().is_empty() {
                        result.existing_translations.push(ExistingTranslation {
                            source_id: entry.id.clone(),
                            text,
                        });
                    }
                    result.sources.push(entry);
                }
                // `new` without a preceding `old` is ignored.
            }
            LineKind::Other => {}
        }
    }
    flush_old(&mut pending_old, result, rel);
}

fn flush_old(
    pending: &mut Option<(u32, String, Option<String>)>,
    result: &mut ExtractionResult,
    rel: &str,
) {
    if let Some((lineno, text, scene)) = pending.take() {
        result.sources.push(entry(rel, lineno, None, text, scene));
    }
}

fn entry(
    rel_path: &str,
    line: u32,
    speaker: Option<String>,
    text: String,
    scene: Option<String>,
) -> SourceEntry {
    SourceEntry {
        id: format!("{}|{}", rel_path, line),
        engine_id: "renpy".to_string(),
        file_path: rel_path.to_string(),
        line,
        speaker,
        source_hash: hash_text(&text),
        source_text: text,
        context: scene,
    }
}

fn script_entry(
    rel_path: &str,
    line: u32,
    speaker: Option<String>,
    text: String,
    scene: Option<String>,
    string_index: usize,
) -> SourceEntry {
    SourceEntry {
        id: format!("{}|{}#{}", rel_path, line, string_index),
        engine_id: "renpy".to_string(),
        file_path: rel_path.to_string(),
        line,
        speaker,
        source_hash: hash_text(&text),
        source_text: text,
        context: scene,
    }
}

/// `define e = Character("Eileen", ...)` (with or without `define`).
fn character_definitions(content: &str) -> Vec<(String, String)> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^[ \t]*(?:define\s+)?(\w+)\s*=\s*Character\s*\(").unwrap()
    });
    let mut out = Vec::new();
    for line in content.lines() {
        if let Some(caps) = re.captures(line) {
            // The first string after `Character(` is the display name.
            let rest = &line[caps.get(0).unwrap().end()..];
            if let Some((_start, _end, name)) = parser::scan_string(rest) {
                out.push((caps[1].to_string(), name));
            }
        }
    }
    out
}

fn read_lossy(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes[3..].to_vec()
    } else {
        bytes
    };
    String::from_utf8_lossy(&bytes).into_owned()
}

fn collect_rpy_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name == "saves" || name == "cache" || name == ".git" {
                continue;
            }
            collect_rpy_files(&path, out)?;
        } else if path.extension().map(|e| e == "rpy").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_rpa_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_rpa_files_inner(dir, &mut out, 0)?;
    Ok(out)
}

fn collect_rpa_files_inner(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) -> Result<()> {
    if depth > 3 {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name == "saves" || name == "cache" || name == ".git" {
                continue;
            }
            collect_rpa_files_inner(&path, out, depth + 1)?;
        } else if path.extension().map(|e| e == "rpa").unwrap_or(false) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gtl-extract-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
    }

    #[test]
    fn extracts_say_narrator_choices_and_old_new() {
        let root = temp_root("main");
        let game = root.join("game");
        write(
            &root,
            "game/script.rpy",
            concat!(
                "define e = Character(\"Eileen\")\n",
                "label start:\n",
                "    \"Narrator line.\"\n",
                "    e \"Hello [player_name]!\"\n",
                "    menu:\n",
                "        \"Go left\":\n",
                "            e \"Left it is.\"\n",
                "        \"Go right\" if flag:\n",
                "            pass\n",
            ),
        );
        write(
            &root,
            "game/tl/thai/script.rpy",
            "translate thai strings:\n    old \"Hello\"\n    new \"สวัสดี\"\n",
        );
        // Non-script noise that must be ignored.
        write(&root, "game/saves/junk.rpy", "e \"never\"\n");
        fs::write(root.join("game/compiled.rpyc"), b"binary").unwrap();

        let result = extract(&game).expect("extract");

        let texts: Vec<&str> = result
            .sources
            .iter()
            .map(|s| s.source_text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "Narrator line.",
                "Hello [player_name]!",
                "Go left",
                "Left it is.",
                "Go right",
                "Hello",
            ]
        );

        // Speaker display name resolved from the Character definition.
        let e_line = &result.sources[1];
        assert_eq!(e_line.speaker.as_deref(), Some("Eileen"));
        assert_eq!(e_line.context.as_deref(), Some("start"));
        assert_eq!(e_line.file_path, "script.rpy");
        assert_eq!(e_line.line, 4);

        // Existing `new` translation is attached to the `old` entry.
        assert_eq!(result.existing_translations.len(), 1);
        assert_eq!(result.existing_translations[0].text, "สวัสดี");
        assert_eq!(
            result.existing_translations[0].source_id,
            result.sources[5].id
        );

        // Hashing is text-based; local id is rel|line (the `old` line).
        assert_eq!(e_line.source_hash, hash_text("Hello [player_name]!"));
        assert_eq!(result.sources[5].id, "tl/thai/script.rpy|2");
    }

    #[test]
    fn old_without_new_still_becomes_an_entry() {
        let root = temp_root("oldonly");
        let game = root.join("game");
        write(
            &root,
            "game/tl/thai/extra.rpy",
            "translate thai strings:\n    old \"Goodbye\"\n",
        );
        let result = extract(&game).unwrap();
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].source_text, "Goodbye");
        assert!(result.existing_translations.is_empty());
    }

    #[test]
    fn consecutive_old_lines_do_not_lose_entries() {
        let root = temp_root("oldold");
        let game = root.join("game");
        write(
            &root,
            "game/tl/thai/x.rpy",
            "translate thai strings:\n    old \"A\"\n    old \"B\"\n    new \"บี\"\n",
        );
        let result = extract(&game).unwrap();
        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.existing_translations.len(), 1);
        assert_eq!(result.existing_translations[0].text, "บี");
        assert_eq!(result.existing_translations[0].source_id, "tl/thai/x.rpy|3");
    }

    #[test]
    fn extracts_quest_titles_descriptions_objectives_and_screen_text() {
        let root = temp_root("quest");
        let game = root.join("game");
        write(
            &root,
            "game/quests.rpy",
            concat!(
                "label quests:\n",
                "$ student_life = Quest(\"Student Life\", \"Study and socialize.\")\n",
                "$ student_life.add_objective(\"Attend class.\", visible=True)\n",
                "screen quest_log:\n",
                "    text \"Quest Log\":\n",
            ),
        );

        let result = extract(&game).unwrap();
        let texts: Vec<&str> = result
            .sources
            .iter()
            .map(|source| source.source_text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "Student Life",
                "Study and socialize.",
                "Attend class.",
                "Quest Log"
            ]
        );
        let ids: Vec<&str> = result.sources.iter().map(|source| source.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "quests.rpy|2#0",
                "quests.rpy|2#1",
                "quests.rpy|3#0",
                "quests.rpy|5#0"
            ]
        );
        assert!(result
            .sources
            .iter()
            .all(|source| source.context.as_deref() == Some("quests")));
    }

    #[test]
    fn extracts_from_rpa_archive_and_skips_skeleton_lines() {
        let root = temp_root("rpa");
        let game = root.join("game");
        fs::create_dir_all(&game).unwrap();
        let archive = game.join("archive.rpa");
        crate::engine::renpy::rpa::testutil::build_test_archive(
            &archive,
            &[(
                "script.rpy",
                "define e = Character(\"Eileen\")\nlabel start:\n    e \"From archive\"\n    e \"\"\n",
            )],
        );
        // A tl skeleton pair with an empty `new` must not be imported as a
        // translation.
        write(
            &root,
            "game/tl/thai/x.rpy",
            "translate thai strings:\n    old \"From archive\"\n    new \"\"\n",
        );

        let result = extract(&game).unwrap();

        let texts: Vec<&str> = result
            .sources
            .iter()
            .map(|s| s.source_text.as_str())
            .collect();
        assert_eq!(texts, vec!["From archive", "From archive"]);
        // Archive entry: speaker resolved from the define inside the archive.
        assert_eq!(result.sources[0].file_path, "archive.rpa!script.rpy");
        assert_eq!(result.sources[0].speaker.as_deref(), Some("Eileen"));
        assert_eq!(result.sources[0].line, 3);
        // The empty `e ""` skeleton line was skipped.
        assert_eq!(result.sources[1].file_path, "tl/thai/x.rpy");
        assert!(result.existing_translations.is_empty());
    }

    #[test]
    fn extracts_quest_texts_from_rpa_archives() {
        let root = temp_root("rpa-quest");
        let game = root.join("game");
        fs::create_dir_all(&game).unwrap();
        let archive = game.join("archive.rpa");
        crate::engine::renpy::rpa::testutil::build_test_archive(
            &archive,
            &[(
                "quests.rpy",
                concat!(
                    "label quests:\n",
                    "$ quest = Quest(\"Student Life\", \"Study and socialize.\")\n",
                    "$ quest.add_objective(\"Attend class.\", visible=True)\n",
                    "text \"Quest Log\":\n",
                ),
            )],
        );

        let result = extract(&game).unwrap();
        let texts: Vec<&str> = result
            .sources
            .iter()
            .map(|source| source.source_text.as_str())
            .collect();
        assert_eq!(
            texts,
            vec![
                "Student Life",
                "Study and socialize.",
                "Attend class.",
                "Quest Log"
            ]
        );
        assert!(result
            .sources
            .iter()
            .all(|source| source.file_path == "archive.rpa!quests.rpy"));
    }

    /// Probe against a real, archive-packed Ren'Py game when one is
    /// available on this machine. Run with:
    /// `cargo test real_game_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_game_probe() {
        use crate::core::engine::detect_engine;

        let root = std::path::Path::new(r"F:\Downloads\new\NothingWeirdHappensHere-v0.56-pc");
        if !root.exists() {
            eprintln!("probe game not present; skipping");
            return;
        }
        let engine = detect_engine(root).expect("detect");
        assert_eq!(engine.id(), "renpy");
        let result = engine.extract(root).expect("extract");
        println!("sources: {}", result.sources.len());
        println!(
            "existing translations: {}",
            result.existing_translations.len()
        );
        for s in result.sources.iter().take(10) {
            println!(
                "  [{}:{}] {:?}: {:?}",
                s.file_path, s.line, s.speaker, s.source_text
            );
        }
        assert!(result.sources.len() > 100);
    }

    #[test]
    fn engine_detect_and_extract_via_trait() {
        use crate::core::engine::detect_engine;
        let root = temp_root("detect");
        write(&root, "game/script.rpy", "e \"Hi\"\n");

        let engine = detect_engine(&root).expect("renpy should be detected");
        assert_eq!(engine.id(), "renpy");

        let result = engine.extract(&root).unwrap();
        assert_eq!(result.sources.len(), 1);
        assert_eq!(result.sources[0].source_text, "Hi");

        // Protected tokens are engine-provided.
        assert_eq!(
            engine.protected_tokens("Hi [name] {b}x{/b}"),
            vec!["[name]", "{b}", "{/b}"]
        );

        // A directory without .rpy files is not a Ren'Py project...
        let empty = temp_root("empty");
        assert!(detect_engine(&empty).is_none());

        // ...unless it packs everything into .rpa archives.
        let packed = temp_root("packed");
        fs::create_dir_all(packed.join("game")).unwrap();
        crate::engine::renpy::rpa::testutil::build_test_archive(
            &packed.join("game").join("archive.rpa"),
            &[("script.rpy", "e \"Packed\"\n")],
        );
        let engine = detect_engine(&packed).expect("archive-only game should be detected");
        assert_eq!(engine.id(), "renpy");
        let result = engine.extract(&packed).unwrap();
        assert_eq!(result.sources[0].source_text, "Packed");
        assert_eq!(result.sources[0].file_path, "archive.rpa!script.rpy");
    }
}
