//! Background embedding worker for the write-path.
//!
//! `enqueue` posts an [`EmbedJob`] to the worker and returns straight away —
//! the canonical record is already on disk, and the worker fills in the real
//! vector asynchronously.
//!
//! The worker uses a **bounded** queue (back-pressure instead of unbounded
//! growth), batches jobs (up to [`EmbedWorkerConfig::batch_size`] or a
//! [`EmbedWorkerConfig::batch_window`] deadline, whichever fires first),
//! retries on provider errors with exponential backoff, and on terminal
//! failure marks the affected rows as `failed` so search can ignore them.
//! Writes go through an [`EmbeddingSink`] abstraction so the worker itself
//! does not depend on `LanceDB`.

use crate::provider::EmbeddingProvider;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, warn};

/// One unit of work flowing through the embedding worker.
#[derive(Debug, Clone)]
pub struct EmbedJob {
    /// Composite key in the projection — `<kind>:<id>`.
    pub row_id: String,
    /// Canonical embedding input text (renderer output, without model prefix).
    pub text: String,
    /// blake3 hex digest of `text` — written back into the row so the next
    /// rebuild can detect that this content is already fresh.
    pub content_hash: String,
}

/// Single row update the worker hands to the sink after a successful embed
/// batch.
#[derive(Debug, Clone)]
pub struct FreshUpdate {
    pub row_id: String,
    pub vector: Vec<f32>,
    pub content_hash: String,
}

/// Trait the worker uses to push completed embeddings back into storage.
///
/// The projection ships the production implementation; tests use an in-memory
/// implementation so the worker logic can be exercised without `LanceDB`.
#[async_trait::async_trait]
pub trait EmbeddingSink: Send + Sync {
    /// Persist a batch of freshly-embedded rows. Implementations should
    /// upsert by `row_id` and flip the row's state to `fresh`.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    async fn upsert_batch_fresh(&self, updates: Vec<FreshUpdate>) -> anyhow::Result<()>;

    /// Flip rows to `state = failed` after the worker exhausted its retry
    /// budget. Implementations should leave the placeholder vector + hash
    /// alone so a future rebuild can recover.
    ///
    /// # Errors
    ///
    /// Returns an error when the write fails.
    async fn mark_failed(&self, row_ids: &[String]) -> anyhow::Result<()>;
}

/// Knobs for the worker actor.
#[derive(Debug, Clone)]
pub struct EmbedWorkerConfig {
    /// Maximum number of jobs buffered in the channel before `enqueue`
    /// applies back-pressure.
    pub queue_capacity: usize,
    pub batch_size: usize,
    pub batch_window: Duration,
    pub max_retries: u32,
    pub retry_base: Duration,
}

impl Default for EmbedWorkerConfig {
    fn default() -> Self {
        Self {
            queue_capacity: 256,
            batch_size: 32,
            batch_window: Duration::from_millis(100),
            max_retries: 3,
            retry_base: Duration::from_millis(200),
        }
    }
}

/// Worker-side errors surfaced to the caller of [`EmbedWorkerHandle`].
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("embedding worker queue is full")]
    Full,
    #[error("embedding worker queue is closed")]
    Closed,
    #[error("embedding worker did not drain within {0:?}")]
    DrainTimeout(Duration),
}

pub struct EmbedWorker;

impl EmbedWorker {
    /// Spawn the worker on the current tokio runtime. Returns a clone-able
    /// handle for enqueue and shutdown.
    #[must_use]
    pub fn spawn(
        provider: Arc<dyn EmbeddingProvider>,
        sink: Arc<dyn EmbeddingSink>,
        config: EmbedWorkerConfig,
    ) -> EmbedWorkerHandle {
        let (sender, receiver) = mpsc::channel::<WorkerMessage>(config.queue_capacity.max(1));
        let task = tokio::spawn(worker_loop(receiver, provider, sink, config));
        EmbedWorkerHandle {
            inner: Arc::new(EmbedWorkerHandleInner {
                sender,
                task: Mutex::new(Some(task)),
            }),
        }
    }
}

#[derive(Clone)]
pub struct EmbedWorkerHandle {
    inner: Arc<EmbedWorkerHandleInner>,
}

struct EmbedWorkerHandleInner {
    sender: mpsc::Sender<WorkerMessage>,
    task: Mutex<Option<JoinHandle<()>>>,
}

enum WorkerMessage {
    Job(EmbedJob),
    Shutdown(oneshot::Sender<()>),
}

impl EmbedWorkerHandle {
    /// Push a job onto the queue. Returns [`WorkerError::Full`] when the
    /// bounded queue is at capacity (back-pressure), or [`WorkerError::Closed`]
    /// when the worker has exited.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when the queue is full or closed.
    pub async fn enqueue(&self, job: EmbedJob) -> Result<(), WorkerError> {
        self.inner
            .sender
            .send(WorkerMessage::Job(job))
            .await
            .map_err(|_| WorkerError::Closed)
    }

    /// Try to push a job without awaiting. Returns `Err(Full)` when the
    /// bounded queue is at capacity.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError`] when the queue is full or closed.
    pub fn try_enqueue(&self, job: EmbedJob) -> Result<(), WorkerError> {
        self.inner
            .sender
            .try_send(WorkerMessage::Job(job))
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => WorkerError::Full,
                mpsc::error::TrySendError::Closed(_) => WorkerError::Closed,
            })
    }

    /// Tell the worker to drain its queue, wait up to `timeout`, then return.
    /// On timeout the worker task is aborted; any unprocessed jobs stay as
    /// `pending` rows and the next rebuild reconciles them.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::DrainTimeout`] when the worker does not finish
    /// within the timeout.
    pub async fn shutdown_and_drain(&self, timeout: Duration) -> Result<(), WorkerError> {
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .inner
            .sender
            .send(WorkerMessage::Shutdown(done_tx))
            .await
            .is_err()
        {
            return Ok(());
        }
        if let Ok(Ok(()) | Err(_)) = tokio::time::timeout(timeout, done_rx).await {
            self.join_task().await;
            Ok(())
        } else {
            if let Some(task) = self.inner.task.lock().await.take() {
                task.abort();
            }
            Err(WorkerError::DrainTimeout(timeout))
        }
    }

    async fn join_task(&self) {
        let mut guard = self.inner.task.lock().await;
        if let Some(task) = guard.take() {
            let _ = task.await;
        }
    }
}

async fn worker_loop(
    mut rx: mpsc::Receiver<WorkerMessage>,
    provider: Arc<dyn EmbeddingProvider>,
    sink: Arc<dyn EmbeddingSink>,
    config: EmbedWorkerConfig,
) {
    loop {
        let Some(first) = rx.recv().await else {
            return;
        };

        let mut batch: Vec<EmbedJob> = Vec::with_capacity(config.batch_size);
        let mut shutdown_ack: Option<oneshot::Sender<()>> = None;

        match first {
            WorkerMessage::Job(j) => batch.push(j),
            WorkerMessage::Shutdown(ack) => {
                drain_remaining(&mut rx, &mut batch, &mut Some(ack), &mut shutdown_ack);
                if !batch.is_empty() {
                    process_batch(provider.as_ref(), sink.as_ref(), batch, &config).await;
                }
                if let Some(ack) = shutdown_ack {
                    let _ = ack.send(());
                }
                return;
            }
        }

        let window_deadline = Instant::now() + config.batch_window;
        while batch.len() < config.batch_size {
            let remaining = window_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(WorkerMessage::Job(j))) => batch.push(j),
                Ok(Some(WorkerMessage::Shutdown(ack))) => {
                    let mut taken = Some(ack);
                    drain_remaining(&mut rx, &mut batch, &mut taken, &mut shutdown_ack);
                    break;
                }
                Ok(None) | Err(_) => break,
            }
        }

        process_batch(provider.as_ref(), sink.as_ref(), batch, &config).await;

        if let Some(ack) = shutdown_ack {
            let _ = ack.send(());
            return;
        }
    }
}

fn drain_remaining(
    rx: &mut mpsc::Receiver<WorkerMessage>,
    batch: &mut Vec<EmbedJob>,
    incoming_ack: &mut Option<oneshot::Sender<()>>,
    out_ack: &mut Option<oneshot::Sender<()>>,
) {
    if let Some(ack) = incoming_ack.take() {
        *out_ack = Some(ack);
    }
    while let Ok(msg) = rx.try_recv() {
        match msg {
            WorkerMessage::Job(j) => batch.push(j),
            WorkerMessage::Shutdown(ack) => {
                if let Some(old) = out_ack.replace(ack) {
                    let _ = old.send(());
                }
            }
        }
    }
}

async fn process_batch(
    provider: &dyn EmbeddingProvider,
    sink: &dyn EmbeddingSink,
    batch: Vec<EmbedJob>,
    config: &EmbedWorkerConfig,
) {
    if batch.is_empty() {
        return;
    }

    let doc_prefix = provider.doc_prefix().map(str::to_string);
    let texts: Vec<String> = batch
        .iter()
        .map(|j| match &doc_prefix {
            Some(p) => format!("{p}{}", j.text),
            None => j.text.clone(),
        })
        .collect();

    let vectors = match embed_with_retry(provider, &texts, config).await {
        Ok(v) => v,
        Err(e) => {
            error!(
                error = %e,
                jobs = batch.len(),
                "embedding batch failed after retries; marking rows as failed"
            );
            let ids: Vec<String> = batch.iter().map(|j| j.row_id.clone()).collect();
            if let Err(sink_err) = sink.mark_failed(&ids).await {
                error!(error = %sink_err, "sink.mark_failed failed");
            }
            return;
        }
    };

    if vectors.len() != batch.len() {
        error!(
            returned = vectors.len(),
            expected = batch.len(),
            "provider returned wrong vector count; marking rows as failed"
        );
        let ids: Vec<String> = batch.iter().map(|j| j.row_id.clone()).collect();
        if let Err(sink_err) = sink.mark_failed(&ids).await {
            error!(error = %sink_err, "sink.mark_failed failed");
        }
        return;
    }

    let updates: Vec<FreshUpdate> = batch
        .into_iter()
        .zip(vectors)
        .map(|(j, vector)| FreshUpdate {
            row_id: j.row_id,
            vector,
            content_hash: j.content_hash,
        })
        .collect();

    if let Err(e) = sink.upsert_batch_fresh(updates).await {
        error!(error = %e, "sink.upsert_batch_fresh failed");
    }
}

async fn embed_with_retry(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
    config: &EmbedWorkerConfig,
) -> anyhow::Result<Vec<Vec<f32>>> {
    let attempts = config.max_retries.max(1);
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=attempts {
        match provider.embed(texts).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt >= attempts {
                    last_err = Some(e);
                    break;
                }
                let backoff = config.retry_base * (1u32 << (attempt - 1));
                warn!(
                    error = %e,
                    attempt,
                    next_backoff_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
                    "embedding batch failed; retrying"
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("embed failed without an error value")))
}
