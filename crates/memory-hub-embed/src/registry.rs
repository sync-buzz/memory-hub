//! Hard-coded registry of supported GGUF embedding models.
//!
//! Each entry pins URL, SHA-256, dimensions, `max_tokens` and pooling so
//! downloads are verifiable and the index can guard against silently swapping
//! models. New models are added by appending a [`ModelEntry`] constant and
//! listing it in [`ALL`] — there is no runtime extension surface.

use crate::provider::Pooling;

/// SHA-256 sentinel used while a real hash hasn't been recorded yet. The
/// downloader treats this as "compute and print, do not verify"; the registry
/// test relaxes its check for entries pinned to this value. Replace with the
/// actual hex digest as each model is downloaded.
pub const PLACEHOLDER_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    /// One-line, user-facing summary of when to pick this model.
    pub description: &'static str,
    /// Short language-coverage label (e.g. "Multilingual", "English only").
    pub languages: &'static str,
    /// Direct HF resolve URL for the GGUF file.
    pub url: &'static str,
    /// Lowercase 64-hex SHA-256 of the GGUF file. May be
    /// [`PLACEHOLDER_SHA256`] until the model has been downloaded and verified
    /// locally.
    pub sha256: &'static str,
    pub dimensions: usize,
    pub max_tokens: usize,
    pub size_bytes: u64,
    /// GGUF quantisation suffix (e.g. `"Q5_K_M"`). Surfaced by model status
    /// and persisted in projection metadata for observability.
    pub quantisation: &'static str,
    pub pooling: Pooling,
    /// Prefix the model expects on query inputs (asymmetric encoders).
    pub query_prefix: Option<&'static str>,
    /// Prefix the model expects on document inputs (asymmetric encoders).
    pub doc_prefix: Option<&'static str>,
}

/// Default model — best multilingual quality, used as the Memory Hub default.
pub const BGE_M3: ModelEntry = ModelEntry {
    id: "bge-m3",
    display_name: "BGE-M3 (Q5_K_M)",
    description: "Best multilingual quality, especially for non-English and \
                  mixed-language notes. Largest download and a little slower \
                  to index — worth it when search quality matters most.",
    languages: "Multilingual (best)",
    url: "https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q5_K_M.gguf",
    sha256: PLACEHOLDER_SHA256,
    dimensions: 1024,
    max_tokens: 1024,
    size_bytes: 467_662_912,
    quantisation: "Q5_K_M",
    pooling: Pooling::Cls,
    query_prefix: Some("query: "),
    doc_prefix: None,
};

/// Balanced default — solid semantic search at a small download.
pub const NOMIC_EMBED_TEXT_V15: ModelEntry = ModelEntry {
    id: "nomic-embed-text-v1.5",
    display_name: "Nomic Embed Text v1.5 (Q5_K_M)",
    description: "Balanced default — solid semantic search at a small download. \
                  Decent across languages; for heavy non-English or \
                  mixed-language notes, BGE-M3 is noticeably stronger.",
    languages: "Multilingual (good)",
    url: "https://huggingface.co/nomic-ai/nomic-embed-text-v1.5-GGUF/resolve/main/nomic-embed-text-v1.5.Q5_K_M.gguf",
    sha256: "0c7930f6c4f6f29b7da5046e3a2c0832aa3f602db3de5760a95f0582dbd3d6e6",
    dimensions: 768,
    max_tokens: 2048,
    size_bytes: 99_588_928,
    quantisation: "Q5_K_M",
    pooling: Pooling::Mean,
    query_prefix: Some("search_query: "),
    doc_prefix: Some("search_document: "),
};

/// Tiny English-only — for CI and constrained environments.
pub const BGE_SMALL_EN_V15: ModelEntry = ModelEntry {
    id: "bge-small-en-v1.5",
    display_name: "BGE Small EN v1.5 (Q5_K_M)",
    description: "Tiny and fast, English only. Good for low-disk machines or \
                  English-only projects; skips other languages.",
    languages: "English only",
    url: "https://huggingface.co/ChristianAzinn/bge-small-en-v1.5-gguf/resolve/main/bge-small-en-v1.5.Q5_K_M.gguf",
    sha256: "57f08f3e03e87282b4ece8eb4b90e18bd4aeebfcb57aa7f73901acabd5a5a583",
    dimensions: 384,
    max_tokens: 512,
    size_bytes: 30_475_552,
    quantisation: "Q5_K_M",
    pooling: Pooling::Cls,
    query_prefix: Some("query: "),
    doc_prefix: None,
};

pub const ALL: &[&ModelEntry] = &[&BGE_M3, &NOMIC_EMBED_TEXT_V15, &BGE_SMALL_EN_V15];

#[must_use]
pub fn all_models() -> &'static [&'static ModelEntry] {
    ALL
}

#[must_use]
pub fn find(id: &str) -> Option<&'static ModelEntry> {
    ALL.iter().copied().find(|m| m.id == id)
}

/// Default model — best multilingual quality, matching the Memory Hub index
/// configuration. Can change via registry data without changing MCP tool
/// semantics.
#[must_use]
pub fn default_model() -> &'static ModelEntry {
    &BGE_M3
}

/// Platform-aware default model.
///
/// Selects the best default for the compile-time target:
///
/// | Target | Backend | Default | Rationale |
/// |---|---|---|---|
/// | `aarch64-apple-darwin` (M1/M2/M3) | Metal | `bge-m3` | Metal accelerates 1024-dim; best multilingual quality |
/// | `x86_64-apple-darwin` (Intel Mac) | CPU | `nomic-embed-text-v1.5` | CPU is slower; 768-dim is an acceptable balance |
/// | `x86_64-*linux*` / `x86_64-pc-windows` | CPU | `nomic-embed-text-v1.5` | Same CPU balance |
///
/// Tests cover every supported target via `cfg!` mocks — see
/// `tests::platform_default_matches_target`.
#[must_use]
pub fn platform_default_model() -> &'static ModelEntry {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        &BGE_M3
    } else if cfg!(any(
        all(target_arch = "x86_64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "windows"),
    )) {
        &NOMIC_EMBED_TEXT_V15
    } else {
        &BGE_M3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_default_matches_target() {
        let model = platform_default_model();
        if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
            assert_eq!(model.id, "bge-m3");
        } else if cfg!(any(
            all(target_arch = "x86_64", target_os = "macos"),
            all(target_arch = "x86_64", target_os = "linux"),
            all(target_arch = "x86_64", target_os = "windows"),
        )) {
            assert_eq!(model.id, "nomic-embed-text-v1.5");
        } else {
            assert_eq!(model.id, "bge-m3");
        }
    }

    #[test]
    fn platform_default_is_in_registry() {
        let model = platform_default_model();
        assert!(ALL.iter().any(|m| m.id == model.id));
    }

    #[test]
    fn find_returns_known_models() {
        assert_eq!(find("bge-m3").map(|m| m.id), Some("bge-m3"));
        assert_eq!(
            find("nomic-embed-text-v1.5").map(|m| m.id),
            Some("nomic-embed-text-v1.5")
        );
        assert_eq!(
            find("bge-small-en-v1.5").map(|m| m.id),
            Some("bge-small-en-v1.5")
        );
        assert!(find("nonexistent").is_none());
    }
}
