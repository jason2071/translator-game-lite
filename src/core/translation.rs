use crate::core::source::SourceEntry;

/// Status of a single entry's translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationStatus {
    /// Not translated yet.
    Pending,
    /// Translated automatically (AI or translation memory).
    Translated,
    /// Edited manually — never overwritten automatically.
    Edited,
    /// Last automatic attempt failed validation; user decides to retry.
    Failed,
}

impl TranslationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TranslationStatus::Pending => "pending",
            TranslationStatus::Translated => "translated",
            TranslationStatus::Edited => "edited",
            TranslationStatus::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "translated" => TranslationStatus::Translated,
            "edited" => TranslationStatus::Edited,
            "failed" => TranslationStatus::Failed,
            _ => TranslationStatus::Pending,
        }
    }
}

/// A source entry together with its translation state. This is what the
/// engine exporter consumes (it needs the original location and text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslationEntry {
    pub source: SourceEntry,
    pub translated_text: Option<String>,
    pub status: TranslationStatus,
    pub updated_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        for s in [
            TranslationStatus::Pending,
            TranslationStatus::Translated,
            TranslationStatus::Edited,
            TranslationStatus::Failed,
        ] {
            assert_eq!(TranslationStatus::from_str(s.as_str()), s);
        }
    }

    #[test]
    fn manual_edits_are_not_pending_or_failed() {
        // The policy "automatic translation never overwrites Translated or
        // Edited" is enforced by the pending-entries query, which only
        // selects 'pending' and 'failed'.
        assert_eq!(TranslationStatus::from_str("pending"), TranslationStatus::Pending);
        assert_eq!(TranslationStatus::from_str("failed"), TranslationStatus::Failed);
        assert_ne!(TranslationStatus::from_str("pending"), TranslationStatus::Edited);
        assert_ne!(TranslationStatus::from_str("pending"), TranslationStatus::Translated);
    }
}
