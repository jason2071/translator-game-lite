/// A project-scoped glossary entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlossaryEntry {
    pub id: String,
    pub project_id: String,
    pub source: String,
    pub target: String,
    pub note: Option<String>,
    pub enabled: bool,
}

/// True when `term` occurs in `text` as a standalone term.
///
/// Plain alphanumeric terms (typical for English) must match on word
/// boundaries: "Master" matches "Master" but not "Masterpiece". Terms
/// containing punctuation or non-alphanumeric characters (CJK, "St.", ...)
/// match as substrings, because word boundaries are meaningless for them.
pub fn term_matches(term: &str, text: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    if is_word_like(term) {
        let pattern = format!(r"\b{}\b", regex::escape(term));
        regex::RegexBuilder::new(&pattern)
            .case_insensitive(true)
            .build()
            .map(|re| re.is_match(text))
            .unwrap_or(false)
    } else {
        text.to_lowercase().contains(&term.to_lowercase())
    }
}

fn is_word_like(term: &str) -> bool {
    term.chars().any(|c| c.is_ascii_alphanumeric())
        && term
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '\''))
}

/// The enabled glossary entries whose source term appears in `text`.
pub fn matches_for_text<'a>(
    entries: &'a [GlossaryEntry],
    text: &str,
) -> Vec<&'a GlossaryEntry> {
    entries
        .iter()
        .filter(|e| e.enabled && term_matches(&e.source, text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &str, target: &str) -> GlossaryEntry {
        GlossaryEntry {
            id: "g1".into(),
            project_id: "p1".into(),
            source: source.into(),
            target: target.into(),
            note: None,
            enabled: true,
        }
    }

    #[test]
    fn word_boundary_matching() {
        assert!(term_matches("Master", "The Master is here."));
        assert!(term_matches("master", "MASTER"));
        assert!(!term_matches("Master", "What a masterpiece."));
        assert!(!term_matches("Master", "Mastering the craft."));
        assert!(term_matches("Guild", "I'm going to the guild."));
    }

    #[test]
    fn substring_matching_for_punctuated_terms() {
        assert!(term_matches("St. Alice", "Talk to St. Alice now."));
        assert!(term_matches("ユウカ", "ユウカです"));
    }

    #[test]
    fn disabled_and_empty_entries_never_match() {
        let mut e = entry("Master", "นายท่าน");
        e.enabled = false;
        assert!(matches_for_text(&[e.clone()], "Master").is_empty());
        assert!(!term_matches("", "anything"));
    }

    #[test]
    fn matches_for_text_filters_enabled_and_present() {
        let entries = vec![
            entry("Alice", "อลิซ"),
            entry("Guild", "กิลด์"),
            entry("Dragon", "มังกร"),
        ];
        let hits = matches_for_text(&entries, "Alice goes to the guild.");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source, "Alice");
        assert_eq!(hits[1].source, "Guild");
    }
}
