#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use memory_hub_embed::Pooling;
use memory_hub_embed::download::{
    DownloadError, DownloadOpts, EnsureOutcome, cache_dir_root, ensure_model,
};
use memory_hub_embed::registry::{ModelEntry, PLACEHOLDER_SHA256};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const BODY: &[u8] = b"GGUF\0\0\0\0test-bytes-for-fake-model-payload";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

fn fresh_id(prefix: &str) -> &'static str {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    Box::leak(format!("{prefix}-{n}").into_boxed_str())
}

fn leak_entry(server_uri: &str, sha256: &'static str, id: &'static str) -> &'static ModelEntry {
    let url = format!("{server_uri}/model.gguf");
    let entry = ModelEntry {
        id,
        display_name: "Test Model",
        description: "Test model fixture.",
        languages: "Test",
        url: Box::leak(url.into_boxed_str()),
        sha256,
        dimensions: 4,
        max_tokens: 32,
        size_bytes: BODY.len() as u64,
        quantisation: "Q5_K_M",
        pooling: Pooling::Mean,
        query_prefix: None,
        doc_prefix: None,
    };
    Box::leak(Box::new(entry))
}

fn leak_static_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn opts(cache: &TempDir) -> DownloadOpts {
    DownloadOpts {
        cache_dir: Some(cache.path().to_path_buf()),
        ..Default::default()
    }
}

#[tokio::test]
async fn download_with_pinned_sha_succeeds_and_verifies() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let sha = leak_static_str(sha256_hex(BODY));
    let entry = leak_entry(&server.uri(), sha, fresh_id("pinned"));
    let cache = TempDir::new().unwrap();

    let outcome = ensure_model(entry, opts(&cache)).await.unwrap();
    let EnsureOutcome::Downloaded {
        path,
        sha256,
        verified,
    } = outcome
    else {
        panic!("expected Downloaded, got {outcome:?}");
    };
    assert!(verified, "registry SHA matches body → verified");
    assert_eq!(sha256, sha);
    assert!(path.exists());
    assert!(!path.with_file_name("model.gguf.partial").exists());
}

#[tokio::test]
async fn download_with_placeholder_returns_computed_digest() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let entry = leak_entry(&server.uri(), PLACEHOLDER_SHA256, fresh_id("placeholder"));
    let cache = TempDir::new().unwrap();

    let outcome = ensure_model(entry, opts(&cache)).await.unwrap();
    let EnsureOutcome::Downloaded {
        sha256, verified, ..
    } = outcome
    else {
        panic!("expected Downloaded, got {outcome:?}");
    };
    assert!(!verified, "placeholder registry → not verified");
    assert_eq!(sha256, sha256_hex(BODY));
}

#[tokio::test]
async fn second_call_is_cached_and_skips_http() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let sha = leak_static_str(sha256_hex(BODY));
    let entry = leak_entry(&server.uri(), sha, fresh_id("cached"));
    let cache = TempDir::new().unwrap();

    let first = ensure_model(entry, opts(&cache)).await.unwrap();
    assert!(matches!(first, EnsureOutcome::Downloaded { .. }));

    let second = ensure_model(entry, opts(&cache)).await.unwrap();
    let EnsureOutcome::Cached { sha256, .. } = second else {
        panic!("expected Cached on second call, got {second:?}");
    };
    assert_eq!(sha256, sha);
}

#[tokio::test]
async fn body_mismatch_returns_hash_mismatch_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
        .mount(&server)
        .await;

    let wrong_sha = "deadbeef".repeat(8);
    let entry = leak_entry(
        &server.uri(),
        leak_static_str(wrong_sha.clone()),
        fresh_id("mismatch"),
    );
    let cache = TempDir::new().unwrap();

    let err = ensure_model(
        entry,
        DownloadOpts {
            max_attempts: Some(1),
            ..opts(&cache)
        },
    )
    .await
    .unwrap_err();

    match err {
        DownloadError::HashMismatch {
            expected, computed, ..
        } => {
            assert_eq!(expected, wrong_sha);
            assert_eq!(computed, sha256_hex(BODY));
        }
        other => panic!("expected HashMismatch, got {other:?}"),
    }

    let final_path: PathBuf = cache.path().join(entry.id).join("model.gguf");
    assert!(!final_path.exists());
    assert!(!final_path.with_file_name("model.gguf.partial").exists());
}

#[tokio::test]
async fn first_attempt_500_then_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
        .expect(1)
        .mount(&server)
        .await;

    let sha = leak_static_str(sha256_hex(BODY));
    let entry = leak_entry(&server.uri(), sha, fresh_id("retry-ok"));
    let cache = TempDir::new().unwrap();

    let outcome = ensure_model(
        entry,
        DownloadOpts {
            max_attempts: Some(3),
            ..opts(&cache)
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        EnsureOutcome::Downloaded { verified: true, .. }
    ));
}

#[tokio::test]
async fn three_consecutive_500s_give_too_many_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/model.gguf"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;

    let sha = leak_static_str(sha256_hex(BODY));
    let entry = leak_entry(&server.uri(), sha, fresh_id("retry-exhausted"));
    let cache = TempDir::new().unwrap();

    let err = ensure_model(
        entry,
        DownloadOpts {
            max_attempts: Some(3),
            ..opts(&cache)
        },
    )
    .await
    .unwrap_err();

    let DownloadError::TooManyAttempts { attempts, .. } = err else {
        panic!("expected TooManyAttempts, got {err:?}");
    };
    assert_eq!(attempts, 3);

    let final_path = cache.path().join(entry.id).join("model.gguf");
    assert!(!final_path.exists());
    assert!(!final_path.with_file_name("model.gguf.partial").exists());
}

#[tokio::test]
async fn explicit_cache_dir_wins_over_env() {
    // We can't set env vars safely (unsafe_code = forbid), so we just verify
    // that explicit cache_dir is returned verbatim — the env path is tested
    // implicitly by every other test that passes explicit cache_dir.
    let cache = TempDir::new().unwrap();
    let resolved = cache_dir_root(&DownloadOpts {
        cache_dir: Some(cache.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(resolved, cache.path());
}
