//! [`LlamaCppProvider`] — production embedding backend.
//!
//! Owns the process-wide [`LlamaBackend`] and a lazily-loaded `Arc<LlamaModel>`.
//! Each [`embed`](LlamaCppProvider::embed) call spawns a blocking task that
//! tokenises, packs sub-batches into a fresh [`LlamaContext`], runs
//! `ctx.decode`, pools at the context level, then L2-normalises.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::provider::{EmbeddingProvider, Pooling};
use crate::registry::ModelEntry;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use llama_cpp_2::context::params::{LlamaContextParams, LlamaPoolingType};
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::token::LlamaToken;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use tokio::sync::OnceCell;

const BATCH_TOKEN_BUDGET: usize = 512;
const N_SEQ_MAX: usize = 64;

static BACKEND: OnceLock<std::result::Result<LlamaBackend, String>> = OnceLock::new();

/// Initialise (or return) the global llama.cpp backend. Cheap after the first
/// call. Returns the same error string forever if the first init failed.
///
/// Logs from llama.cpp / ggml are silenced via `LlamaBackend::void_logs()` so
/// they never leak to stderr.
///
/// # Errors
///
/// Returns an error when `llama_backend_init` fails.
pub fn ensure_backend() -> Result<&'static LlamaBackend> {
    let entry = BACKEND.get_or_init(|| {
        LlamaBackend::init()
            .map(|mut backend| {
                backend.void_logs();
                backend
            })
            .map_err(|e| format!("{e}"))
    });
    entry
        .as_ref()
        .map_err(|e| anyhow!("llama backend init failed: {e}"))
}

/// Human-readable name of the compile-time-selected acceleration backend.
#[must_use]
pub fn backend_name() -> &'static str {
    if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
        "Metal"
    } else {
        "CPU"
    }
}

pub struct LlamaCppProvider {
    entry: &'static ModelEntry,
    model_path: PathBuf,
    model: OnceCell<Arc<LlamaModel>>,
}

impl LlamaCppProvider {
    /// Construct a provider against a downloaded GGUF file. The file is not
    /// opened until [`embed`](Self::embed) or [`warm_up`](Self::warm_up) is
    /// first called.
    #[must_use]
    pub fn new(entry: &'static ModelEntry, model_path: PathBuf) -> Self {
        Self {
            entry,
            model_path,
            model: OnceCell::new(),
        }
    }

    #[must_use]
    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    #[must_use]
    pub fn entry(&self) -> &'static ModelEntry {
        self.entry
    }

    /// Eagerly load the model so the first `embed` call doesn't pay the cold
    /// mmap+parse cost. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error when the model file cannot be loaded.
    pub async fn warm_up(&self) -> Result<()> {
        self.load_model().await.map(|_| ())
    }

    async fn load_model(&self) -> Result<Arc<LlamaModel>> {
        let arc = self
            .model
            .get_or_try_init(|| async {
                let path = self.model_path.clone();
                let handle = tokio::task::spawn_blocking(move || -> Result<Arc<LlamaModel>> {
                    let backend = ensure_backend()?;
                    let params = LlamaModelParams::default().with_n_gpu_layers(999);
                    let model = LlamaModel::load_from_file(backend, &path, &params)
                        .map_err(|e| anyhow!("load_from_file({}): {e}", path.display()))?;
                    Ok(Arc::new(model))
                });
                handle
                    .await
                    .map_err(|e| anyhow!("model-load task panicked: {e}"))?
            })
            .await?;
        Ok(Arc::clone(arc))
    }
}

#[async_trait]
impl EmbeddingProvider for LlamaCppProvider {
    fn name(&self) -> &'static str {
        "llama-cpp"
    }

    fn model_id(&self) -> &str {
        self.entry.id
    }

    fn dimensions(&self) -> usize {
        self.entry.dimensions
    }

    fn max_tokens(&self) -> usize {
        self.entry.max_tokens
    }

    fn pooling(&self) -> Pooling {
        self.entry.pooling
    }

    fn query_prefix(&self) -> Option<&str> {
        self.entry.query_prefix
    }

    fn doc_prefix(&self) -> Option<&str> {
        self.entry.doc_prefix
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let model = self.load_model().await?;
        let entry = self.entry;
        let texts = texts.to_vec();
        let n_threads = pick_n_threads();

        tokio::task::spawn_blocking(move || encode_blocking(&model, entry, &texts, n_threads))
            .await
            .map_err(|e| anyhow!("embed task panicked: {e}"))?
    }
}

fn encode_blocking(
    model: &LlamaModel,
    entry: &ModelEntry,
    texts: &[String],
    n_threads: i32,
) -> Result<Vec<Vec<f32>>> {
    let backend = ensure_backend()?;
    let dim = entry.dimensions;
    let mut out: Vec<Vec<f32>> = (0..texts.len()).map(|_| Vec::new()).collect();

    let tokens_per_text: Vec<Option<Vec<LlamaToken>>> = texts
        .iter()
        .map(|t| tokenize_one(model, entry, t))
        .collect::<Result<_>>()?;

    for (i, opt) in tokens_per_text.iter().enumerate() {
        if opt.is_none() {
            out[i] = vec![0.0; dim];
        }
    }

    let mut order: Vec<(usize, usize)> = tokens_per_text
        .iter()
        .enumerate()
        .filter_map(|(i, opt)| opt.as_ref().map(|t| (i, t.len())))
        .collect();
    if order.is_empty() {
        return Ok(out);
    }
    order.sort_by_key(|entry| std::cmp::Reverse(entry.1));

    let token_budget = BATCH_TOKEN_BUDGET.max(entry.max_tokens);
    let n_ctx = NonZeroU32::new(u32::try_from(token_budget).unwrap_or(u32::MAX))
        .ok_or_else(|| anyhow!("token budget evaluated to zero"))?;

    let ctx_params = LlamaContextParams::default()
        .with_embeddings(true)
        .with_n_batch(token_budget as u32)
        .with_n_ubatch(token_budget as u32)
        .with_n_ctx(Some(n_ctx))
        .with_n_seq_max(N_SEQ_MAX as u32)
        .with_n_threads(n_threads)
        .with_n_threads_batch(n_threads)
        .with_pooling_type(to_llama_pool(entry.pooling));

    let mut ctx = model
        .new_context(backend, ctx_params)
        .map_err(|e| anyhow!("new_context failed: {e}"))?;

    let mut cursor = 0;
    while cursor < order.len() {
        let mut batch = LlamaBatch::new(token_budget, N_SEQ_MAX as i32);
        let mut tokens_in_batch = 0usize;
        let mut seqs: Vec<(usize, i32)> = Vec::new();

        while cursor < order.len() {
            let (orig_idx, tok_len) = order[cursor];
            if tok_len > token_budget {
                return Err(anyhow!(
                    "tokenised sequence ({tok_len} tok) exceeds batch budget ({token_budget}) — \
                     truncation invariant violated"
                ));
            }
            if !seqs.is_empty()
                && (tokens_in_batch + tok_len > token_budget || seqs.len() >= N_SEQ_MAX)
            {
                break;
            }
            let seq_id = seqs.len() as i32;
            let toks = tokens_per_text[orig_idx]
                .as_ref()
                .ok_or_else(|| anyhow!("indexed entry is unexpectedly None"))?;
            batch
                .add_sequence(toks, seq_id, true)
                .map_err(|e| anyhow!("batch add_sequence failed: {e}"))?;
            seqs.push((orig_idx, seq_id));
            tokens_in_batch += tok_len;
            cursor += 1;
        }

        ctx.decode(&mut batch)
            .map_err(|e| anyhow!("ctx.decode failed: {e}"))?;

        for (orig_idx, seq_id) in seqs {
            let emb = ctx
                .embeddings_seq_ith(seq_id)
                .map_err(|e| anyhow!("embeddings_seq_ith({seq_id}): {e}"))?;
            if emb.len() != dim {
                return Err(anyhow!(
                    "model returned {} dims, registry pinned {dim} for {}",
                    emb.len(),
                    entry.id
                ));
            }
            let mut v = emb.to_vec();
            l2_normalise(&mut v);
            out[orig_idx] = v;
        }
    }

    Ok(out)
}

fn tokenize_one(
    model: &LlamaModel,
    entry: &ModelEntry,
    text: &str,
) -> Result<Option<Vec<LlamaToken>>> {
    if text.trim().is_empty() {
        return Ok(None);
    }

    let snippet: String = text
        .chars()
        .take(entry.max_tokens.saturating_mul(4))
        .collect();

    let mut toks = model
        .str_to_token(&snippet, AddBos::Always)
        .map_err(|e| anyhow!("str_to_token failed: {e}"))?;

    if toks.len() > entry.max_tokens {
        let sep = model.token_sep();
        if sep.0 >= 0 {
            toks.truncate(entry.max_tokens.saturating_sub(1));
            toks.push(sep);
        } else {
            toks.truncate(entry.max_tokens);
        }
    }

    if toks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(toks))
    }
}

fn l2_normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn to_llama_pool(p: Pooling) -> LlamaPoolingType {
    match p {
        Pooling::Mean => LlamaPoolingType::Mean,
        Pooling::Cls => LlamaPoolingType::Cls,
        Pooling::LastToken => LlamaPoolingType::Last,
    }
}

fn pick_n_threads() -> i32 {
    if let Ok(s) = std::env::var("MEMORY_HUB_EMBED_THREADS")
        && let Ok(n) = s.parse::<i32>()
    {
        return n.max(1);
    }
    std::thread::available_parallelism()
        .map(|n| i32::try_from(n.get()).unwrap_or(4))
        .unwrap_or(4)
        .max(1)
}
