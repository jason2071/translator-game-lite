/// A translation project: one game directory + one engine + language pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub engine_id: String,
    pub source_language: String,
    pub target_language: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Project {
    pub fn new(name: impl Into<String>, path: impl Into<String>, engine_id: &str) -> Self {
        let now = now_unix();
        Self {
            id: new_id(),
            name: name.into(),
            path: path.into(),
            engine_id: engine_id.to_string(),
            source_language: "English".to_string(),
            target_language: "Thai".to_string(),
            created_at: now,
            updated_at: now,
        }
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Short random id (no uuid dependency).
pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let addr = std::process::id() as u64;
    format!("{:x}{:x}", nanos, addr << 16)
}
