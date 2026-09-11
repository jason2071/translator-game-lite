/// One translatable string from a game — deliberately engine-independent.
/// Ren'Py-specific details must not appear here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub id: String,
    pub engine_id: String,
    pub file_path: String,
    pub line: u32,

    pub speaker: Option<String>,

    pub source_text: String,
    pub source_hash: String,

    pub context: Option<String>,
}

/// sha256 hex of the source text — the identity used for translation memory
/// and incremental scans.
pub fn hash_text(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{:02x}", byte));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_sha256_hex() {
        let a = hash_text("Hello");
        let b = hash_text("Hello");
        let c = hash_text("Hello\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // sha256("Hello") known value
        assert_eq!(
            a,
            "185f8db32271fe25f561a6fc938b2e264306ec304eda518007d1764826381969"
        );
    }
}
