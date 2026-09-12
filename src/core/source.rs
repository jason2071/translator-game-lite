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

/// Remove space-like/invisible characters some models emit (zero-width
/// spaces, fillers, NBSP, soft hyphens, ...) and collapse horizontal
/// whitespace. Explicit line breaks are preserved for translated paragraphs.
pub fn clean_spaces(text: &str) -> String {
    let mapped: String = text
        .chars()
        .map(|c| {
            if matches!(
                c,
                '\u{00a0}' | '\u{00ad}' | '\u{180e}' | '\u{2000}'
                    ..='\u{200f}'
                        | '\u{2028}'
                        | '\u{2029}'
                        | '\u{202f}'
                        | '\u{205f}'
                        | '\u{2800}'
                        | '\u{3000}'
                        | '\u{3164}'
                        | '\u{feff}'
            ) {
                ' '
            } else {
                c
            }
        })
        .collect();
    mapped
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n")
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

    #[test]
    fn clean_spaces_trims_and_collapses() {
        assert_eq!(clean_spaces("  สวัสดี   ครับ  "), "สวัสดี ครับ");
        assert_eq!(clean_spaces("a\u{00a0}b"), "a b");
        assert_eq!(clean_spaces("a\u{200b}b"), "a b");
        assert_eq!(clean_spaces("\u{feff}สวัสดี"), "สวัสดี");
        assert_eq!(clean_spaces("ok"), "ok");
    }

    #[test]
    fn clean_spaces_preserves_manual_line_breaks() {
        assert_eq!(
            clean_spaces("  บรรทัดแรก   \r\n  บรรทัดถัดไป  \n\n  ย่อหน้าใหม่ "),
            "บรรทัดแรก\nบรรทัดถัดไป\n\nย่อหน้าใหม่"
        );
    }
}
