#![allow(clippy::expect_used, clippy::unwrap_used)]

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_embed::MockProvider;
use memory_hub_index::{Projection, SearchFilters, SearchMode, SearchRequest};
use memory_hub_store::{GitStore, Operation, Transaction};
use std::sync::Arc;

fn record(
    key: &str,
    kind: &str,
    content: &str,
) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, kind, content)?),
    })
}

fn seed_store(
    dir: &std::path::Path,
) -> Result<(GitStore, memory_hub_store::Revision), Box<dyn std::error::Error>> {
    Repository::init(dir)?;
    let store = GitStore::open(dir)?;
    let base = store.current()?.revision().clone();
    let revision = store.apply(&Transaction {
        id: "seed".into(),
        expected_revision: base,
        operations: vec![
            Operation::put(record("alpha", "note", "alpha beta gamma")?),
            Operation::put(record("beta", "note", "delta epsilon zeta")?),
            Operation::put(record("gamma", "note", "eta theta iota")?),
        ],
    })?;
    Ok((store, revision.revision))
}

fn search_request(query: &str, revision: &memory_hub_store::Revision) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        limit: 20,
        offset: 0,
        filters: SearchFilters::default(),
        revision: revision.clone(),
    }
}

/// The canonical render text of a record: `"{kind} — {content}"`.
fn render_text(kind: &str, content: &str) -> String {
    format!("{kind} — {content}")
}

#[tokio::test]
async fn fts_only_degradation_when_no_provider() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let (store, revision) = seed_store(project.path())?;
    let snapshot = store.snapshot(&revision)?;
    let projection = Projection::open(project.path().join("index")).await?;
    projection.rebuild(&snapshot).await?;

    let result = projection
        .search(&search_request("alpha", &revision))
        .await?;
    assert_eq!(result.mode, SearchMode::Fts);
    assert!(result.degraded, "no provider => degraded must be true");
    Ok(())
}

#[tokio::test]
async fn vector_rescue_fires_when_bm25_below_threshold() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let (store, revision) = seed_store(project.path())?;
    let snapshot = store.snapshot(&revision)?;
    let provider = Arc::new(MockProvider::new(64));
    let projection = Projection::open(project.path().join("index"))
        .await?
        .with_embed_provider(provider);
    projection.rebuild(&snapshot).await?;

    // Query = exact render text of "alpha". BM25 matches all 3 records (they
    // all have kind "note"), so fts_count = 3 < RESCUE_THRESHOLD = 5. The
    // vector channel finds "alpha" with cosine sim = 1.0 (same text → same
    // hash → same vector).
    let query = render_text("note", "alpha beta gamma");
    let result = projection
        .search(&search_request(&query, &revision))
        .await?;
    assert_eq!(result.mode, SearchMode::Hybrid, "vector rescue should fire");
    assert!(!result.degraded);
    assert!(
        result
            .hits
            .iter()
            .any(|h| h.id == "alpha" && h.vector_score.is_some()),
        "alpha should appear with a vector score"
    );
    Ok(())
}

#[tokio::test]
async fn rrf_fuse_combines_bm25_and_vector_channels() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let (store, revision) = seed_store(project.path())?;
    let snapshot = store.snapshot(&revision)?;
    let provider = Arc::new(MockProvider::new(64));
    let projection = Projection::open(project.path().join("index"))
        .await?
        .with_embed_provider(provider);
    projection.rebuild(&snapshot).await?;

    let query = render_text("note", "alpha beta gamma");
    let result = projection
        .search(&search_request(&query, &revision))
        .await?;
    assert_eq!(result.mode, SearchMode::Hybrid);
    assert!(!result.hits.is_empty());

    // The BM25 hit "alpha" should carry an fts_score.
    let bm25_hit = result.hits.iter().find(|h| h.fts_score.is_some());
    assert!(bm25_hit.is_some(), "BM25 hit should be in fused results");

    // The vector hit "alpha" should carry a vector_score.
    let vec_hit = result.hits.iter().find(|h| h.vector_score.is_some());
    assert!(vec_hit.is_some(), "vector hit should be in fused results");

    // "alpha" should appear exactly once in the fused result (deduplicated).
    let alpha_count = result.hits.iter().filter(|h| h.id == "alpha").count();
    assert_eq!(alpha_count, 1, "RRF should deduplicate by id");

    // The combined_rank of "alpha" (present in both channels) should be
    // higher than a hit present in only one channel.
    let alpha = result
        .hits
        .iter()
        .find(|h| h.id == "alpha")
        .expect("alpha is among the hits");
    let single_channel = result
        .hits
        .iter()
        .filter(|h| h.id != "alpha")
        .max_by(|a, b| {
            a.combined_rank
                .partial_cmp(&b.combined_rank)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    if let Some(other) = single_channel {
        assert!(
            alpha.combined_rank >= other.combined_rank,
            "dual-channel hit should rank >= single-channel: {} vs {}",
            alpha.combined_rank,
            other.combined_rank
        );
    }
    Ok(())
}

#[tokio::test]
async fn fingerprint_mismatch_skips_vector_channel() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let (store, revision) = seed_store(project.path())?;
    let snapshot = store.snapshot(&revision)?;

    // Build with provider A (dim 64).
    let provider_a = Arc::new(MockProvider::new(64));
    let projection = Projection::open(project.path().join("index"))
        .await?
        .with_embed_provider(provider_a);
    projection.rebuild(&snapshot).await?;

    // Reopen with provider B (dim 128) — fingerprint mismatch.
    let provider_b = Arc::new(MockProvider::new(128));
    let projection = Projection::open(project.path().join("index"))
        .await?
        .with_embed_provider(provider_b);

    let query = render_text("note", "alpha beta gamma");
    let result = projection
        .search(&search_request(&query, &revision))
        .await?;
    assert_eq!(
        result.mode,
        SearchMode::Fts,
        "fingerprint mismatch => no vector rescue"
    );
    assert!(
        !result.degraded,
        "provider attached => degraded must be false even on mismatch"
    );
    Ok(())
}

#[tokio::test]
async fn vector_floor_discards_low_similarity_hits() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let (store, revision) = seed_store(project.path())?;
    let snapshot = store.snapshot(&revision)?;
    // High dimension: random hash vectors are near-orthogonal (std ≈ 1/√1024
    // ≈ 0.03), so cosine sim for unrelated texts is well below the 0.35 floor.
    let provider = Arc::new(MockProvider::new(1024));
    let projection = Projection::open(project.path().join("index"))
        .await?
        .with_embed_provider(provider);
    projection.rebuild(&snapshot).await?;

    // Query that does not match any record's render text. All cosine
    // similarities should be near 0 — below the 0.35 floor — so the vector
    // channel produces no surviving hits and mode stays Fts.
    let result = projection
        .search(&search_request("zzzzz-nonexistent-query", &revision))
        .await?;
    assert_eq!(
        result.mode,
        SearchMode::Fts,
        "all vector hits below floor => mode should stay Fts"
    );
    for hit in &result.hits {
        if let Some(score) = hit.vector_score {
            assert!(
                score >= 0.35,
                "vector score {score} is below the rescue floor"
            );
        }
    }
    Ok(())
}
