//! `memory-hub model` subcommand handlers.
//!
//! Five commands:
//!
//! - `download <id>` — stream GGUF into cache with progress bar and SHA-256
//!   verification.
//! - `list` — table of all registry models with on-disk and active status.
//! - `show <id>` — detailed metadata for one model.
//! - `use <id>` — persist active model choice to config (does not download).
//! - `benchmark <id>` — throughput grid (batch × token-length), warning when
//!   below floor. Requires the model on disk.

use std::io::{self, Write};
use std::time::Instant;

use indicatif::{ProgressBar, ProgressStyle};
use memory_hub_embed::{
    DownloadOpts, DownloadSpec, EmbeddingProvider, EnsureOutcome, LlamaCppProvider, ModelEntry,
    ModelVerification, PLACEHOLDER_SHA256, Pooling, all_models, backend_name, cached_model_path,
    ensure_model, find_model, verify_model_sync,
};
use serde::Serialize;

use crate::config;
use crate::exit::Code;

/// Render a byte count as megabytes for display. Model sizes are hundreds of
/// megabytes, far inside `f64`'s exact range.
#[allow(clippy::cast_precision_loss)]
fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

/// Throughput floor below which the benchmark emits a warning (embeddings/sec).
const BENCHMARK_FLOOR: f64 = 2.0;

const BATCH_SIZES: &[usize] = &[1, 4, 16];
const TOKEN_LENGTHS: &[usize] = &[16, 128, 512];

pub(crate) fn download(id: &str, output: Output) -> Code {
    let Some(entry) = find_model(id) else {
        return model_not_found(id, output);
    };

    let progress = match output {
        Output::Human => {
            let pb = ProgressBar::new(entry.size_bytes);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] \
                     {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar())
                .progress_chars("=>-"),
            );
            Some(pb)
        }
        Output::Json => None,
    };

    let opts = DownloadOpts {
        progress,
        ..Default::default()
    };

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(error) => {
            return internal_error(&format!("failed to start async runtime: {error}"), output);
        }
    };

    match runtime.block_on(ensure_model(DownloadSpec::from(entry), opts)) {
        Ok(outcome) => match output {
            Output::Human => {
                let _ = print_download_human(entry, &outcome);
                Code::Success
            }
            Output::Json => {
                let _ = print_download_json(entry, &outcome);
                Code::Success
            }
        },
        Err(error) => {
            let msg = format!("model download failed: {error}");
            match output {
                Output::Human => {
                    eprintln!("memory-hub: {msg}");
                }
                Output::Json => {
                    let _ = print_error_json(&msg);
                }
            }
            Code::Internal
        }
    }
}

pub(crate) fn list(output: Output) -> Code {
    let active = config::resolve_active_model();
    let configured = config::configured_model_id();
    let opts = DownloadOpts::default();

    let rows: Vec<ListRow> = all_models()
        .iter()
        .map(|entry| {
            let on_disk = match verify_model_sync(*entry, &opts) {
                Ok(ModelVerification::Present { .. }) => true,
                Ok(ModelVerification::Missing | ModelVerification::Broken { .. }) | Err(_) => false,
            };
            let active_flag = entry.id == active.id;
            let is_configured = configured.as_deref().is_some_and(|id| id == entry.id);
            ListRow {
                id: entry.id.to_owned(),
                display_name: entry.display_name.to_owned(),
                languages: entry.languages.to_owned(),
                dimensions: entry.dimensions,
                size_bytes: entry.size_bytes,
                on_disk,
                active: active_flag,
                configured: is_configured,
            }
        })
        .collect();

    match output {
        Output::Human => {
            let _ = print_list_human(&rows);
            let missing: Vec<&ListRow> = rows.iter().filter(|r| !r.on_disk).collect();
            if !missing.is_empty() {
                println!();
                for row in &missing {
                    println!(
                        "  Hint: download with `memory-hub model download {}`",
                        row.id
                    );
                }
            }
            Code::Success
        }
        Output::Json => {
            let _ = print_list_json(&rows);
            Code::Success
        }
    }
}

pub(crate) fn show(id: &str, output: Output) -> Code {
    let Some(entry) = find_model(id) else {
        return model_not_found(id, output);
    };

    let opts = DownloadOpts::default();
    let cached_path = cached_model_path(DownloadSpec::from(entry), &opts).ok();
    let verification = verify_model_sync(entry, &opts).ok();
    let backend = backend_name();
    let active = config::resolve_active_model();
    let is_active = entry.id == active.id;

    let detail = ShowDetail {
        id: entry.id.to_owned(),
        display_name: entry.display_name.to_owned(),
        description: entry.description.to_owned(),
        languages: entry.languages.to_owned(),
        dimensions: entry.dimensions,
        max_tokens: entry.max_tokens,
        quantisation: entry.quantisation.to_owned(),
        pooling: pooling_label(entry.pooling),
        query_prefix: entry.query_prefix.map(str::to_owned),
        doc_prefix: entry.doc_prefix.map(str::to_owned),
        size_bytes: entry.size_bytes,
        url: entry.url.to_owned(),
        sha256: entry.sha256.to_owned(),
        cached_path: cached_path.map(|p| p.display().to_string()),
        on_disk: matches!(verification, Some(ModelVerification::Present { .. })),
        backend: backend.to_owned(),
        active: is_active,
    };

    match output {
        Output::Human => {
            let _ = print_show_human(&detail);
            Code::Success
        }
        Output::Json => {
            let _ = print_show_json(&detail);
            Code::Success
        }
    }
}

pub(crate) fn use_model(id: &str, output: Output) -> Code {
    let Some(entry) = find_model(id) else {
        return model_not_found(id, output);
    };

    if let Err(error) = config::set_active_model(id) {
        let msg = format!("failed to write config: {error}");
        match output {
            Output::Human => eprintln!("memory-hub: {msg}"),
            Output::Json => {
                let _ = print_error_json(&msg);
            }
        }
        return Code::Internal;
    }

    let opts = DownloadOpts::default();
    let on_disk = matches!(
        verify_model_sync(entry, &opts),
        Ok(ModelVerification::Present { .. })
    );

    match output {
        Output::Human => {
            println!("Active model set to: {} ({})", entry.id, entry.display_name);
            if !on_disk {
                println!(
                    "  Hint: model is not on disk — download with `memory-hub model download {}`",
                    entry.id
                );
            }
            Code::Success
        }
        Output::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "active_model": entry.id,
                    "on_disk": on_disk,
                    "hint": if on_disk { None } else { Some(format!("memory-hub model download {}", entry.id)) },
                })
            );
            Code::Success
        }
    }
}

pub(crate) fn benchmark(id: &str, output: Output) -> Code {
    #![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let Some(entry) = find_model(id) else {
        return model_not_found(id, output);
    };

    let opts = DownloadOpts::default();
    match verify_model_sync(entry, &opts) {
        Ok(ModelVerification::Present { path, .. }) => {
            let provider = LlamaCppProvider::new(entry, path);
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(error) => {
                    return internal_error(
                        &format!("failed to start async runtime: {error}"),
                        output,
                    );
                }
            };

            if let Err(error) = runtime.block_on(provider.warm_up()) {
                let msg = format!("failed to load model: {error}");
                match output {
                    Output::Human => eprintln!("memory-hub: {msg}"),
                    Output::Json => {
                        let _ = print_error_json(&msg);
                    }
                }
                return Code::Internal;
            }

            let mut results = Vec::new();
            for &batch_size in BATCH_SIZES {
                for &token_len in TOKEN_LENGTHS {
                    let texts = generate_benchmark_texts(batch_size, token_len);
                    let start = Instant::now();
                    let embed_result = runtime.block_on(provider.embed(&texts));
                    let elapsed = start.elapsed();
                    let (throughput, error) = match embed_result {
                        Ok(vectors) => {
                            if elapsed.as_secs_f64() > 0.0 {
                                (vectors.len() as f64 / elapsed.as_secs_f64(), None)
                            } else {
                                (0.0, None)
                            }
                        }
                        Err(e) => (0.0, Some(format!("{e}"))),
                    };
                    results.push(BenchmarkResult {
                        batch_size,
                        token_length: token_len,
                        elapsed_ms: elapsed.as_millis() as u64,
                        embeddings_per_sec: throughput,
                        error,
                    });
                }
            }

            let below_floor = results
                .iter()
                .any(|r| r.embeddings_per_sec < BENCHMARK_FLOOR);

            match output {
                Output::Human => {
                    let _ = print_benchmark_human(entry, &results, below_floor);
                    Code::Success
                }
                Output::Json => {
                    let _ = print_benchmark_json(entry, &results, below_floor);
                    Code::Success
                }
            }
        }
        Ok(ModelVerification::Missing) => {
            let msg = format!(
                "model '{}' is not on disk — download with `memory-hub model download {}`",
                entry.id, entry.id
            );
            match output {
                Output::Human => eprintln!("memory-hub: {msg}"),
                Output::Json => {
                    let _ = print_error_json(&msg);
                }
            }
            Code::DoctorFailed
        }
        Ok(ModelVerification::Broken { .. }) => {
            let msg = format!(
                "model '{}' is on disk but its hash does not match — re-download with `memory-hub model download {}`",
                entry.id, entry.id
            );
            match output {
                Output::Human => eprintln!("memory-hub: {msg}"),
                Output::Json => {
                    let _ = print_error_json(&msg);
                }
            }
            Code::DoctorFailed
        }
        Err(error) => internal_error(&format!("failed to verify model: {error}"), output),
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Output {
    Human,
    Json,
}

impl From<crate::cli::Output> for Output {
    fn from(output: crate::cli::Output) -> Self {
        match output {
            crate::cli::Output::Human => Self::Human,
            crate::cli::Output::Json => Self::Json,
        }
    }
}

#[derive(Debug, Serialize)]
struct ListRow {
    id: String,
    display_name: String,
    languages: String,
    dimensions: usize,
    size_bytes: u64,
    on_disk: bool,
    active: bool,
    configured: bool,
}

#[derive(Debug, Serialize)]
struct ShowDetail {
    id: String,
    display_name: String,
    description: String,
    languages: String,
    dimensions: usize,
    max_tokens: usize,
    quantisation: String,
    pooling: String,
    query_prefix: Option<String>,
    doc_prefix: Option<String>,
    size_bytes: u64,
    url: String,
    sha256: String,
    cached_path: Option<String>,
    on_disk: bool,
    backend: String,
    active: bool,
}

#[derive(Debug, Serialize)]
struct BenchmarkResult {
    batch_size: usize,
    token_length: usize,
    elapsed_ms: u64,
    embeddings_per_sec: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pooling_label(p: Pooling) -> String {
    match p {
        Pooling::Mean => "mean".to_owned(),
        Pooling::Cls => "cls".to_owned(),
        Pooling::LastToken => "last_token".to_owned(),
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn model_not_found(id: &str, output: Output) -> Code {
    let available: Vec<&str> = all_models().iter().map(|m| m.id).collect();
    let msg = format!(
        "model '{id}' not found; available: {}",
        available.join(", ")
    );
    match output {
        Output::Human => eprintln!("memory-hub: {msg}"),
        Output::Json => {
            let _ = print_error_json(&msg);
        }
    }
    Code::Usage
}

fn internal_error(msg: &str, output: Output) -> Code {
    match output {
        Output::Human => eprintln!("memory-hub: {msg}"),
        Output::Json => {
            let _ = print_error_json(msg);
        }
    }
    Code::Internal
}

/// Generate benchmark texts targeting an approximate token count.
///
/// The `target_tokens` label is approximate — actual tokenisation depends on
/// the model's tokenizer. We use ~4 chars per token as a rough estimate and
/// surface this as "approx" in the output.
fn generate_benchmark_texts(count: usize, target_tokens: usize) -> Vec<String> {
    let chars_needed = target_tokens.saturating_mul(4);
    let word = "benchmark ";
    let words_needed = chars_needed.div_ceil(word.len());
    let text: String = word.repeat(words_needed);
    (0..count).map(|_| text.clone()).collect()
}

// ---------------------------------------------------------------------------
// Renderers
// ---------------------------------------------------------------------------

fn print_download_human(entry: &ModelEntry, outcome: &EnsureOutcome) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match outcome {
        EnsureOutcome::Cached { path, sha256 } => {
            writeln!(
                out,
                "Model {} is already cached: {}",
                entry.id,
                path.display()
            )?;
            writeln!(out, "  SHA-256: {sha256}")?;
        }
        EnsureOutcome::Downloaded {
            path,
            sha256,
            verified,
        } => {
            writeln!(
                out,
                "Downloaded {} ({}) to: {}",
                entry.id,
                format_size(entry.size_bytes),
                path.display()
            )?;
            writeln!(out, "  SHA-256: {sha256}")?;
            if !verified {
                writeln!(
                    out,
                    "  Note: SHA-256 is a placeholder — paste this digest into the registry."
                )?;
            }
        }
    }
    Ok(())
}

fn print_download_json(entry: &ModelEntry, outcome: &EnsureOutcome) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let verified = match outcome {
        EnsureOutcome::Downloaded { verified, .. } => *verified,
        EnsureOutcome::Cached { .. } => entry.sha256 != PLACEHOLDER_SHA256,
    };
    let value = serde_json::json!({
        "model_id": entry.id,
        "path": outcome.path().display().to_string(),
        "sha256": outcome.sha256(),
        "verified": verified,
    });
    writeln!(out, "{value}")
}

fn print_list_human(rows: &[ListRow]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "{:<26} {:<30} {:<24} {:>5} {:>10} {:>6} {:>6}",
        "ID", "Name", "Languages", "Dims", "Size", "Disk", "Active"
    )?;
    writeln!(out, "{}", "-".repeat(110))?;
    for row in rows {
        writeln!(
            out,
            "{:<26} {:<30} {:<24} {:>5} {:>10} {:>6} {:>6}",
            row.id,
            row.display_name,
            row.languages,
            row.dimensions,
            format_size(row.size_bytes),
            if row.on_disk { "yes" } else { "no" },
            if row.active { "yes" } else { "no" },
        )?;
    }
    Ok(())
}

fn print_list_json(rows: &[ListRow]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, rows).map_err(io::Error::other)?;
    writeln!(out)
}

fn print_show_human(detail: &ShowDetail) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "Model: {} ({})", detail.id, detail.display_name)?;
    writeln!(out, "  Description:    {}", detail.description)?;
    writeln!(out, "  Languages:      {}", detail.languages)?;
    writeln!(out, "  Dimensions:     {}", detail.dimensions)?;
    writeln!(out, "  Max tokens:     {}", detail.max_tokens)?;
    writeln!(out, "  Quantisation:   {}", detail.quantisation)?;
    writeln!(out, "  Pooling:        {}", detail.pooling)?;
    if let Some(prefix) = &detail.query_prefix {
        writeln!(out, "  Query prefix:   {prefix:?}")?;
    } else {
        writeln!(out, "  Query prefix:   (none)")?;
    }
    if let Some(prefix) = &detail.doc_prefix {
        writeln!(out, "  Doc prefix:     {prefix:?}")?;
    } else {
        writeln!(out, "  Doc prefix:     (none)")?;
    }
    writeln!(out, "  Size:           {}", format_size(detail.size_bytes))?;
    writeln!(out, "  URL:            {}", detail.url)?;
    writeln!(out, "  SHA-256:        {}", detail.sha256)?;
    if let Some(path) = &detail.cached_path {
        writeln!(out, "  Cached path:    {path}")?;
    } else {
        writeln!(out, "  Cached path:    (not downloaded)")?;
    }
    writeln!(
        out,
        "  On disk:        {}",
        if detail.on_disk { "yes" } else { "no" }
    )?;
    writeln!(out, "  Backend:        {}", detail.backend)?;
    writeln!(
        out,
        "  Active:         {}",
        if detail.active { "yes" } else { "no" }
    )?;
    Ok(())
}

fn print_show_json(detail: &ShowDetail) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, detail).map_err(io::Error::other)?;
    writeln!(out)
}

fn print_benchmark_human(
    entry: &ModelEntry,
    results: &[BenchmarkResult],
    below_floor: bool,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(
        out,
        "Benchmark: {} ({})  backend: {}  dimensions: {}",
        entry.id,
        entry.display_name,
        backend_name(),
        entry.dimensions
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "{:>6} {:>14} {:>12} {:>20}",
        "Batch", "Tokens (approx)", "Time (ms)", "Embeddings/sec"
    )?;
    writeln!(out, "{}", "-".repeat(56))?;
    for r in results {
        if let Some(err) = &r.error {
            writeln!(
                out,
                "{:>6} {:>14} {:>12} {:>20}  ERROR: {err}",
                r.batch_size, r.token_length, r.elapsed_ms, r.embeddings_per_sec
            )?;
        } else {
            writeln!(
                out,
                "{:>6} {:>14} {:>12} {:>20.2}",
                r.batch_size, r.token_length, r.elapsed_ms, r.embeddings_per_sec
            )?;
        }
    }
    if below_floor {
        writeln!(out)?;
        writeln!(
            out,
            "Warning: some results are below {BENCHMARK_FLOOR:.0} embeddings/sec — \
             search performance may be degraded."
        )?;
    }
    Ok(())
}

fn print_benchmark_json(
    entry: &ModelEntry,
    results: &[BenchmarkResult],
    below_floor: bool,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let value = serde_json::json!({
        "model_id": entry.id,
        "backend": backend_name(),
        "dimensions": entry.dimensions,
        "results": results,
        "below_floor": below_floor,
        "floor": BENCHMARK_FLOOR,
    });
    writeln!(out, "{value}")
}

fn print_error_json(msg: &str) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let value = serde_json::json!({"error": msg});
    writeln!(out, "{value}")
}

/// Public entry point for the doctor check — inspects the active model.
pub(crate) fn doctor_check() -> DoctorModelCheck {
    let active = config::resolve_active_model();
    let opts = DownloadOpts::default();
    match verify_model_sync(active, &opts) {
        Ok(ModelVerification::Present { .. }) => DoctorModelCheck {
            model_id: active.id.to_owned(),
            status: DoctorModelStatus::Ok,
            message: format!("model '{}' is present and verified", active.id),
        },
        Ok(ModelVerification::Missing) => DoctorModelCheck {
            model_id: active.id.to_owned(),
            status: DoctorModelStatus::Missing,
            message: format!(
                "model '{}' is not on disk — run `memory-hub model download {}`",
                active.id, active.id
            ),
        },
        Ok(ModelVerification::Broken { .. }) => DoctorModelCheck {
            model_id: active.id.to_owned(),
            status: DoctorModelStatus::Broken,
            message: format!(
                "model '{}' is on disk but hash does not match — re-download with `memory-hub model download {}`",
                active.id, active.id
            ),
        },
        Err(error) => DoctorModelCheck {
            model_id: active.id.to_owned(),
            status: DoctorModelStatus::Error,
            message: format!("failed to verify model: {error}"),
        },
    }
}

#[derive(Debug)]
pub(crate) struct DoctorModelCheck {
    pub model_id: String,
    pub status: DoctorModelStatus,
    pub message: String,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DoctorModelStatus {
    Ok,
    Missing,
    Broken,
    Error,
}

impl DoctorModelStatus {
    pub(crate) fn kind(&self) -> Option<&'static str> {
        match self {
            Self::Ok => None,
            Self::Missing => Some("model_missing"),
            Self::Broken => Some("model_broken"),
            Self::Error => Some("model_check_error"),
        }
    }
}

/// `memory-hub setup` — first-run wizard.
///
/// 1. Checks if binary is accessible
/// 2. Shows available models with the platform default highlighted
/// 3. Downloads the selected model with progress bar
/// 4. Sets it as active in config
/// 5. Runs doctor for final verification
#[allow(clippy::too_many_lines)] // Linear first-run wizard: one screen of prose per step.
pub(crate) fn setup(output: Output) -> Code {
    let active = config::resolve_active_model();
    let platform_default = memory_hub_embed::platform_default_model();
    let opts = DownloadOpts::default();

    // Collect model info for display
    let models: Vec<(&ModelEntry, bool)> = all_models()
        .iter()
        .map(|entry| {
            let on_disk = matches!(
                verify_model_sync(*entry, &opts),
                Ok(ModelVerification::Present { .. })
            );
            (*entry, on_disk)
        })
        .collect();

    match output {
        Output::Json => {
            let models_json: Vec<serde_json::Value> = models
                .iter()
                .map(|(entry, on_disk)| {
                    serde_json::json!({
                        "id": entry.id,
                        "display_name": entry.display_name,
                        "dimensions": entry.dimensions,
                        "size_bytes": entry.size_bytes,
                        "on_disk": on_disk,
                        "is_platform_default": entry.id == platform_default.id,
                        "is_active": entry.id == active.id,
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "setup": true,
                    "platform_default": platform_default.id,
                    "active_model": active.id,
                    "models": models_json,
                })
            );
            // In JSON mode, download the platform default if not on disk.
            if skip_model_check(&models, platform_default.id) {
                println!(
                    "{}",
                    serde_json::json!({"setup_complete": true, "model": platform_default.id, "already_on_disk": true})
                );
            } else {
                run_setup_download(platform_default.id, output);
            }
        }
        Output::Human => {
            println!("Memory Hub Setup Wizard");
            println!("=======================");
            println!();

            // Check if binary is accessible
            println!("1. Checking installation...");
            let exe = std::env::current_exe();
            match &exe {
                Ok(path) => {
                    println!("   Binary: {}", path.display());
                }
                Err(error) => {
                    println!("   Warning: could not determine binary path: {error}");
                }
            }
            println!();

            // Show models
            println!("2. Available models:");
            for (entry, on_disk) in &models {
                let default_marker = if entry.id == platform_default.id {
                    " (platform default)"
                } else {
                    ""
                };
                let active_marker = if entry.id == active.id {
                    " [active]"
                } else {
                    ""
                };
                let disk_marker = if *on_disk { " [on disk]" } else { "" };
                println!(
                    "   - {}{}{}{}",
                    entry.display_name, default_marker, active_marker, disk_marker
                );
                println!(
                    "     id: {}, dimensions: {}, size: {:.1} MB",
                    entry.id,
                    entry.dimensions,
                    megabytes(entry.size_bytes)
                );
            }
            println!();

            // Select model — use platform default, or the first one on disk.
            let selected_id = if active.id != platform_default.id
                || !models
                    .iter()
                    .any(|(e, on_disk)| e.id == active.id && *on_disk)
            {
                platform_default.id.to_owned()
            } else {
                active.id.to_owned()
            };

            // Check if already on disk
            let already_downloaded = models
                .iter()
                .any(|(entry, on_disk)| entry.id == selected_id && *on_disk);

            if already_downloaded {
                println!("3. Model '{selected_id}' is already on disk.");
                if let Err(error) = config::set_active_model(&selected_id) {
                    eprintln!("memory-hub: failed to set active model: {error}");
                    return Code::Internal;
                }
                println!("   Set as active model.");
            } else {
                println!("3. Downloading model '{selected_id}'...");
                let result = download(&selected_id, output);
                if matches!(result, Code::Success) {
                    if let Err(error) = config::set_active_model(&selected_id) {
                        eprintln!("memory-hub: failed to set active model: {error}");
                        return Code::Internal;
                    }
                    println!("   Model '{selected_id}' set as active.");
                } else {
                    println!("   Model download failed. MCP will run in FTS-only mode.");
                    println!(
                        "   You can retry later with: memory-hub model download {selected_id}"
                    );
                }
            }
            println!();

            // Run doctor
            println!("4. Running doctor check...");
            let report = crate::doctor::inspect(None);
            if report.is_healthy() {
                println!("   All checks passed.");
            } else {
                println!("   Some checks failed — see doctor output above.");
            }

            println!();
            println!("Setup complete!");
            println!("Run 'memory-hub mcp' to start the MCP server.");
        }
    }
    Code::Success
}

/// Check if the platform default model is already on disk.
fn skip_model_check(models: &[(&ModelEntry, bool)], model_id: &str) -> bool {
    models
        .iter()
        .any(|(entry, on_disk)| entry.id == model_id && *on_disk)
}

/// Download and set a model as active in JSON mode.
fn run_setup_download(model_id: &str, output: Output) {
    let result = download(model_id, output);
    if matches!(result, Code::Success) {
        let _ = config::set_active_model(model_id);
        println!(
            "{}",
            serde_json::json!({"setup_complete": true, "model": model_id})
        );
    } else {
        println!(
            "{}",
            serde_json::json!({"setup_complete": false, "model": model_id, "error": "download failed"})
        );
    }
}

/// Check if any model is on disk. Used by the MCP server to print a first-run
/// hint when no model is available.
///
/// Existence only: verification hashes every GGUF in the cache — around half a
/// gigabyte per model — which used to run on every `memory-hub mcp` start and
/// dominated session start-up. `doctor` and `model list` still verify, because
/// there the cost buys an answer the user asked for.
#[must_use]
pub(crate) fn any_model_on_disk() -> bool {
    let opts = DownloadOpts::default();
    all_models().iter().any(|entry| {
        memory_hub_embed::cached_model_path(*entry, &opts).is_ok_and(|path| path.is_file())
    })
}
