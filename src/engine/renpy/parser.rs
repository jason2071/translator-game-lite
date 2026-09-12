//! Line-level parser for Ren'Py `.rpy` scripts.
//!
//! The parser classifies lines it can understand (say statements, menu
//! choices, `old`/`new` translation pairs, labels) and treats everything
//! else as opaque. It never rewrites files; the exporter does surgical
//! string-literal replacement using the line numbers recorded here.

/// Statement keywords that can precede a string literal. A line starting
/// with one of these (e.g. `return "x"`, `show image "bg"`) is never a say
/// statement.
const STATEMENT_KEYWORDS: &[&str] = &[
    "if",
    "elif",
    "else",
    "while",
    "for",
    "return",
    "call",
    "jump",
    "scene",
    "show",
    "hide",
    "play",
    "stop",
    "voice",
    "queue",
    "python",
    "default",
    "define",
    "menu",
    "label",
    "init",
    "transform",
    "style",
    "image",
    "pause",
    "with",
    "window",
    "nvl",
    "screen",
    "translate",
    "old",
    "new",
    "use",
    "add",
    "textbutton",
    "imagebutton",
    "hotspot",
    "vbox",
    "hbox",
    "frame",
    "fixed",
    "grid",
    "side",
    "bar",
    "key",
    "timer",
    "mousearea",
    "drag",
    "draggroup",
    "config",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    /// `e "Hello."`, `e happy "Hello."`, `extend "..."` or narrator `"Hello."`
    Say {
        speaker: Option<String>,
        text: String,
    },
    /// `"Choice text":` or `"Choice text" if cond:` inside a `menu:` block
    Choice { text: String },
    /// `old "Hello"` in a translate block
    Old { text: String },
    /// `new "สวัสดี"` in a translate block
    New { text: String },
    /// `label start:`
    Label { name: String },
    /// A string used to create or update a quest, objective, or screen text.
    /// `string_index` is the zero-based quoted-string position on its line.
    ScriptText { text: String, string_index: usize },
    /// Anything the v1 parser does not translate (python, show, jump, ...)
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedLine {
    /// 1-based line number in the file.
    pub lineno: u32,
    /// Leading indentation in spaces.
    pub indent: usize,
    pub kind: LineKind,
}

/// Classify every meaningful line of a `.rpy` file.
pub fn parse(content: &str) -> Vec<ParsedLine> {
    let mut out = Vec::new();
    let mut in_triple_quote = false;

    for (idx, raw) in content.lines().enumerate() {
        let lineno = (idx + 1) as u32;

        // Skip bodies of triple-quoted strings (rare in dialogue scripts).
        if raw.matches("\"\"\"").count() % 2 == 1 {
            in_triple_quote = !in_triple_quote;
            continue;
        }
        if in_triple_quote {
            continue;
        }

        let trimmed = raw.trim_start();
        let indent = raw.len() - trimmed.len();
        for kind in classify_all(trimmed) {
            if kind != LineKind::Other {
                out.push(ParsedLine {
                    lineno,
                    indent,
                    kind,
                });
            }
        }
    }
    out
}

fn classify_all(trimmed: &str) -> Vec<LineKind> {
    let primary = classify(trimmed);
    if primary != LineKind::Other {
        return vec![primary];
    }
    quest_and_screen_texts(trimmed)
}

fn classify(trimmed: &str) -> LineKind {
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return LineKind::Other;
    }
    if let Some(text) = keyword_string(trimmed, "old") {
        return LineKind::Old { text };
    }
    if let Some(text) = keyword_string(trimmed, "new") {
        return LineKind::New { text };
    }
    if let Some(rest) = keyword_is(trimmed, "label") {
        if let Some(name) = rest.split_whitespace().next() {
            return LineKind::Label {
                name: name.trim_end_matches(':').to_string(),
            };
        }
    }
    if keyword_is(trimmed, "translate").is_some() {
        return LineKind::Other;
    }
    if trimmed.starts_with('"') {
        return classify_string_led_line(trimmed);
    }
    classify_say_line(trimmed)
}

/// `keyword "string" [# comment]` — the `old`/`new` statement form.
pub fn keyword_string(line: &str, keyword: &str) -> Option<String> {
    let rest = keyword_is(line, keyword)?;
    let rest = rest.trim_start();
    if !rest.starts_with('"') {
        return None;
    }
    let (_start, end, text) = scan_string(rest)?;
    let suffix = rest[end..].trim();
    if suffix.is_empty() || suffix.starts_with('#') {
        Some(text)
    } else {
        None
    }
}

/// `"..."`-led line: menu choice, narrator say, or something else.
fn classify_string_led_line(trimmed: &str) -> LineKind {
    let Some((_start, end, text)) = scan_string(trimmed) else {
        return LineKind::Other;
    };
    let suffix = trimmed[end..].trim();

    if let Some(after) = suffix.strip_prefix(':') {
        // `"Choice":` — what follows the colon must be blank or a comment.
        let after = after.trim();
        if after.is_empty() || after.starts_with('#') {
            return LineKind::Choice { text };
        }
        return LineKind::Other;
    }
    if suffix.starts_with("if ") {
        // `"Choice" if flag:` — the colon ends the line.
        if let Some(colon) = suffix.find(':') {
            let after = suffix[colon + 1..].trim();
            if after.is_empty() || after.starts_with('#') {
                return LineKind::Choice { text };
            }
        }
        return LineKind::Other;
    }
    if is_say_suffix(suffix) {
        return LineKind::Say {
            speaker: None,
            text,
        };
    }
    LineKind::Other
}

/// `speaker [attributes] "text" [nointeract] [with x] [# comment]`
fn classify_say_line(trimmed: &str) -> LineKind {
    let Some((_start, end, text)) = scan_string(trimmed) else {
        return LineKind::Other;
    };
    let quote_pos = trimmed.find('"').unwrap_or(0);
    let prefix = trimmed[..quote_pos].trim();
    let suffix = trimmed[end..].trim();

    let tokens: Vec<&str> = prefix.split_whitespace().collect();
    if tokens.is_empty() || STATEMENT_KEYWORDS.contains(&tokens[0]) {
        return LineKind::Other;
    }
    if !tokens
        .iter()
        .all(|t| t.chars().all(|c| c.is_alphanumeric() || c == '_'))
    {
        return LineKind::Other;
    }
    if !is_say_suffix(suffix) {
        return LineKind::Other;
    }
    LineKind::Say {
        speaker: Some(tokens[0].to_string()),
        text,
    }
}

/// Extract string literals from the Ren'Py/Python patterns used by quest
/// journals and HUD controls, plus static `text "..."` screen labels. We
/// intentionally avoid arbitrary Python strings so URLs, keys, and
/// implementation details remain outside the translation queue.
fn quest_and_screen_texts(line: &str) -> Vec<LineKind> {
    let strings = strings_in_line(line);
    if strings.is_empty() {
        return Vec::new();
    }

    let indices: Vec<usize> = if line.contains("Quest(") || line.contains(".add_objective(") {
        strings.iter().map(|(index, _)| *index).collect()
    } else if line.contains("create_nav_button(") {
        // The project's navigation helper takes direction, destination, then
        // the user-facing tooltip. Keep the first two implementation strings
        // (such as `left` and `bedroom`) out of the translation queue.
        strings.iter().nth(2).map(|(index, _)| *index).into_iter().collect()
    } else if line.contains("SetVariable(\"tooltip_text\"") {
        // `SetVariable("tooltip_text", "Phone")` is the other HUD-tooltip
        // form used by this game's screens.
        strings.iter().nth(1).map(|(index, _)| *index).into_iter().collect()
    } else if line.contains(".objectives[") && line.contains("[\"text\"]") {
        // The `"text"` dictionary key is not user-facing; the assignment
        // value after it is.
        strings.iter().skip(1).map(|(index, _)| *index).collect()
    } else if line.trim_start().starts_with("text ") {
        strings.iter().take(1).map(|(index, _)| *index).collect()
    } else if line.trim_start().starts_with('$')
        && line.contains("_texts")
        && line.contains('[')
    {
        strings.iter().map(|(index, _)| *index).collect()
    } else {
        Vec::new()
    };

    indices
        .into_iter()
        .filter_map(|wanted| {
            strings
                .iter()
                .find(|(index, _)| *index == wanted)
                .map(|(_, text)| LineKind::ScriptText {
                    text: text.clone(),
                    string_index: wanted,
                })
        })
        .collect()
}

fn strings_in_line(line: &str) -> Vec<(usize, String)> {
    let mut strings = Vec::new();
    let mut offset = 0usize;
    while let Some((_start, end, text)) = scan_string(&line[offset..]) {
        strings.push((strings.len(), text));
        offset += end;
    }
    strings
}

/// Valid text after a dialogue string for it to be a say statement.
fn is_say_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || suffix.starts_with('#')
        || suffix == "nointeract"
        || suffix.starts_with("with ")
        || suffix.starts_with("nointeract ")
}

/// `foo` prefix helper: requires whitespace or end after the keyword, so
/// `old "x"` matches but `old_x = "y"` does not.
pub fn keyword_is<'a>(line: &'a str, keyword: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(keyword)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest)
    } else {
        None
    }
}

/// Locate the first double-quoted string in `s`.
/// Returns (start index of `"`, end index just past the closing `"`,
/// unescaped content). Common Ren'Py escapes, including `\n`, are decoded so
/// a translated paragraph survives a scan/export round trip.
pub fn scan_string(s: &str) -> Option<(usize, usize, String)> {
    let start = s.find('"')?;
    let bytes = s.as_bytes();
    let mut content = String::new();
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return None;
                }
                let next = bytes[i + 1];
                match next {
                    b'"' => {
                        content.push('"');
                        i += 2;
                    }
                    b'\'' => {
                        content.push('\'');
                        i += 2;
                    }
                    b'\\' => {
                        content.push('\\');
                        i += 2;
                    }
                    b'n' => {
                        content.push('\n');
                        i += 2;
                    }
                    b'r' => {
                        content.push('\r');
                        i += 2;
                    }
                    b't' => {
                        content.push('\t');
                        i += 2;
                    }
                    _ => {
                        content.push('\\');
                        let end = (i + 1 + utf8_len(next)).min(s.len());
                        content.push_str(&s[i + 1..end]);
                        i = end;
                    }
                }
            }
            b'"' => return Some((start, i + 1, content)),
            _ => {
                let end = (i + utf8_len(bytes[i])).min(s.len());
                content.push_str(&s[i..end]);
                i = end;
            }
        }
    }
    None
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Escape a translation for embedding in a Ren'Py string literal.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}

/// Ren'Py protected tokens: `[variable]` interpolation and `{tag}` text
/// markers. Doubled `[[` / `{{` are literal escapes and are not tokens.
pub fn protected_tokens(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\[[^\[\]]+\]|\{[^{}]+\}").unwrap());
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<LineKind> {
        parse(src).into_iter().map(|l| l.kind).collect()
    }

    #[test]
    fn narrator_and_speaker_say() {
        let kinds = kinds("label start:\n\"Hello.\"\ne \"Hi there!\"\n");
        assert_eq!(
            kinds,
            vec![
                LineKind::Label {
                    name: "start".into()
                },
                LineKind::Say {
                    speaker: None,
                    text: "Hello.".into()
                },
                LineKind::Say {
                    speaker: Some("e".into()),
                    text: "Hi there!".into()
                },
            ]
        );
    }

    #[test]
    fn define_character_line_is_not_a_say() {
        assert!(kinds("define e = Character(\"Eileen\")\n").is_empty());
    }

    #[test]
    fn speaker_attributes_are_allowed() {
        assert_eq!(
            kinds("e happy \"Good morning.\"\n"),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "Good morning.".into()
            }]
        );
    }

    #[test]
    fn escaped_quotes_and_backslashes() {
        assert_eq!(
            kinds("e \"He said \\\"hi\\\". Right?\"\n"),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "He said \"hi\". Right?".into()
            }]
        );
        assert_eq!(
            kinds("e \"C:\\\\path\"\n"),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "C:\\path".into()
            }]
        );
    }

    #[test]
    fn say_with_nointeract_and_with_clause() {
        assert_eq!(
            kinds("e \"Go.\" nointeract\n"),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "Go.".into()
            }]
        );
        assert_eq!(
            kinds("e \"Go.\" with dissolve\n"),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "Go.".into()
            }]
        );
    }

    #[test]
    fn menu_choices() {
        let kinds = kinds("menu:\n    \"Go left\":\n    \"Go right\" if flag:\n");
        assert_eq!(
            kinds,
            vec![
                LineKind::Choice {
                    text: "Go left".into()
                },
                LineKind::Choice {
                    text: "Go right".into()
                },
            ]
        );
    }

    #[test]
    fn old_new_pairs() {
        let parsed = parse("translate thai strings:\n    old \"Hello\"\n    new \"สวัสดี\"\n");
        let kinds: Vec<LineKind> = parsed.iter().map(|l| l.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                LineKind::Old {
                    text: "Hello".into()
                },
                LineKind::New {
                    text: "สวัสดี".into()
                },
            ]
        );
        // Indentation is preserved for the exporter.
        assert_eq!(parsed[0].indent, 4);
        assert_eq!(parsed[0].lineno, 2);
        assert_eq!(parsed[1].lineno, 3);
    }

    #[test]
    fn python_and_statement_lines_are_ignored() {
        let src = concat!(
            "# a comment\n",
            "x = \"not dialogue\"\n",
            "if flag == \"yes\":\n",
            "    return \"never\"\n",
            "show eileen happy\n",
            "jump start\n",
            "$ renpy.notify(\"nope\")\n",
            "default points = 0\n",
            "translate thai start_9a8b7c6d:\n",
            "old_x = \"y\"\n",
        );
        assert!(kinds(src).is_empty());
    }

    #[test]
    fn quest_objective_and_screen_texts_are_captured() {
        let kinds = kinds(concat!(
            "$ quest = Quest(\"Student Life\", \"Study and socialize.\")\n",
            "$ quest.add_objective(\"Attend class.\", visible=True)\n",
            "$ quest.objectives[0][\"text\"] = \"Attend math class.\"\n",
            "$ _quest_texts = [\"One\", \"Two\"]\n",
            "text \"Quest Log\":\n",
        ));
        assert_eq!(
            kinds,
            vec![
                LineKind::ScriptText {
                    text: "Student Life".into(),
                    string_index: 0,
                },
                LineKind::ScriptText {
                    text: "Study and socialize.".into(),
                    string_index: 1,
                },
                LineKind::ScriptText {
                    text: "Attend class.".into(),
                    string_index: 0,
                },
                LineKind::ScriptText {
                    text: "Attend math class.".into(),
                    string_index: 1,
                },
                LineKind::ScriptText {
                    text: "One".into(),
                    string_index: 0,
                },
                LineKind::ScriptText {
                    text: "Two".into(),
                    string_index: 1,
                },
                LineKind::ScriptText {
                    text: "Quest Log".into(),
                    string_index: 0,
                },
            ]
        );
    }

    #[test]
    fn navigation_tooltips_are_captured_without_internal_targets() {
        let kinds = kinds(concat!(
            "$ create_nav_button(10, 20, \"left\", \"bedroom\", \"2nd Floor\")\n",
            "hovered SetVariable(\"tooltip_text\", \"Phone\")\n",
        ));
        assert_eq!(
            kinds,
            vec![
                LineKind::ScriptText {
                    text: "2nd Floor".into(),
                    string_index: 2,
                },
                LineKind::ScriptText {
                    text: "Phone".into(),
                    string_index: 1,
                },
            ]
        );
    }

    #[test]
    fn labels_are_captured() {
        let kinds = kinds("label start:\nlabel my_scene_2:\n");
        assert_eq!(
            kinds,
            vec![
                LineKind::Label {
                    name: "start".into()
                },
                LineKind::Label {
                    name: "my_scene_2".into()
                },
            ]
        );
    }

    #[test]
    fn triple_quoted_bodies_are_skipped() {
        let src = "e \"\"\"multi\nline\"\"\"\ne \"after\"\n";
        assert_eq!(
            kinds(src),
            vec![LineKind::Say {
                speaker: Some("e".into()),
                text: "after".into()
            }]
        );
    }

    #[test]
    fn narrator_say_requires_clean_suffix() {
        assert_eq!(
            kinds("\"Fine.\" # short answer\n"),
            vec![LineKind::Say {
                speaker: None,
                text: "Fine.".into()
            }]
        );
        // Dictionary-style line is not dialogue.
        assert!(kinds("\"key\": value,\n").is_empty());
    }

    #[test]
    fn escape_roundtrip() {
        let original = "He said \"hi\" \\ ok";
        let escaped = escape(original);
        let (_, _, unescaped) = scan_string(&format!("\"{}\"", escaped)).unwrap();
        assert_eq!(unescaped, original);
    }

    #[test]
    fn newline_escape_roundtrip() {
        let original = "บรรทัดแรก\nบรรทัดถัดไป";
        let escaped = escape(original);
        assert_eq!(escaped, "บรรทัดแรก\\nบรรทัดถัดไป");
        let (_, _, unescaped) = scan_string(&format!("\"{}\"", escaped)).unwrap();
        assert_eq!(unescaped, original);
    }

    #[test]
    fn protected_token_extraction() {
        let tokens = protected_tokens(
            "Hello [player_name]! {b}Bold{/b} {color=#fff}x{/color} [[literal {{x",
        );
        assert_eq!(
            tokens,
            vec!["[player_name]", "{b}", "{/b}", "{color=#fff}", "{/color}"]
        );
    }

    #[test]
    fn blank_and_comment_lines_are_skipped() {
        assert!(parse("\n\n   \n# comment\n").is_empty());
    }
}
