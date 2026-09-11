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
    "if", "elif", "else", "while", "for", "return", "call", "jump", "scene", "show", "hide",
    "play", "stop", "voice", "queue", "python", "default", "define", "menu", "label", "init",
    "transform", "style", "image", "pause", "with", "window", "nvl", "screen", "translate",
    "old", "new", "use", "add", "textbutton", "imagebutton", "hotspot", "vbox", "hbox", "frame",
    "fixed", "grid", "side", "bar", "key", "timer", "mousearea", "drag", "draggroup", "config",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    /// `e "Hello."`, `e happy "Hello."`, `extend "..."` or narrator `"Hello."`
    Say { speaker: Option<String>, text: String },
    /// `"Choice text":` or `"Choice text" if cond:` inside a `menu:` block
    Choice { text: String },
    /// `old "Hello"` in a translate block
    Old { text: String },
    /// `new "สวัสดี"` in a translate block
    New { text: String },
    /// `label start:`
    Label { name: String },
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
        let kind = classify(trimmed);
        if kind != LineKind::Other {
            out.push(ParsedLine { lineno, indent, kind });
        }
    }
    out
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
/// unescaped content). `\"` and `\\` and `\'` are unescaped; unknown escapes
/// (e.g. `\n`) are kept verbatim so export can round-trip them.
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
                LineKind::Label { name: "start".into() },
                LineKind::Say { speaker: None, text: "Hello.".into() },
                LineKind::Say { speaker: Some("e".into()), text: "Hi there!".into() },
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
            vec![LineKind::Say { speaker: Some("e".into()), text: "Go.".into() }]
        );
        assert_eq!(
            kinds("e \"Go.\" with dissolve\n"),
            vec![LineKind::Say { speaker: Some("e".into()), text: "Go.".into() }]
        );
    }

    #[test]
    fn menu_choices() {
        let kinds = kinds("menu:\n    \"Go left\":\n    \"Go right\" if flag:\n");
        assert_eq!(
            kinds,
            vec![
                LineKind::Choice { text: "Go left".into() },
                LineKind::Choice { text: "Go right".into() },
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
                LineKind::Old { text: "Hello".into() },
                LineKind::New { text: "สวัสดี".into() },
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
    fn labels_are_captured() {
        let kinds = kinds("label start:\nlabel my_scene_2:\n");
        assert_eq!(
            kinds,
            vec![
                LineKind::Label { name: "start".into() },
                LineKind::Label { name: "my_scene_2".into() },
            ]
        );
    }

    #[test]
    fn triple_quoted_bodies_are_skipped() {
        let src = "e \"\"\"multi\nline\"\"\"\ne \"after\"\n";
        assert_eq!(
            kinds(src),
            vec![LineKind::Say { speaker: Some("e".into()), text: "after".into() }]
        );
    }

    #[test]
    fn narrator_say_requires_clean_suffix() {
        assert_eq!(
            kinds("\"Fine.\" # short answer\n"),
            vec![LineKind::Say { speaker: None, text: "Fine.".into() }]
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
