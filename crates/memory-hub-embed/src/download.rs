//! GGUF model downloader.
//!
//! Streams a model's URL into `<cache>/<model.id>/model.gguf`, hashing as it
//! writes and renaming the `.partial` file atomically on success.
//!
//! Every entry point takes a [`DownloadSpec`] — the four fields fetching
//! actually needs — rather than a whole registry entry, so the embedding
//! registry ([`ModelEntry`]) and a future generation registry share one
//! downloader instead of growing a second one.
//!
//! Cache root resolution order:
//!
//!   1. [`DownloadOpts::cache_dir`] (explicit, wins over everything),
//!   2. `$MEMORY_HUB_MODEL_CACHE`,
//!   3. [`dirs::cache_dir`] joined with `memory-hub/models`.
//!
//! Verification: when the registry pins a real SHA-256 the download is
//! rejected on mismatch. When it is [`PLACEHOLDER_SHA256`] the computed digest
//! is returned for the operator to paste back into the registry.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::registry::{ModelEntry, PLACEHOLDER_SHA256};

const ENV_CACHE_DIR: &str = "MEMORY_HUB_MODEL_CACHE";
const DEFAULT_MAX_ATTEMPTS: u8 = 3;
const PARTIAL_SUFFIX: &str = "model.gguf.partial";
const FINAL_NAME: &str = "model.gguf";
const HASH_READ_CHUNK: usize = 1 << 20; // 1 MiB

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("could not resolve a cache directory; set ${ENV_CACHE_DIR} or pass cache_dir")]
    CacheDirMissing,

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("http error fetching {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("server returned status {status} for {url}")]
    BadStatus { url: String, status: u16 },

    #[error("sha-256 mismatch for {model}: expected {expected}, computed {computed}")]
    HashMismatch {
        model: &'static str,
        expected: String,
        computed: String,
    },

    #[error("download of {model} failed after {attempts} attempt(s); last error: {last}")]
    TooManyAttempts {
        model: &'static str,
        attempts: u8,
        last: String,
    },
}

/// What fetching a GGUF needs, independent of what the file is *for*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadSpec {
    pub id: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub size_bytes: u64,
}

impl From<&'static ModelEntry> for DownloadSpec {
    fn from(e: &'static ModelEntry) -> Self {
        Self {
            id: e.id,
            url: e.url,
            sha256: e.sha256,
            size_bytes: e.size_bytes,
        }
    }
}

/// Byte-progress callback: `(downloaded, total)`. `total` is `0` until the
/// HTTP `Content-Length` is known.
pub type ProgressCallback = Arc<dyn Fn(u64, u64) + Send + Sync>;

#[derive(Default)]
pub struct DownloadOpts {
    pub cache_dir: Option<PathBuf>,
    pub progress: Option<ProgressBar>,
    pub on_progress: Option<ProgressCallback>,
    pub max_attempts: Option<u8>,
    pub http_client: Option<reqwest::Client>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    Cached {
        path: PathBuf,
        sha256: String,
    },
    Downloaded {
        path: PathBuf,
        sha256: String,
        verified: bool,
    },
}

impl EnsureOutcome {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            EnsureOutcome::Cached { path, .. } | EnsureOutcome::Downloaded { path, .. } => path,
        }
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        match self {
            EnsureOutcome::Cached { sha256, .. } | EnsureOutcome::Downloaded { sha256, .. } => {
                sha256
            }
        }
    }
}

/// Resolve the cache root according to the precedence in the module doc.
///
/// # Errors
///
/// Returns [`DownloadError::CacheDirMissing`] when no cache directory can be
/// resolved.
pub fn cache_dir_root(opts: &DownloadOpts) -> Result<PathBuf, DownloadError> {
    if let Some(d) = &opts.cache_dir {
        return Ok(d.clone());
    }
    if let Ok(env) = std::env::var(ENV_CACHE_DIR)
        && !env.is_empty()
    {
        return Ok(PathBuf::from(env));
    }
    dirs::cache_dir()
        .map(|d| d.join("memory-hub").join("models"))
        .ok_or(DownloadError::CacheDirMissing)
}

/// Path the GGUF file ends up at once a download succeeds.
///
/// # Errors
///
/// Returns [`DownloadError::CacheDirMissing`] when no cache directory can be
/// resolved.
pub fn model_path(
    spec: impl Into<DownloadSpec>,
    opts: &DownloadOpts,
) -> Result<PathBuf, DownloadError> {
    Ok(cache_dir_root(opts)?.join(spec.into().id).join(FINAL_NAME))
}

/// Download `entry` to the cache, returning a path that is guaranteed to
/// exist and (when the registry pinned a real SHA-256) to match it.
///
/// Idempotent: a subsequent call sees the file on disk, re-hashes it, and
/// returns [`EnsureOutcome::Cached`] without touching the network.
///
/// # Errors
///
/// Returns [`DownloadError`] for I/O, HTTP, hash mismatch, or exhausted
/// retry budget.
pub async fn ensure_model(
    spec: impl Into<DownloadSpec>,
    opts: DownloadOpts,
) -> Result<EnsureOutcome, DownloadError> {
    let entry = spec.into();
    let final_path = model_path(entry, &opts)?;
    let dir = final_path.parent().ok_or_else(|| DownloadError::Io {
        path: final_path.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "model path has no parent"),
    })?;
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|source| DownloadError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
    let partial = dir.join(PARTIAL_SUFFIX);

    if final_path.exists() {
        let computed = hash_file(&final_path).await?;
        if entry.sha256 == PLACEHOLDER_SHA256 || entry.sha256.eq_ignore_ascii_case(&computed) {
            return Ok(EnsureOutcome::Cached {
                path: final_path,
                sha256: computed,
            });
        }
        tracing::warn!(
            model = entry.id,
            expected = entry.sha256,
            computed = %computed,
            "cached GGUF hash does not match registry — re-downloading"
        );
        tokio::fs::remove_file(&final_path)
            .await
            .map_err(|source| DownloadError::Io {
                path: final_path.clone(),
                source,
            })?;
    }

    let client = opts.http_client.unwrap_or_else(default_client);
    let max_attempts = opts.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS).max(1);
    let progress = opts.progress;
    let on_progress = opts.on_progress;
    let mut last_err: Option<String> = None;

    for attempt in 1..=max_attempts {
        if partial.exists() {
            let _ = tokio::fs::remove_file(&partial).await;
        }

        match download_once(
            &entry,
            &client,
            &partial,
            progress.as_ref(),
            on_progress.as_ref(),
        )
        .await
        {
            Ok(computed) => {
                if entry.sha256 != PLACEHOLDER_SHA256
                    && !entry.sha256.eq_ignore_ascii_case(&computed)
                {
                    let _ = tokio::fs::remove_file(&partial).await;
                    return Err(DownloadError::HashMismatch {
                        model: entry.id,
                        expected: entry.sha256.to_string(),
                        computed,
                    });
                }
                tokio::fs::rename(&partial, &final_path)
                    .await
                    .map_err(|source| DownloadError::Io {
                        path: final_path.clone(),
                        source,
                    })?;
                let verified = entry.sha256 != PLACEHOLDER_SHA256;
                return Ok(EnsureOutcome::Downloaded {
                    path: final_path,
                    sha256: computed,
                    verified,
                });
            }
            Err(e) => {
                tracing::warn!(model = entry.id, attempt, error = %e, "download attempt failed");
                last_err = Some(e.to_string());
                if attempt < max_attempts {
                    let backoff_ms = 200u64.saturating_mul(1u64 << (attempt - 1));
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }

    let _ = tokio::fs::remove_file(&partial).await;
    Err(DownloadError::TooManyAttempts {
        model: entry.id,
        attempts: max_attempts,
        last: last_err.unwrap_or_else(|| "<unknown>".to_string()),
    })
}

fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("memory-hub/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn download_once(
    entry: &DownloadSpec,
    client: &reqwest::Client,
    partial: &Path,
    progress: Option<&ProgressBar>,
    on_progress: Option<&ProgressCallback>,
) -> Result<String, DownloadError> {
    let resp = client
        .get(entry.url)
        .send()
        .await
        .map_err(|source| DownloadError::Http {
            url: entry.url.to_string(),
            source,
        })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(DownloadError::BadStatus {
            url: entry.url.to_string(),
            status: status.as_u16(),
        });
    }

    let total = resp.content_length().unwrap_or(entry.size_bytes);
    if let Some(pb) = progress {
        pb.set_length(total);
        pb.set_position(0);
    }
    if let Some(cb) = on_progress {
        cb(0, total);
    }

    let mut file = File::create(partial)
        .await
        .map_err(|source| DownloadError::Io {
            path: partial.to_path_buf(),
            source,
        })?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    let mut written: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| DownloadError::Http {
            url: entry.url.to_string(),
            source,
        })?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|source| DownloadError::Io {
                path: partial.to_path_buf(),
                source,
            })?;
        written = written.saturating_add(chunk.len() as u64);
        if let Some(pb) = progress {
            pb.set_position(written);
        }
        if let Some(cb) = on_progress {
            cb(written, total);
        }
    }
    file.flush().await.map_err(|source| DownloadError::Io {
        path: partial.to_path_buf(),
        source,
    })?;
    file.sync_all().await.map_err(|source| DownloadError::Io {
        path: partial.to_path_buf(),
        source,
    })?;
    drop(file);

    if let Some(pb) = progress {
        pb.finish_and_clear();
    }

    Ok(format!("{:x}", hasher.finalize()))
}

async fn hash_file(path: &Path) -> Result<String, DownloadError> {
    let mut f = File::open(path).await.map_err(|source| DownloadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_READ_CHUNK];
    loop {
        let n = f.read(&mut buf).await.map_err(|source| DownloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Sync variant of [`hash_file`] using `std::fs`.
fn hash_file_sync(path: &Path) -> Result<String, DownloadError> {
    let mut f = std::fs::File::open(path).map_err(|source| DownloadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_READ_CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|source| DownloadError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Result of a sync model presence check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelVerification {
    /// Model file is on disk and its SHA-256 matches the registry pin.
    Present {
        path: PathBuf,
        sha256: String,
        verified: bool,
    },
    /// Model file is not on disk.
    Missing,
    /// Model file is on disk but its SHA-256 does not match the registry pin.
    Broken {
        path: PathBuf,
        expected: String,
        computed: String,
    },
}

/// Synchronously check whether a model is on disk and its hash matches the
/// registry pin. Does not touch the network.
///
/// # Errors
///
/// Returns [`DownloadError`] when the cache directory cannot be resolved or
/// the file cannot be read.
pub fn verify_model_sync(
    spec: impl Into<DownloadSpec>,
    opts: &DownloadOpts,
) -> Result<ModelVerification, DownloadError> {
    let entry = spec.into();
    let path = model_path(entry, opts)?;
    if !path.exists() {
        return Ok(ModelVerification::Missing);
    }
    let computed = hash_file_sync(&path)?;
    if entry.sha256 == PLACEHOLDER_SHA256 || entry.sha256.eq_ignore_ascii_case(&computed) {
        Ok(ModelVerification::Present {
            path,
            sha256: computed,
            verified: entry.sha256 != PLACEHOLDER_SHA256,
        })
    } else {
        Ok(ModelVerification::Broken {
            path,
            expected: entry.sha256.to_string(),
            computed,
        })
    }
}
