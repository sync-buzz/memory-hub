//! Canonical text renderer for the generic [`Envelope`].
//!
//! One renderer is shared by full rebuild and incremental update. The output
//! is what the embedding provider encodes; the model-specific `doc_prefix` is
//! applied later by the worker, so the cache key (content hash) is stable
//! across model swaps that only change the prefix.

use memory_hub_core::{Envelope, StoredRecord};

/// Renderer version. Bumped when the rendering logic changes; the fingerprint
/// includes this so a renderer change forces a rebuild.
pub const RENDERER_VERSION: u32 = 1;

/// Render the canonical embedding input text for a record.
///
/// For plaintext records the envelope's `kind`, `title` and `content` are
/// composed into a single deterministic string. Encrypted records yield an
/// empty string — they are not embedded.
#[must_use]
pub fn render_envelope(record: &StoredRecord) -> String {
    let StoredRecord::Plaintext { envelope } = record else {
        return String::new();
    };
    render_envelope_inner(envelope)
}

/// Render the canonical embedding input text for an envelope.
#[must_use]
pub fn render_envelope_inner(envelope: &Envelope) -> String {
    let mut parts: Vec<&str> = Vec::with_capacity(3);
    parts.push(envelope.kind.as_str());
    if let Some(title) = envelope.title.as_deref()
        && !title.trim().is_empty()
    {
        parts.push(title);
    }
    parts.push(envelope.content.as_str());
    parts.join(" — ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use memory_hub_core::Envelope;

    #[test]
    fn renders_kind_title_content() {
        let envelope = Envelope::new("arch/seam", "decision", "We chose X because Y.").unwrap();
        let text = render_envelope_inner(&envelope);
        assert_eq!(text, "decision — We chose X because Y.");
    }

    #[test]
    fn omits_empty_title() {
        let envelope = Envelope::new("key", "note", "body").unwrap();
        let text = render_envelope_inner(&envelope);
        assert_eq!(text, "note — body");
    }

    #[test]
    fn encrypted_record_yields_empty() {
        use memory_hub_core::{CURRENT_ENVELOPE_VERSION, EncryptedRecord, OpaqueStorageId};
        use std::collections::BTreeMap;
        let record = StoredRecord::Encrypted {
            encrypted: EncryptedRecord {
                envelope_version: CURRENT_ENVELOPE_VERSION,
                storage_id: OpaqueStorageId::new(
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                )
                .unwrap(),
                key_epoch: 1,
                cipher_suite: "suite".into(),
                nonce: "nonce".into(),
                ciphertext: "ct".into(),
                extensions: BTreeMap::new(),
            },
        };
        assert_eq!(render_envelope(&record), "");
    }

    #[test]
    fn same_envelope_produces_same_text() {
        let envelope = Envelope::new("k", "note", "stable").unwrap();
        assert_eq!(
            render_envelope_inner(&envelope),
            render_envelope_inner(&envelope)
        );
    }

    #[test]
    fn different_content_produces_different_text() {
        let a = Envelope::new("k", "note", "alpha").unwrap();
        let b = Envelope::new("k", "note", "beta").unwrap();
        assert_ne!(render_envelope_inner(&a), render_envelope_inner(&b));
    }
}
