#![allow(clippy::unwrap_used, clippy::expect_used)]

use anyhow::Result;
use async_trait::async_trait;
use memory_hub_embed::{
    EmbedJob, EmbedWorker, EmbedWorkerConfig, EmbeddingProvider, EmbeddingSink, FreshUpdate,
    MockProvider, Pooling, WorkerError,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Default)]
struct InMemorySink {
    fresh: Mutex<HashMap<String, FreshUpdate>>,
    failed: Mutex<Vec<String>>,
}

#[async_trait]
impl EmbeddingSink for InMemorySink {
    async fn upsert_batch_fresh(&self, updates: Vec<FreshUpdate>) -> Result<()> {
        let mut guard = self.fresh.lock().await;
        for u in updates {
            guard.insert(u.row_id.clone(), u);
        }
        Ok(())
    }

    async fn mark_failed(&self, row_ids: &[String]) -> Result<()> {
        let mut guard = self.failed.lock().await;
        guard.extend(row_ids.iter().cloned());
        Ok(())
    }
}

struct FlakyProvider {
    inner: MockProvider,
    fail_count: AtomicUsize,
    attempts: AtomicUsize,
}

impl FlakyProvider {
    fn new(dim: usize, fail_count: usize) -> Self {
        Self {
            inner: MockProvider::new(dim),
            fail_count: AtomicUsize::new(fail_count),
            attempts: AtomicUsize::new(0),
        }
    }

    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl EmbeddingProvider for FlakyProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_tokens(&self) -> usize {
        self.inner.max_tokens()
    }
    fn pooling(&self) -> Pooling {
        self.inner.pooling()
    }
    fn query_prefix(&self) -> Option<&str> {
        self.inner.query_prefix()
    }
    fn doc_prefix(&self) -> Option<&str> {
        self.inner.doc_prefix()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.fail_count.load(Ordering::SeqCst) > 0 {
            self.fail_count.fetch_sub(1, Ordering::SeqCst);
            anyhow::bail!("synthetic provider failure");
        }
        self.inner.embed(texts).await
    }
}

struct AlwaysFailingProvider {
    inner: MockProvider,
    attempts: AtomicUsize,
}

impl AlwaysFailingProvider {
    fn new(dim: usize) -> Self {
        Self {
            inner: MockProvider::new(dim),
            attempts: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for AlwaysFailingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_tokens(&self) -> usize {
        self.inner.max_tokens()
    }
    fn pooling(&self) -> Pooling {
        self.inner.pooling()
    }
    fn query_prefix(&self) -> Option<&str> {
        self.inner.query_prefix()
    }
    fn doc_prefix(&self) -> Option<&str> {
        self.inner.doc_prefix()
    }
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("provider is hard-failing for the test");
    }
}

struct GatedProvider {
    inner: MockProvider,
    gate: Arc<AtomicBool>,
    embed_entered: Arc<AtomicUsize>,
}

impl GatedProvider {
    fn new(dim: usize, gate: Arc<AtomicBool>) -> Self {
        Self::new_with_signal(dim, gate, Arc::new(AtomicUsize::new(0)))
    }

    fn new_with_signal(dim: usize, gate: Arc<AtomicBool>, embed_entered: Arc<AtomicUsize>) -> Self {
        Self {
            inner: MockProvider::new(dim),
            gate,
            embed_entered,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for GatedProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_tokens(&self) -> usize {
        self.inner.max_tokens()
    }
    fn pooling(&self) -> Pooling {
        self.inner.pooling()
    }
    fn query_prefix(&self) -> Option<&str> {
        self.inner.query_prefix()
    }
    fn doc_prefix(&self) -> Option<&str> {
        self.inner.doc_prefix()
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_entered.fetch_add(1, Ordering::SeqCst);
        while !self.gate.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.inner.embed(texts).await
    }
}

fn make_job(i: usize) -> EmbedJob {
    EmbedJob {
        row_id: format!("decision:d-{:06x}", i * 7),
        text: format!("decision: payload {i}"),
        content_hash: format!("hash-{i:06}"),
    }
}

const DIM: usize = 768;

#[tokio::test(flavor = "multi_thread")]
async fn drains_one_hundred_jobs_into_fresh_rows() {
    let provider = Arc::new(MockProvider::new(DIM));
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(
        provider.clone(),
        sink.clone(),
        EmbedWorkerConfig {
            batch_window: Duration::from_millis(20),
            ..EmbedWorkerConfig::default()
        },
    );

    let n: usize = 100;
    for i in 0..n {
        handle.enqueue(make_job(i)).await.expect("enqueue ok");
    }

    handle
        .shutdown_and_drain(Duration::from_secs(5))
        .await
        .expect("worker drained");

    let fresh = sink.fresh.lock().await;
    assert_eq!(fresh.len(), n);
    let failed = sink.failed.lock().await;
    assert!(failed.is_empty(), "expected no failed rows, got {failed:?}");

    let sample = fresh.get("decision:d-000000").expect("row present");
    assert_eq!(sample.vector.len(), DIM);
    assert_eq!(sample.content_hash, "hash-000000");
}

#[tokio::test(flavor = "multi_thread")]
async fn flaky_provider_recovers_within_retry_budget() {
    let provider = Arc::new(FlakyProvider::new(DIM, 2));
    let provider_for_attempts = provider.clone();
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(
        provider,
        sink.clone(),
        EmbedWorkerConfig {
            batch_window: Duration::from_millis(20),
            max_retries: 4,
            retry_base: Duration::from_millis(1),
            ..EmbedWorkerConfig::default()
        },
    );

    handle.enqueue(make_job(0)).await.unwrap();
    handle
        .shutdown_and_drain(Duration::from_secs(2))
        .await
        .unwrap();

    assert_eq!(provider_for_attempts.attempts(), 3);
    let fresh = sink.fresh.lock().await;
    assert_eq!(fresh.len(), 1, "row should have been embedded eventually");
    let failed = sink.failed.lock().await;
    assert!(failed.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn always_failing_provider_marks_rows_failed_after_retries() {
    let provider = Arc::new(AlwaysFailingProvider::new(DIM));
    let provider_for_attempts = provider.clone();
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(
        provider,
        sink.clone(),
        EmbedWorkerConfig {
            batch_window: Duration::from_millis(20),
            max_retries: 3,
            retry_base: Duration::from_millis(1),
            ..EmbedWorkerConfig::default()
        },
    );

    handle.enqueue(make_job(1)).await.unwrap();
    handle.enqueue(make_job(2)).await.unwrap();
    handle
        .shutdown_and_drain(Duration::from_secs(2))
        .await
        .unwrap();

    assert_eq!(provider_for_attempts.attempts.load(Ordering::SeqCst), 3);

    let fresh = sink.fresh.lock().await;
    assert!(fresh.is_empty(), "no rows should have made it to fresh");
    let mut failed = sink.failed.lock().await.clone();
    failed.sort();
    assert_eq!(
        failed,
        vec![
            "decision:d-000007".to_string(),
            "decision:d-00000e".to_string()
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_after_drain_rejects_subsequent_enqueue() {
    let provider = Arc::new(MockProvider::new(DIM));
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(provider, sink.clone(), EmbedWorkerConfig::default());

    handle.enqueue(make_job(0)).await.unwrap();
    handle
        .shutdown_and_drain(Duration::from_secs(2))
        .await
        .unwrap();

    let err = handle
        .enqueue(make_job(99))
        .await
        .expect_err("worker is gone");
    assert!(matches!(err, WorkerError::Closed));
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_drain_times_out_when_worker_is_stuck() {
    let gate = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(GatedProvider::new(DIM, gate.clone()));
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(
        provider,
        sink,
        EmbedWorkerConfig {
            batch_window: Duration::from_millis(5),
            ..EmbedWorkerConfig::default()
        },
    );

    handle.enqueue(make_job(0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    let err = handle
        .shutdown_and_drain(Duration::from_millis(50))
        .await
        .expect_err("expected drain timeout");
    assert!(matches!(err, WorkerError::DrainTimeout(_)));

    gate.store(true, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_clones_share_the_same_queue() {
    let provider = Arc::new(MockProvider::new(DIM));
    let sink = Arc::new(InMemorySink::default());
    let handle = EmbedWorker::spawn(provider, sink.clone(), EmbedWorkerConfig::default());

    let h1 = handle.clone();
    let h2 = handle.clone();
    h1.enqueue(make_job(1)).await.unwrap();
    h2.enqueue(make_job(2)).await.unwrap();

    handle
        .shutdown_and_drain(Duration::from_secs(2))
        .await
        .unwrap();

    let fresh = sink.fresh.lock().await;
    assert_eq!(fresh.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn bounded_queue_applies_backpressure() {
    let gate = Arc::new(AtomicBool::new(false));
    let embed_entered = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(GatedProvider::new_with_signal(
        DIM,
        gate.clone(),
        embed_entered.clone(),
    ));
    let sink = Arc::new(InMemorySink::default());
    let capacity = 4;
    let handle = EmbedWorker::spawn(
        provider,
        sink,
        EmbedWorkerConfig {
            queue_capacity: capacity,
            batch_window: Duration::from_millis(5),
            ..EmbedWorkerConfig::default()
        },
    );

    // Enqueue one job so the worker wakes, pulls it into a batch, and blocks
    // in embed() on the gate. Once embed() is entered the worker stops
    // consuming from the channel.
    handle.enqueue(make_job(0)).await.unwrap();
    while embed_entered.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // The worker is now blocked in embed(); fill the channel to capacity.
    for i in 1..=capacity {
        handle.enqueue(make_job(i)).await.unwrap();
    }

    // The next try_enqueue should hit back-pressure.
    let err = handle
        .try_enqueue(make_job(99))
        .expect_err("queue should be full");
    assert!(matches!(err, WorkerError::Full));

    gate.store(true, Ordering::SeqCst);
    handle
        .shutdown_and_drain(Duration::from_secs(5))
        .await
        .unwrap();
}
