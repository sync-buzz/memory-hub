//! Hash-based incremental cache primitives shared by the rebuild pass and
//! the background worker.
//!
//! The full diff/upsert logic lives in the projection; this module owns the
//! canonical hash so both call sites agree on the cache key.

/// blake3 hex digest of the canonical embedding input text (`render_envelope`
/// output, without the model-specific `doc_prefix`). Stored in the projection
/// so a re-rebuild can short-circuit unchanged records.
#[must_use]
pub fn content_hash_of(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_when_text_changes() {
        let a = content_hash_of("decision: a — body");
        let b = content_hash_of("decision: a — body!");
        assert_ne!(a, b);
        assert_eq!(a, content_hash_of("decision: a — body"));
    }

    #[test]
    fn digest_is_64_hex_chars() {
        let h = content_hash_of("anything");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
