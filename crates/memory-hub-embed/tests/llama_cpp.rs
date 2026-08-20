#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for [`LlamaCppProvider`] against real GGUF models.
//!
//! All tests are `#[ignore]`-tagged because they require a downloaded model
//! file. Run with:
//!
//! ```sh
//! cargo test -p memory-hub-embed --test llama_cpp -- --ignored
//! ```

use std::sync::Arc;

use memory_hub_embed::{
    DownloadOpts, EmbeddingProvider, LlamaCppProvider, ModelEntry, ensure_model,
    registry::{BGE_M3, BGE_SMALL_EN_V15},
};

async fn provider_for(entry: &'static ModelEntry) -> Arc<LlamaCppProvider> {
    let outcome = ensure_model(
        entry,
        DownloadOpts {
            max_attempts: Some(1),
            ..Default::default()
        },
    )
    .await
    .unwrap_or_else(|e| {
        panic!(
            "{} must be available on disk (or downloadable in --ignored mode): {e}",
            entry.id
        )
    });
    Arc::new(LlamaCppProvider::new(entry, outcome.path().to_path_buf()))
}

async fn provider() -> Arc<LlamaCppProvider> {
    provider_for(&BGE_SMALL_EN_V15).await
}

fn l2_norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

#[tokio::test]
#[ignore = "requires bge-small GGUF on disk; opt-in via --ignored"]
async fn dim_is_384_and_norm_is_unit() {
    let p = provider().await;
    let vecs = p
        .embed(&["hello world".to_string()])
        .await
        .expect("embed should succeed");
    assert_eq!(vecs.len(), 1);
    assert_eq!(vecs[0].len(), 384, "bge-small dim is pinned at 384");
    let n = l2_norm(&vecs[0]);
    assert!(
        (n - 1.0).abs() < 1e-4,
        "vector should be L2-normalised, got ||v|| = {n}"
    );
}

#[tokio::test]
#[ignore = "requires bge-small GGUF on disk; opt-in via --ignored"]
async fn empty_and_whitespace_inputs_yield_zero_vectors() {
    let p = provider().await;
    let vecs = p
        .embed(&[String::new(), "   \n\t".to_string()])
        .await
        .expect("embed should succeed");
    assert_eq!(vecs.len(), 2);
    for (i, v) in vecs.iter().enumerate() {
        assert_eq!(v.len(), 384, "slot {i} must have full dim even when zero");
        assert!(
            v.iter().all(|x| *x == 0.0),
            "slot {i} must be all-zero for blank input"
        );
    }
}

#[tokio::test]
#[ignore = "requires bge-small GGUF on disk; opt-in via --ignored"]
async fn embeddings_are_deterministic() {
    let p = provider().await;
    let a = p
        .embed(&["consistency check".to_string()])
        .await
        .expect("first embed");
    let b = p
        .embed(&["consistency check".to_string()])
        .await
        .expect("second embed");
    assert_eq!(a, b, "same input must produce identical vectors");
}

#[tokio::test]
#[ignore = "requires bge-small GGUF on disk; opt-in via --ignored"]
async fn batched_matches_single_calls_within_tolerance() {
    let p = provider().await;
    let inputs = vec![
        "the quick brown fox".to_string(),
        "jumps over the lazy dog".to_string(),
        "lorem ipsum dolor sit amet".to_string(),
    ];

    let batched = p.embed(&inputs).await.expect("batched embed");
    let mut singles = Vec::with_capacity(inputs.len());
    for s in &inputs {
        let mut v = p
            .embed(std::slice::from_ref(s))
            .await
            .expect("single embed");
        singles.push(v.remove(0));
    }

    assert_eq!(batched.len(), singles.len());
    for (i, (b, s)) in batched.iter().zip(singles.iter()).enumerate() {
        assert_eq!(b.len(), s.len(), "dim mismatch at slot {i}");
        let l_inf = b
            .iter()
            .zip(s.iter())
            .map(|(x, y)| (x - y).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            l_inf < 1e-3,
            "slot {i}: batched vs single drift {l_inf} >= 1e-3 (padding-induced FP noise)"
        );
    }
}

/// Guards the reason the `llama-cpp-2` / `-sys` pin exists at all. Creating
/// the bge-m3 embedding context used to abort on Metal. This test embeds
/// through `LlamaCppProvider::embed`, which reaches `model.new_context` in
/// `encode_blocking`; an abort would kill the test process outright.
#[tokio::test]
#[ignore = "requires bge-m3 GGUF on disk; opt-in via --ignored"]
async fn bge_m3_context_builds_and_embeds_on_metal() {
    let p = provider_for(&BGE_M3).await;
    let vecs = p
        .embed(&[
            "local model runtime".to_string(),
            "a note about reindexing".to_string(),
        ])
        .await
        .expect("bge-m3 embed should succeed");

    assert_eq!(vecs.len(), 2);
    for (i, v) in vecs.iter().enumerate() {
        assert_eq!(v.len(), 1024, "slot {i}: bge-m3 dim is pinned at 1024");
        assert!(
            v.iter().any(|x| *x != 0.0),
            "slot {i}: non-blank input must not yield the zero vector"
        );
        let n = l2_norm(v);
        assert!(
            (n - 1.0).abs() < 1e-4,
            "slot {i}: vector should be L2-normalised, got ||v|| = {n}"
        );
    }
    assert_ne!(
        vecs[0], vecs[1],
        "different inputs must produce different vectors"
    );
}

#[tokio::test]
#[ignore = "requires bge-small GGUF on disk; opt-in via --ignored"]
async fn long_input_truncates_without_panic() {
    let p = provider().await;
    let long = "lorem ipsum ".repeat(20_000);
    let vecs = p.embed(&[long]).await.expect("long input must not panic");
    assert_eq!(vecs.len(), 1);
    assert_eq!(vecs[0].len(), 384);
    let n = l2_norm(&vecs[0]);
    assert!(
        (n - 1.0).abs() < 1e-4,
        "long-input vector should still be unit-norm, got ||v|| = {n}"
    );
}
