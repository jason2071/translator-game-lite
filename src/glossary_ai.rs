//! AI-assisted glossary extraction.
//!
//! Two stages:
//! 1. [`mine_candidates`] scans the project's source texts locally for
//!    likely proper nouns and recurring phrases (capitalized phrases,
//!    mid-sentence capitalized words), ranked by frequency.
//! 2. [`propose_glossary`] sends the candidates to the configured AI
//!    provider (the "glossary" purpose profile) and parses the reply into
//!    term/translation proposals for the user to review.

use anyhow::Result;
use regex::Regex;

use crate::ai::{RequestItem, TranslationProvider, TranslationRequest};

#[derive(Debug, Clone)]
pub struct Candidate {
    pub term: String,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct Proposal {
    pub source: String,
    pub target: String,
    pub occurrences: usize,
}

/// Words so common in dialogue that their capitalized forms carry no
/// proper-noun signal.
const STOPWORDS: &[&str] = &[
    "the", "and", "but", "for", "with", "about", "from", "into", "onto", "over", "under", "that",
    "this", "these", "those", "there", "then", "than", "them", "they", "their", "she", "her",
    "hers", "him", "his", "you", "your", "yours", "our", "ours", "was", "were", "has", "had",
    "have", "not", "are", "yes", "yeah", "hey", "hello", "well", "what", "when", "where", "why",
    "who", "how", "why", "just", "like", "know", "think", "really", "right", "okay", "fine",
    "come", "came", "going", "went", "gone", "get", "got", "gotta", "want", "wanted", "tell",
    "told", "said", "says", "look", "looked", "see", "saw", "seen", "make", "made", "take",
    "took", "give", "gave", "good", "bad", "girl", "girls", "man", "men", "woman", "women",
    "wait", "stop", "thank", "thanks", "sorry", "please", "even", "ever", "still", "again",
    "always", "never", "maybe", "because", "while", "after", "before", "here", "now", "one",
    "two", "let", "lets", "don", "didn", "isn", "won", "can", "could", "would", "should", "will",
];

fn is_stopword(word: &str) -> bool {
    let lower = word.to_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

fn contains_variable(term: &str) -> bool {
    term.contains('[') || term.contains('{')
}

/// Mine frequently-occurring proper-noun-ish terms from source texts.
///
/// Two signals:
/// - phrases of two or more consecutive capitalized words ("Miss Jones")
/// - single capitalized words appearing mid-sentence, i.e. right after a
///   lowercase letter or a comma (sentence starts are ignored, they are
///   mostly ordinary words)
pub fn mine_candidates(texts: &[String], max: usize) -> Vec<Candidate> {
    let phrase_re = Regex::new(r"\b[A-Z][A-Za-z0-9']*(?:\s+[A-Z][A-Za-z0-9']*)+\b").unwrap();
    let single_re = Regex::new(r"([a-z,;] )([A-Z][a-z]{2,})\b").unwrap();

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for text in texts {
        for m in phrase_re.find_iter(text) {
            if contains_variable(m.as_str()) {
                continue;
            }
            // Split the phrase at stopwords ("The Morrow Guild" ->
            // "Morrow Guild") and count each run of real words.
            let mut run: Vec<&str> = Vec::new();
            for word in m.as_str().split_whitespace() {
                if is_stopword(word) {
                    if run.len() >= 2 {
                        let term = run.join(" ");
                        if term.len() >= 6 {
                            *counts.entry(term).or_default() += 1;
                        }
                    }
                    run.clear();
                } else {
                    run.push(word);
                }
            }
            if run.len() >= 2 {
                let term = run.join(" ");
                if term.len() >= 6 {
                    *counts.entry(term).or_default() += 1;
                }
            }
        }
        for c in single_re.captures_iter(text) {
            let Some(m) = c.get(2) else { continue };
            let word = m.as_str();
            if word.len() < 4 || is_stopword(word) || contains_variable(text) {
                continue;
            }
            *counts.entry(word.to_string()).or_default() += 1;
        }
    }

    let mut candidates: Vec<Candidate> = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(term, count)| Candidate { term, count })
        .collect();
    candidates.sort_by(|a, b| b.count.cmp(&a.count).then(a.term.cmp(&b.term)));
    candidates.truncate(max);
    candidates
}

/// Ask the AI provider to curate the candidates into glossary proposals.
/// Returns term/translation pairs for the candidates the model kept,
/// ordered by frequency (the input order).
pub fn propose_glossary(
    provider: &dyn TranslationProvider,
    target_language: &str,
    candidates: &[Candidate],
    batch_size: usize,
) -> Result<Vec<Proposal>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let batch_size = batch_size.clamp(10, 100);
    let counts: std::collections::HashMap<&str, usize> = candidates
        .iter()
        .map(|c| (c.term.as_str(), c.count))
        .collect();

    let mut proposals = Vec::new();
    for batch in candidates.chunks(batch_size) {
        let list = serde_json::to_string_pretty(
            &batch
                .iter()
                .map(|c| serde_json::json!({ "term": c.term, "count": c.count }))
                .collect::<Vec<_>>(),
        )?;

        let system_prompt = format!(
            "You curate a glossary for translating a game script into {target_language}.\n\
             The user gives candidate terms with how often each occurs.\n\
             Keep the terms that need a consistent translation: character names, places,\n\
             organizations, items, recurring phrases. Drop generic words, fragments that are\n\
             not real terms, and anything containing [brackets] or {{tags}}.\n\
             Translate each kept term into {target_language}.\n\
             Reply with STRICT JSON only, no commentary:\n\
             {{\"translations\":[{{\"id\":\"<exact candidate term>\",\"text\":\"<translation>\"}}]}}\n\
             The \"id\" must be copied character-for-character from the candidate list."
        );
        let request = TranslationRequest {
            system_prompt,
            items: vec![RequestItem {
                id: "candidates".into(),
                text: list,
                context: None,
            }],
        };
        let response = provider.translate(&request).map_err(|e| {
            e.context(format!(
                "the provider rejected the glossary request (model may not be able to emit JSON)"
            ))
        })?;

        for item in response.translations {
            let Some(&count) = counts.get(item.id.as_str()) else {
                continue; // model invented a term — ignore it
            };
            if item.text.trim().is_empty() {
                continue;
            }
            proposals.push(Proposal {
                source: item.id,
                target: item.text,
                occurrences: count,
            });
        }
    }

    // Dedupe (a term may come back in several batches) keeping the first.
    let mut seen = std::collections::HashSet::new();
    proposals.retain(|p| seen.insert(p.source.to_lowercase()));
    Ok(proposals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts() -> Vec<String> {
        vec![
            "Miss Jones brought you back to reality, and Marrow was waiting.".into(),
            "\"Marrow is dangerous,\" said Miss Jones again. The Morrow Guild meets tonight."
                .into(),
            "you remembered what happened that night with the Morrow Guild.".into(),
            "Just some ordinary sentence with no names at all in it.".into(),
        ]
    }

    #[test]
    fn mines_recurring_names_and_phrases() {
        let candidates = mine_candidates(&texts(), 50);
        let terms: Vec<&str> = candidates.iter().map(|c| c.term.as_str()).collect();
        assert!(terms.contains(&"Miss Jones"), "{terms:?}");
        assert!(terms.iter().any(|t| t.contains("Morrow Guild")), "{terms:?}");
    }

    #[test]
    fn skips_stopwords_and_variables() {
        let texts = vec![
            "She said the guild would come, and Nothing happened here.".into(),
            "[player_name] looked at {w} the sky.".into(),
        ];
        let candidates = mine_candidates(&texts, 50);
        assert!(candidates
            .iter()
            .all(|c| !c.term.contains('[') && !c.term.contains('{')));
    }

    #[test]
    fn keeps_only_known_terms_and_dedupes() {
        use crate::ai::provider::{TranslatedItem, TranslationResponse};
        struct Mock;
        impl TranslationProvider for Mock {
            fn translate(&self, _request: &TranslationRequest) -> Result<TranslationResponse> {
                Ok(TranslationResponse {
                    translations: vec![
                        TranslatedItem {
                            id: "Miss Jones".into(),
                            text: "เมียส์ โจนส์".into(),
                        },
                        TranslatedItem {
                            id: "Made Up Term".into(), // invented — dropped
                            text: "อะไรสักอย่าง".into(),
                        },
                    ],
                })
            }
            fn list_models(&self) -> Result<Vec<String>> {
                unimplemented!()
            }
        }

        let candidates = vec![
            Candidate { term: "Morrow Guild".into(), count: 5 },
            Candidate { term: "Miss Jones".into(), count: 2 },
        ];
        let proposals = propose_glossary(&Mock, "Thai", &candidates, 50).unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].source, "Miss Jones");
        assert_eq!(proposals[0].target, "เมียส์ โจนส์");
        assert_eq!(proposals[0].occurrences, 2);
    }

    #[test]
    fn empty_candidates_skip_the_ai_call() {
        use crate::ai::provider::TranslationResponse;
        struct Panic;
        impl TranslationProvider for Panic {
            fn translate(&self, _request: &TranslationRequest) -> Result<TranslationResponse> {
                panic!("must not be called")
            }
            fn list_models(&self) -> Result<Vec<String>> {
                unimplemented!()
            }
        }
        let proposals = propose_glossary(&Panic, "Thai", &[], 50).unwrap();
        assert!(proposals.is_empty());
    }
}
