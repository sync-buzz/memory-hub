#![allow(clippy::unwrap_used, clippy::expect_used)]

use memory_hub_embed::registry::{
    ALL, BGE_M3, BGE_SMALL_EN_V15, NOMIC_EMBED_TEXT_V15, PLACEHOLDER_SHA256, all_models, find,
};

#[test]
fn registry_is_well_formed() {
    assert_eq!(
        ALL.len(),
        3,
        "registry should pin exactly three models for now"
    );
    for entry in all_models() {
        assert!(!entry.id.is_empty(), "id must be non-empty");
        assert!(
            entry.url.starts_with("https://"),
            "url must be https: {}",
            entry.url
        );
        assert!(
            std::path::Path::new(entry.url)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("gguf")),
            "url must point at a GGUF: {}",
            entry.url
        );
        assert!(entry.dimensions > 0, "{}: dimensions=0", entry.id);
        assert!(entry.max_tokens > 0, "{}: max_tokens=0", entry.id);
        assert!(entry.size_bytes > 0, "{}: size_bytes=0", entry.id);
        assert_eq!(
            entry.sha256.len(),
            64,
            "{}: sha256 must be 64 hex chars",
            entry.id
        );
        assert!(
            entry.sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "{}: sha256 must be hex",
            entry.id
        );
    }
}

#[test]
fn placeholders_are_clearly_marked() {
    for entry in all_models() {
        if entry.sha256 == PLACEHOLDER_SHA256 {
            eprintln!("note: {} still uses PLACEHOLDER_SHA256", entry.id);
        }
    }
}

#[test]
fn find_round_trips() {
    assert!(find("nonexistent-model").is_none());
    for entry in all_models() {
        let got = find(entry.id).expect("should be findable");
        assert_eq!(got.id, entry.id);
        assert_eq!(got.dimensions, entry.dimensions);
    }
}

#[test]
fn bge_m3_is_the_default() {
    use memory_hub_embed::registry::default_model;
    assert_eq!(default_model().id, BGE_M3.id);
    assert_eq!(default_model().dimensions, 1024);
}

#[test]
fn known_dimensions() {
    assert_eq!(NOMIC_EMBED_TEXT_V15.dimensions, 768);
    assert_eq!(BGE_M3.dimensions, 1024);
    assert_eq!(BGE_SMALL_EN_V15.dimensions, 384);
}
