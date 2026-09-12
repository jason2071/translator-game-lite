//! Writes translations back into Ren'Py `.rpy` files.
//!
//! Only the string literal of a translated line is replaced — indentation,
//! statement keywords, trailing clauses and comments stay byte-identical,
//! as do every untouched line and the file's line-ending style.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::core::engine::ExportReport;
use crate::core::translation::TranslationEntry;

use super::parser::{escape, keyword_is, keyword_string, scan_string};

const THAI_FONT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/fonts/IBMPlexSansThai-Regular.ttf"
));
const THAI_FONT_OVERRIDE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/renpy/01_gtl_thai_font.rpy"
));

struct RawLine {
    /// Line content without its terminator.
    text: String,
    /// `\n` or `\r\n`, preserved on write.
    eol: &'static str,
}

fn split_preserving_endings(content: &str) -> Vec<RawLine> {
    content
        .split_inclusive('\n')
        .map(|chunk| {
            if let Some(stripped) = chunk.strip_suffix("\r\n") {
                RawLine {
                    text: stripped.to_string(),
                    eol: "\r\n",
                }
            } else if let Some(stripped) = chunk.strip_suffix('\n') {
                RawLine {
                    text: stripped.to_string(),
                    eol: "\n",
                }
            } else {
                RawLine {
                    text: chunk.to_string(),
                    eol: "",
                }
            }
        })
        .collect()
}

/// Locate the requested double-quoted string on a line. The returned span
/// includes both quotes.
fn scan_string_at(raw: &str, string_index: usize) -> Option<(usize, usize, String)> {
    let mut offset = 0usize;
    for current_index in 0..=string_index {
        let (start, end, text) = scan_string(&raw[offset..])?;
        if current_index == string_index {
            return Some((offset + start, offset + end, text));
        }
        offset += end;
    }
    None
}

/// Replace one string literal on `raw`, keeping everything else on the line
/// intact. `None` when that string does not exist.
fn rewrite_string_span(raw: &str, string_index: usize, new_text: &str) -> Option<String> {
    let (start, end, _current) = scan_string_at(raw, string_index)?;
    Some(format!(
        "{}\"{}\"{}",
        &raw[..start],
        escape(new_text),
        &raw[end..]
    ))
}

/// Script-only strings can share one line (for example,
/// `Quest("Title", "Description")`). Their local IDs end in `#<index>`;
/// dialogue and menu entries retain their historic `path|line` IDs.
fn string_index_from_source_id(source_id: &str) -> usize {
    source_id
        .rsplit('|')
        .next()
        .and_then(|part| part.split_once('#'))
        .and_then(|(_, index)| index.parse().ok())
        .unwrap_or(0)
}

/// Apply translations to the game files under `game_root`.
///
/// Entries from loose `.rpy` files are rewritten in place. Entries that came
/// from inside an `.rpa` archive (virtual paths like `archive.rpa!script.rpy`)
/// cannot be written back into the archive — they are exported as
/// `translate <language> strings:` old/new pairs into `tl/<language>/`, which
/// Ren'Py applies at runtime (including to dialogue without its own
/// translate block).
#[cfg(test)]
pub fn export(
    game_root: &Path,
    translations: &[TranslationEntry],
    target_language: &str,
    to_default_language: bool,
) -> Result<ExportReport> {
    export_with_thai_font_scales(
        game_root,
        translations,
        target_language,
        to_default_language,
        100.0,
        80.0,
    )
}

/// Variant of [`export`] that lets callers independently choose the dialogue
/// and UI scales of the bundled Thai font. Values are clamped to 50–150%.
pub fn export_with_thai_font_scales(
    game_root: &Path,
    translations: &[TranslationEntry],
    target_language: &str,
    to_default_language: bool,
    thai_dialogue_font_scale_percent: f32,
    thai_ui_font_scale_percent: f32,
) -> Result<ExportReport> {
    let mut with_text: Vec<&TranslationEntry> = Vec::new();
    for t in translations {
        if t.translated_text.as_deref().unwrap_or("").trim().is_empty() {
            continue;
        }
        with_text.push(t);
    }
    let (virtual_entries, real_entries): (Vec<_>, Vec<_>) = with_text
        .into_iter()
        .partition(|t| t.source.file_path.contains('!'));

    let mut report = export_in_place(game_root, &real_entries)?;
    let (files, written) = export_virtual(
        game_root,
        &virtual_entries,
        target_language,
        to_default_language,
    )?;
    report.files_written += files;
    report.entries_written += written;

    if to_default_language {
        // tl/None only covers screens/system strings. To translate dialogue
        // in games without a language selector, also refresh the
        // language-named strings file and force the language on startup.
        let (f2, w2) = export_virtual(game_root, &virtual_entries, target_language, false)?;
        report.files_written += f2;
        report.entries_written += w2;

        let lang = sanitize_dir_name(target_language);
        let force = game_root
            .join("tl")
            .join("None")
            .join("00_gtl_apply_language.rpy");
        std::fs::write(
            &force,
            format!(
                "# Written by Game Translator Lite: this game has no language\n\
                 # selector, so the translation is applied automatically.\n\
                 init 999 python:\n\
                 \x20   persistent.language = \"{lang}\"\n\
                 \x20   try:\n\
                 \x20       renpy.change_language(\"{lang}\")\n\
                 \x20   except Exception:\n\
                 \x20       pass\n"
            ),
        )?;
    } else {
        // Leaving None mode: remove the language-forcing file.
        let _ = std::fs::remove_file(
            game_root
                .join("tl")
                .join("None")
                .join("00_gtl_apply_language.rpy"),
        );
    }

    if target_language.eq_ignore_ascii_case("thai") {
        install_thai_font(
            game_root,
            thai_dialogue_font_scale_percent,
            thai_ui_font_scale_percent,
        )?;
    }
    Ok(report)
}

/// Install a bundled Thai-capable font and apply it after the game's own GUI
/// styles have initialized. Archive-packed games often bundle a Latin-only
/// font, which otherwise renders Thai translations as square glyphs.
fn install_thai_font(
    game_root: &Path,
    thai_dialogue_font_scale_percent: f32,
    thai_ui_font_scale_percent: f32,
) -> Result<usize> {
    let dir = game_root.join("tl").join("None");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("gtl_thai_font.ttf"), THAI_FONT)?;
    let scale = |percent: f32, default: f32| {
        if percent.is_finite() {
            percent.clamp(50.0, 150.0) / 100.0
        } else {
            default
        }
    };
    let dialogue_scale = scale(thai_dialogue_font_scale_percent, 1.0);
    let ui_scale = scale(thai_ui_font_scale_percent, 0.8);
    std::fs::write(
        dir.join("01_gtl_thai_font.rpy"),
        THAI_FONT_OVERRIDE
            .replace(
                "__GTL_DIALOGUE_FONT_SCALE__",
                &format!("{dialogue_scale:.2}"),
            )
            .replace("__GTL_UI_FONT_SCALE__", &format!("{ui_scale:.2}")),
    )?;
    Ok(2)
}

/// Write archive-sourced translations into `tl/<language>/<archive>_gtl.rpy`
/// as old/new string pairs, merging with whatever is already there.
/// With `to_default_language` the file lands in `tl/None` (header
/// `translate None strings:`), applying to the game's original language —
/// for games without a language selector.
/// Returns (files_written, entries_written).
fn export_virtual(
    game_root: &Path,
    entries: &[&TranslationEntry],
    target_language: &str,
    to_default_language: bool,
) -> Result<(usize, usize)> {
    if entries.is_empty() {
        return Ok((0, 0));
    }
    let language = if to_default_language {
        "None".to_string()
    } else {
        sanitize_dir_name(target_language)
    };
    let mut by_archive: std::collections::BTreeMap<&str, Vec<&TranslationEntry>> =
        std::collections::BTreeMap::new();
    for t in entries {
        let archive = t.source.file_path.split('!').next().unwrap_or_default();
        by_archive.entry(archive).or_default().push(t);
    }

    let mut files_written = 0usize;
    let mut entries_written = 0usize;
    let dir = game_root.join("tl").join(&language);
    std::fs::create_dir_all(&dir)?;

    // Ren'Py aborts at load time when two files declare the same `old`
    // string for one language. Collect everything already claimed by
    // sibling files (e.g. tl/None/common.rpym) so those strings are
    // skipped below.
    let mut claimed: std::collections::HashMap<
        std::path::PathBuf,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.ends_with(".rpy") || name.ends_with(".rpym")) {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(e.path()) {
                let olds: std::collections::HashSet<String> = parse_old_new_pairs(&content)
                    .into_iter()
                    .map(|(old, _)| old)
                    .collect();
                claimed.insert(e.path(), olds);
            }
        }
    }
    // Olds written for earlier archives in this same run also count.
    let mut runtime_taken: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (archive, list) in by_archive {
        // One translation per original string; the lowest line wins.
        let mut wanted: std::collections::BTreeMap<String, &str> =
            std::collections::BTreeMap::new();
        let mut ordered: Vec<&&TranslationEntry> = list.iter().collect();
        ordered.sort_by_key(|t| t.source.line);
        for t in ordered {
            wanted
                .entry(t.source.source_text.clone())
                .or_insert(t.translated_text.as_deref().unwrap_or(""));
        }
        if wanted.is_empty() {
            continue;
        }

        let stem = archive.strip_suffix(".rpa").unwrap_or(archive);
        let path = dir.join(format!("{}_gtl.rpy", sanitize_dir_name(stem)));
        let taken: std::collections::HashSet<String> = claimed
            .iter()
            .filter(|(p, _)| *p != &path)
            .flat_map(|(_, olds)| olds.iter().cloned())
            .chain(runtime_taken.iter().cloned())
            .collect();

        // Existing pairs keep their order; ones we supply are updated in
        // place, everything else is appended. Pairs claimed by sibling
        // files are dropped — they would crash the game on load.
        let mut pairs: Vec<(String, String)> = Vec::new();
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for (old, new) in parse_old_new_pairs(&existing) {
                if taken.contains(&old) {
                    continue;
                }
                let new = match wanted.get(&old) {
                    Some(ours) => (*ours).to_string(),
                    None => new.unwrap_or_default(),
                };
                pairs.push((old, new));
            }
        }
        for (old, new) in &wanted {
            if taken.contains(old) || pairs.iter().any(|(o, _)| o == old) {
                continue;
            }
            pairs.push((old.clone(), (*new).to_string()));
        }
        for (old, _) in &pairs {
            runtime_taken.insert(old.clone());
        }

        let mut out = String::new();
        if out.is_empty() && !pairs.is_empty() {
            out.push_str(&format!("translate {} strings:\n", language));
        }
        for (old, new) in &pairs {
            out.push_str(&format!(
                "\n    old \"{}\"\n    new \"{}\"\n",
                escape(old),
                escape(new)
            ));
        }
        std::fs::write(&path, out)?;
        files_written += 1;
        entries_written += wanted.len();
    }
    Ok((files_written, entries_written))
}

fn sanitize_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if cleaned.trim().is_empty() {
        "translated".to_string()
    } else {
        cleaned
    }
}

/// Sequential `old`/`new` pairs of a translation file.
fn parse_old_new_pairs(content: &str) -> Vec<(String, Option<String>)> {
    let mut pairs = Vec::new();
    let mut pending: Option<String> = None;
    for parsed in super::parser::parse(content) {
        match parsed.kind {
            super::parser::LineKind::Old { text } => {
                if let Some(old) = pending.take() {
                    pairs.push((old, None));
                }
                pending = Some(text);
            }
            super::parser::LineKind::New { text } => {
                if let Some(old) = pending.take() {
                    pairs.push((old, Some(text)));
                }
            }
            _ => {}
        }
    }
    if let Some(old) = pending.take() {
        pairs.push((old, None));
    }
    pairs
}

fn export_in_place(game_root: &Path, translations: &[&TranslationEntry]) -> Result<ExportReport> {
    let mut by_file: HashMap<&str, Vec<&TranslationEntry>> = HashMap::new();
    for t in translations {
        by_file
            .entry(t.source.file_path.as_str())
            .or_default()
            .push(t);
    }
    // Sort keys for deterministic file processing.
    let mut files: Vec<(&str, Vec<&TranslationEntry>)> = by_file.into_iter().collect();
    files.sort_by_key(|(f, _)| *f);

    let mut report = ExportReport::default();

    for (rel_path, entries) in files {
        let path = game_root.join(rel_path);
        let Ok(content) = std::fs::read_to_string(&path) else {
            report.entries_skipped += entries.len();
            continue;
        };
        let mut lines = split_preserving_endings(&content);

        // Descending line order: inserting a `new` line for an `old`
        // without translation does not shift any yet-unprocessed entry.
        let mut ordered: Vec<&&TranslationEntry> = entries.iter().collect();
        ordered.sort_by_key(|t| std::cmp::Reverse(t.source.line));

        let mut modified = false;
        for t in ordered {
            let idx = (t.source.line as usize).saturating_sub(1);
            let Some(raw) = lines.get(idx).map(|l| l.text.clone()) else {
                report.entries_skipped += 1;
                continue;
            };
            let trimmed = raw.trim_start();
            // Normalize stray spaces so game files stay clean.
            let translated =
                &crate::core::source::clean_spaces(t.translated_text.as_deref().unwrap_or(""));
            let original = t.source.source_text.as_str();

            if keyword_string(trimmed, "old").as_deref() == Some(original) {
                // Translation target is the adjacent `new "..."` line.
                let mut applied = false;
                let last = (idx + 3).min(lines.len().saturating_sub(1));
                for j in idx + 1..=last {
                    let next_trimmed = lines[j].text.trim_start();
                    if keyword_string(next_trimmed, "new").is_some() {
                        if let Some(new_raw) = rewrite_string_span(&lines[j].text, 0, translated) {
                            lines[j].text = new_raw;
                            applied = true;
                        }
                        break;
                    }
                }
                if !applied {
                    // No `new` line exists yet — insert one after the `old`.
                    let indent_len = raw.len() - trimmed.len();
                    let inserted = format!("{}new \"{}\"", &raw[..indent_len], escape(translated));
                    lines.insert(
                        idx + 1,
                        RawLine {
                            text: inserted,
                            eol: lines[idx].eol,
                        },
                    );
                }
                report.entries_written += 1;
                modified = true;
                continue;
            }

            // An old/new line whose text no longer matches is never touched.
            if keyword_is(trimmed, "old").is_some() || keyword_is(trimmed, "new").is_some() {
                report.entries_skipped += 1;
                continue;
            }

            // say / menu choice: replace the string in place, but only if
            // the file still matches what was scanned.
            let string_index = string_index_from_source_id(&t.source.id);
            let matches_original = scan_string_at(trimmed, string_index)
                .map(|(_s, _e, current)| current == original)
                .unwrap_or(false);
            if !matches_original {
                report.entries_skipped += 1;
                continue;
            }
            if let Some(new_raw) = rewrite_string_span(&raw, string_index, translated) {
                lines[idx].text = new_raw;
                report.entries_written += 1;
                modified = true;
            } else {
                report.entries_skipped += 1;
            }
        }

        if modified {
            let out: String = lines
                .into_iter()
                .map(|l| {
                    let mut s = l.text;
                    s.push_str(l.eol);
                    s
                })
                .collect();
            std::fs::write(&path, out)?;
            report.files_written += 1;
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::source::{hash_text, SourceEntry};
    use crate::core::translation::TranslationStatus;
    use std::fs;

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gtl-export-{}-{}-{}",
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

    fn entry(rel: &str, line: u32, original: &str, translated: &str) -> TranslationEntry {
        TranslationEntry {
            source: SourceEntry {
                id: format!("{rel}|{line}"),
                engine_id: "renpy".into(),
                file_path: rel.into(),
                line,
                speaker: None,
                source_hash: hash_text(original),
                source_text: original.into(),
                context: None,
            },
            translated_text: Some(translated.into()),
            status: TranslationStatus::Translated,
            updated_at: 0,
        }
    }

    fn script_entry(
        rel: &str,
        line: u32,
        string_index: usize,
        original: &str,
        translated: &str,
    ) -> TranslationEntry {
        let mut entry = entry(rel, line, original, translated);
        entry.source.id = format!("{rel}|{line}#{string_index}");
        entry
    }

    #[test]
    fn rewrites_say_lines_in_place() {
        let root = temp_root("say");
        let file = root.join("script.rpy");
        fs::write(
            &file,
            "define e = Character(\"Eileen\")\nlabel start:\n    e \"Hello [player_name]!\"\n    \"Fine.\"\n",
        )
        .unwrap();

        let report = export(
            &root,
            &[
                entry(
                    "script.rpy",
                    3,
                    "Hello [player_name]!",
                    "สวัสดี [player_name]!",
                ),
                entry("script.rpy", 4, "Fine.", "โอเค"),
            ],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.files_written, 1);
        assert_eq!(report.entries_written, 2);
        assert_eq!(report.entries_skipped, 0);

        let out = fs::read_to_string(&file).unwrap();
        assert_eq!(
            out,
            "define e = Character(\"Eileen\")\nlabel start:\n    e \"สวัสดี [player_name]!\"\n    \"โอเค\"\n"
        );
    }

    #[test]
    fn rewrites_quest_and_screen_strings_at_their_exact_positions() {
        let root = temp_root("quest");
        let file = root.join("quests.rpy");
        fs::write(
            &file,
            concat!(
                "$ student_life = Quest(\"Student Life\", \"Study and socialize.\")\n",
                "$ student_life.add_objective(\"Attend class.\", visible=True)\n",
                "text \"Quest Log\":\n",
            ),
        )
        .unwrap();

        let report = export(
            &root,
            &[
                script_entry("quests.rpy", 1, 0, "Student Life", "ชีวิตนักศึกษา"),
                script_entry("quests.rpy", 1, 1, "Study and socialize.", "เรียนและเข้าสังคม"),
                script_entry("quests.rpy", 2, 0, "Attend class.", "เข้าเรียน"),
                script_entry("quests.rpy", 3, 0, "Quest Log", "บันทึกภารกิจ"),
            ],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.entries_written, 4);
        let out = fs::read_to_string(file).unwrap();
        assert!(out.contains("Quest(\"ชีวิตนักศึกษา\", \"เรียนและเข้าสังคม\")"));
        assert!(out.contains("add_objective(\"เข้าเรียน\", visible=True)"));
        assert!(out.contains("text \"บันทึกภารกิจ\":"));
    }

    #[test]
    fn rewrites_adjacent_new_line_for_old_entries() {
        let root = temp_root("oldnew");
        let file = root.join("tl/thai/script.rpy");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "translate thai strings:\n    old \"Hello\"\n    new \"\"\n\ntranslate thai start_x:\n    old \"Bye\"\n",
        )
        .unwrap();

        let report = export(
            &root,
            &[
                entry("tl/thai/script.rpy", 2, "Hello", "สวัสดี"),
                entry("tl/thai/script.rpy", 6, "Bye", "ลาก่อน"),
            ],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.entries_written, 2);
        let out = fs::read_to_string(&file).unwrap();
        assert_eq!(
            out,
            "translate thai strings:\n    old \"Hello\"\n    new \"สวัสดี\"\n\ntranslate thai start_x:\n    old \"Bye\"\n    new \"ลาก่อน\"\n"
        );
    }

    #[test]
    fn skips_entries_when_file_content_changed() {
        let root = temp_root("stale");
        let file = root.join("script.rpy");
        fs::write(&file, "e \"Hello\"\n").unwrap();

        let report = export(
            &root,
            &[entry("script.rpy", 1, "Outdated", "เดิม")],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.entries_skipped, 1);
        assert_eq!(report.entries_written, 0);
        assert_eq!(fs::read_to_string(&file).unwrap(), "e \"Hello\"\n");
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_output() {
        let root = temp_root("escape");
        fs::write(root.join("s.rpy"), "e \"He said \\\"hi\\\"\"\n").unwrap();

        export(
            &root,
            &[entry("s.rpy", 1, "He said \"hi\"", "เขาพูดว่า \"ไฮ\"")],
            "Thai",
            false,
        )
        .unwrap();

        let out = fs::read_to_string(root.join("s.rpy")).unwrap();
        assert_eq!(out, "e \"เขาพูดว่า \\\"ไฮ\\\"\"\n");
    }

    #[test]
    fn preserves_crlf_and_empty_translations_are_ignored() {
        let root = temp_root("crlf");
        fs::write(root.join("s.rpy"), "e \"A\"\r\n# keep\r\n").unwrap();

        let report = export(
            &root,
            &[entry("s.rpy", 1, "A", "เอ"), entry("s.rpy", 2, "gone", "x")],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.entries_written, 1);
        assert_eq!(report.entries_skipped, 1); // line 2 has no string
        let out = fs::read_to_string(root.join("s.rpy")).unwrap();
        assert_eq!(out, "e \"เอ\"\r\n# keep\r\n");
    }

    #[test]
    fn missing_file_counts_as_skipped() {
        let root = temp_root("missing");
        let report = export(&root, &[entry("gone.rpy", 1, "A", "เอ")], "Thai", false).unwrap();
        assert_eq!(report.entries_skipped, 1);
        assert_eq!(report.files_written, 0);
    }

    #[test]
    fn archive_entries_export_to_tl_strings_file() {
        let root = temp_root("rpaexport");
        // Loose files still go in place; archive members (virtual paths)
        // must end up in tl/<lang>/ as old/new pairs.
        fs::write(root.join("script.rpy"), "e \"Loose\"\n").unwrap();

        let report = export(
            &root,
            &[
                entry("script.rpy", 1, "Loose", "หลวม"),
                entry("archive.rpa!script.rpy", 5, "Hello [name]", "สวัสดี [name]"),
                entry("archive.rpa!aisha.rpy", 12, "Hi there.", "ไง"),
                entry("archive.rpa!aisha.rpy", 30, "Hello [name]", "สวัสดี [name]"),
            ],
            "thai",
            false,
        )
        .unwrap();

        // 1 loose entry + 2 unique archive pairs (the duplicate is merged).
        assert_eq!(report.entries_written, 3);
        assert_eq!(report.files_written, 2);

        let out = fs::read_to_string(root.join("tl/thai/archive_gtl.rpy")).unwrap();
        assert_eq!(
            out,
            "translate thai strings:\n\n    old \"Hello [name]\"\n    new \"สวัสดี [name]\"\n\n    old \"Hi there.\"\n    new \"ไง\"\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("script.rpy")).unwrap(),
            "e \"หลวม\"\n"
        );
    }

    #[test]
    fn archive_export_merges_with_existing_tl_file() {
        let root = temp_root("rpamerge");
        let tl = root.join("tl/Thai/archive_gtl.rpy");
        fs::create_dir_all(tl.parent().unwrap()).unwrap();
        fs::write(
            &tl,
            "translate Thai strings:\n\n    old \"Old pair\"\n    new \"เก่า\"\n\n    old \"Hello\"\n    new \"\"\n",
        )
        .unwrap();

        let report = export(
            &root,
            &[
                entry("archive.rpa!s.rpy", 1, "Hello", "สวัสดี"),
                entry("archive.rpa!s.rpy", 2, "Brand new", "ใหม่"),
            ],
            "Thai",
            false,
        )
        .unwrap();

        assert_eq!(report.entries_written, 2);
        let out = fs::read_to_string(&tl).unwrap();
        assert_eq!(
            out,
            "translate Thai strings:\n\n    old \"Old pair\"\n    new \"เก่า\"\n\n    old \"Hello\"\n    new \"สวัสดี\"\n\n    old \"Brand new\"\n    new \"ใหม่\"\n"
        );
    }

    #[test]
    fn to_default_language_writes_tl_none() {
        let root = temp_root("rp.none");
        let report = export(
            &root,
            &[
                entry("archive.rpa!s.rpy", 1, "Hello", "สวัสดี"),
                entry("archive.rpa!s.rpy", 2, "Brand new", "ใหม่"),
            ],
            "Thai",
            true,
        )
        .unwrap();

        assert_eq!(report.files_written, 2);
        let none_path = root.join("tl/None/archive_gtl.rpy");
        let out = fs::read_to_string(&none_path).unwrap();
        assert_eq!(
            out,
            "translate None strings:\n\n    old \"Brand new\"\n    new \"ใหม่\"\n\n    old \"Hello\"\n    new \"สวัสดี\"\n"
        );
        let thai_path = root.join("tl/Thai/archive_gtl.rpy");
        assert!(thai_path.exists());
        let force = fs::read_to_string(root.join("tl/None/00_gtl_apply_language.rpy")).unwrap();
        assert!(force.contains("renpy.change_language(\"Thai\")"), "{force}");
        assert_eq!(
            fs::read(root.join("tl/None/gtl_thai_font.ttf")).unwrap(),
            THAI_FONT
        );
        let font_script = fs::read_to_string(root.join("tl/None/01_gtl_thai_font.rpy")).unwrap();
        assert!(font_script.contains("config.font_replacement_map"));
        assert!(font_script.contains("1.00"), "{font_script}");
        assert!(font_script.contains("0.80"), "{font_script}");
    }

    #[test]
    fn thai_font_scales_are_written_and_clamped() {
        let root = temp_root("rp.font-scale");
        export_with_thai_font_scales(
            &root,
            &[entry("archive.rpa!s.rpy", 1, "Hello", "สวัสดี")],
            "Thai",
            false,
            200.0,
            10.0,
        )
        .unwrap();

        let script = fs::read_to_string(root.join("tl/None/01_gtl_thai_font.rpy")).unwrap();
        assert!(script.contains("1.50"), "{script}");
        assert!(script.contains("0.50"), "{script}");
        assert!(
            !script.contains("__GTL_DIALOGUE_FONT_SCALE__"),
            "{script}"
        );
        assert!(!script.contains("__GTL_UI_FONT_SCALE__"), "{script}");
    }

    #[test]
    fn skips_strings_already_translated_by_sibling_files() {
        let root = temp_root("rp.dup");
        let none = root.join("tl/None");
        fs::create_dir_all(&none).unwrap();
        fs::write(
            none.join("common.rpym"),
            "translate None strings:\n\n    old \"Are you sure?\"\n    new \"Are you sure?\"\n",
        )
        .unwrap();

        export(
            &root,
            &[
                entry("archive.rpa!s.rpy", 1, "Are you sure?", "คุณแน่ใจนะ"),
                entry("archive.rpa!s.rpy", 2, "Brand new", "ใหม่"),
            ],
            "Thai",
            true,
        )
        .unwrap();

        let out = fs::read_to_string(none.join("archive_gtl.rpy")).unwrap();
        assert!(out.contains("Brand new"), "{out}");
        // The conflicting string must be dropped, or Ren'Py aborts on load.
        assert!(!out.contains("Are you sure?"), "{out}");
    }

    #[test]
    fn duplicate_strings_across_archives_are_written_once() {
        let root = temp_root("rp.cross");
        export(
            &root,
            &[
                entry("a.rpa!s.rpy", 1, "Same line", "เหมือนกัน"),
                entry("b.rpa!t.rpy", 1, "Same line", "เหมือนกัน"),
            ],
            "Thai",
            false,
        )
        .unwrap();

        // Both gtl files exist (one per archive), but the second must not
        // re-declare the shared string.
        let a = fs::read_to_string(root.join("tl/Thai/a_gtl.rpy")).unwrap();
        let b = fs::read_to_string(root.join("tl/Thai/b_gtl.rpy")).unwrap();
        assert!(a.contains("Same line"));
        assert!(!b.contains("Same line"), "{b}");
    }
}
