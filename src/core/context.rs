/// Configurable context window: how many previous/next lines to include
/// around the current text (Settings: Context Before / Context After).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextWindow {
    pub before: u32,
    pub after: u32,
}

impl Default for ContextWindow {
    fn default() -> Self {
        Self { before: 1, after: 1 }
    }
}

/// Resolved context for one entry, used both for the AI prompt and the
/// editor's context panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DialogueContext {
    pub speaker: Option<String>,
    pub scene: Option<String>,
    pub previous: Vec<String>,
    pub next: Vec<String>,
}

impl DialogueContext {
    /// Compact prompt block, e.g.
    ///
    /// ```text
    /// Speaker: Alice
    /// Scene: guild_hall
    /// Previous: Where are you going?
    /// Next: Do you want to come?
    /// ```
    pub fn format_prompt(&self) -> String {
        let mut lines = Vec::new();
        if let Some(speaker) = &self.speaker {
            lines.push(format!("Speaker: {}", speaker));
        }
        if let Some(scene) = &self.scene {
            lines.push(format!("Scene: {}", scene));
        }
        if !self.previous.is_empty() {
            lines.push(format!("Previous: {}", self.previous.join(" / ")));
        }
        if !self.next.is_empty() {
            lines.push(format!("Next: {}", self.next.join(" / ")));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_prompt_includes_present_parts_only() {
        let ctx = DialogueContext {
            speaker: Some("Alice".into()),
            scene: None,
            previous: vec!["Where are you going?".into()],
            next: vec!["Do you want to come?".into()],
        };
        let s = ctx.format_prompt();
        assert!(s.contains("Speaker: Alice"));
        assert!(!s.contains("Scene:"));
        assert!(s.contains("Previous: Where are you going?"));
        assert!(s.contains("Next: Do you want to come?"));
    }

    #[test]
    fn multiple_neighbor_lines_are_joined() {
        let ctx = DialogueContext {
            previous: vec!["A".into(), "B".into()],
            next: vec![],
            ..Default::default()
        };
        assert_eq!(ctx.format_prompt(), "Previous: A / B");
    }

    #[test]
    fn default_window_is_one_one() {
        let w = ContextWindow::default();
        assert_eq!((w.before, w.after), (1, 1));
    }
}
