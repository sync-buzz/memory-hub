use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use git_memory_index::Projection;
use git_memory_reconcile::{DivergenceMode, Reconciler};
use git_memory_store::{
    check_push_policy, fetch_and_merge, push_to_remote, read_remote_config, remove_remote_config,
    write_remote_config, GitStore, MemoryRemote, StoreErrorKind,
};

use crate::doctor;
use crate::exit::Code;
use crate::model;

#[derive(Debug, Parser)]
#[command(
    name = "git-memory",
    bin_name = "git memory",
    version,
    about = "Standalone, Git-backed project memory",
    long_about = None,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the public Git Memory MCP interface over standard input/output.
    Mcp {
        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },

    /// Check whether Git Memory can operate in a repository.
    Doctor {
        /// Repository or a path inside it. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },

    /// Reconcile code commits with Memory freshness and checkpoints.
    Reconcile {
        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Explicitly recover when code history diverged after rebase/reset.
        #[arg(long)]
        full_rebuild: bool,

        /// Embed records with the configured model for vector-rescue search.
        /// Degrades to FTS-only with a warning when no model is downloaded.
        #[arg(long)]
        embed: bool,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },

    /// Manage embedding models: download, list, show, use, benchmark.
    Model {
        #[command(subcommand)]
        subcommand: ModelCommand,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human, global = true)]
        output: Output,
    },

    /// Manage the memory remote (separate from code origin).
    Remote {
        #[command(subcommand)]
        subcommand: RemoteCommand,

        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH", global = true)]
        project: Option<PathBuf>,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human, global = true)]
        output: Output,
    },

    /// Fetch from the configured memory remote and merge.
    Fetch {
        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },

    /// Push memory refs to the configured remote.
    Push {
        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Force-push (overwrite remote history). Use with caution.
        #[arg(long)]
        force: bool,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// Download a GGUF model to the local cache with SHA-256 verification.
    Download {
        /// Model id (e.g. `bge-m3`, `nomic-embed-text-v1.5`).
        id: String,
    },

    /// List all models in the registry with on-disk and active status.
    List,

    /// Show detailed metadata for a model.
    Show {
        /// Model id.
        id: String,
    },

    /// Set the active model in config. Does not download — prints a hint if
    /// the file is not on disk.
    Use {
        /// Model id.
        id: String,
    },

    /// Benchmark model throughput across a batch × token-length grid.
    Benchmark {
        /// Model id. Requires the model to be on disk.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum RemoteCommand {
    /// Add or replace the memory remote URL.
    Add {
        /// Remote URL (SSH, HTTPS, or local path).
        url: String,

        /// Optional custom refspec.
        #[arg(long)]
        refspec: Option<String>,
    },

    /// Show the configured memory remote.
    List,

    /// Remove the memory remote configuration.
    Remove,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum Output {
    #[default]
    Human,
    Json,
}

#[allow(clippy::too_many_lines)]
pub(crate) fn run<I, T>(args: I) -> Code
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => {
            let code = if error.use_stderr() {
                Code::Usage
            } else {
                Code::Success
            };
            if let Err(write_error) = error.print() {
                eprintln!("git-memory: unable to write command output: {write_error}");
                return Code::Internal;
            }
            return code;
        }
    };

    match cli.command {
        Command::Mcp { project } => {
            let project = match project.map_or_else(std::env::current_dir, Ok) {
                Ok(project) => project,
                Err(error) => {
                    eprintln!("git-memory: unable to resolve current directory: {error}");
                    return Code::Internal;
                }
            };
            if let Err(error) = git_memory_mcp::serve(&project) {
                eprintln!("git-memory: MCP server failed: {error}");
                return Code::Internal;
            }
            Code::Success
        }
        Command::Doctor { project, output } => {
            let report = doctor::inspect(project.as_deref());
            let render_result = match output {
                Output::Human => doctor::render_human(&report),
                Output::Json => doctor::render_json(&report),
            };

            if let Err(error) = render_result {
                eprintln!("git-memory: unable to write doctor report: {error}");
                return Code::Internal;
            }

            if report.is_healthy() {
                Code::Success
            } else {
                Code::DoctorFailed
            }
        }
        Command::Reconcile {
            project,
            full_rebuild,
            embed,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("git-memory: unable to resolve current directory");
                return Code::Internal;
            };
            let mode = if full_rebuild {
                DivergenceMode::FullRebuild
            } else {
                DivergenceMode::Report
            };
            match Reconciler::open(&project).and_then(|reconciler| reconciler.reconcile(mode)) {
                Ok(report) => {
                    let provider = if embed {
                        resolve_embed_provider()
                    } else {
                        None
                    };
                    let index_result = GitStore::open(&project)
                        .map_err(|error| error.to_string())
                        .and_then(|store| {
                            Projection::synchronize_store_with(&store, provider)
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        });
                    if let Err(error) = index_result {
                        eprintln!("git-memory: index synchronization failed: {error}");
                        return Code::Internal;
                    }
                    let rendered = match output {
                        Output::Json => serde_json::to_string(&report),
                        Output::Human => Ok(format!(
                            "Git Memory reconciled {} code commit(s); HEAD {}",
                            report.processed.len(),
                            report.head.as_deref().unwrap_or("unborn")
                        )),
                    };
                    match rendered {
                        Ok(rendered) => println!("{rendered}"),
                        Err(error) => {
                            eprintln!("git-memory: unable to render reconcile report: {error}");
                            return Code::Internal;
                        }
                    }
                    Code::Success
                }
                Err(error) => {
                    eprintln!("git-memory: reconcile failed: {error}");
                    Code::DoctorFailed
                }
            }
        }
        Command::Model { subcommand, output } => {
            let output = model::Output::from(output);
            match subcommand {
                ModelCommand::Download { id } => model::download(&id, output),
                ModelCommand::List => model::list(output),
                ModelCommand::Show { id } => model::show(&id, output),
                ModelCommand::Use { id } => model::use_model(&id, output),
                ModelCommand::Benchmark { id } => model::benchmark(&id, output),
            }
        }
        Command::Remote {
            subcommand,
            project,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("git-memory: unable to resolve current directory");
                return Code::Internal;
            };
            let git_dir = match GitStore::discover_git_dir(&project) {
                Ok(git_dir) => git_dir,
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            match subcommand {
                RemoteCommand::Add { url, refspec } => {
                    let remote = MemoryRemote { url, refspec };
                    if let Err(error) = write_remote_config(&git_dir, &remote) {
                        eprintln!("git-memory: {error}");
                        return Code::Internal;
                    }
                    match output {
                        Output::Json => println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "url": remote.url,
                                "refspec": remote.refspec
                            }))
                            .unwrap_or_else(|_| "{}".into())
                        ),
                        Output::Human => println!("Memory remote set to {}", remote.url),
                    }
                    Code::Success
                }
                RemoteCommand::List => match read_remote_config(&git_dir) {
                    Ok(Some(remote)) => {
                        match output {
                            Output::Json => println!(
                                "{}",
                                serde_json::to_string(&remote).unwrap_or_else(|_| "{}".into())
                            ),
                            Output::Human => {
                                println!("url: {}", remote.url);
                                if let Some(refspec) = &remote.refspec {
                                    println!("refspec: {refspec}");
                                }
                            }
                        }
                        Code::Success
                    }
                    Ok(None) => {
                        match output {
                            Output::Json => println!("{{}}"),
                            Output::Human => println!("No memory remote configured."),
                        }
                        Code::Success
                    }
                    Err(error) => {
                        eprintln!("git-memory: {error}");
                        Code::Internal
                    }
                },
                RemoteCommand::Remove => {
                    if let Err(error) = remove_remote_config(&git_dir) {
                        eprintln!("git-memory: {error}");
                        return Code::Internal;
                    }
                    match output {
                        Output::Json => println!("{}", serde_json::json!({"removed": true})),
                        Output::Human => println!("Memory remote removed."),
                    }
                    Code::Success
                }
            }
        }
        Command::Fetch { project, output } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("git-memory: unable to resolve current directory");
                return Code::Internal;
            };
            let store = match GitStore::open(&project) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            let git_dir = store.git_dir().to_path_buf();
            let remote = match read_remote_config(&git_dir) {
                Ok(Some(remote)) => remote,
                Ok(None) => {
                    eprintln!("git-memory: no memory remote configured — run `git memory remote add <url>`");
                    return Code::Usage;
                }
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            match fetch_and_merge(&store, &remote, &[]) {
                Ok(result) => {
                    match output {
                        Output::Json => println!(
                            "{}",
                            serde_json::to_string(&result).unwrap_or_else(|_| "{}".into())
                        ),
                        Output::Human => {
                            if result.fast_forward && result.merged {
                                println!("Already up to date.");
                            } else if result.fast_forward {
                                println!(
                                    "Fast-forwarded {} → {}",
                                    result.local_revision_before.as_str(),
                                    result.local_revision_after.as_str()
                                );
                            } else if result.merged {
                                println!(
                                    "Merged remote {} into local {}",
                                    result.remote_revision.as_str(),
                                    result.local_revision_after.as_str()
                                );
                            } else if !result.conflicts.is_empty() {
                                println!("Merge conflicts on {} key(s):", result.conflicts.len());
                                for conflict in &result.conflicts {
                                    println!(
                                        "  {} (local: {}, remote: {})",
                                        conflict.key,
                                        conflict.local_content_hash,
                                        conflict.remote_content_hash
                                    );
                                }
                                println!("Resolve conflicts and retry.");
                            }
                        }
                    }
                    if result.conflicts.is_empty() {
                        Code::Success
                    } else {
                        Code::NonFastForward
                    }
                }
                Err(error) => {
                    let code = store_error_to_code(error.kind);
                    eprintln!("git-memory: {error}");
                    code
                }
            }
        }
        Command::Push {
            project,
            force,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("git-memory: unable to resolve current directory");
                return Code::Internal;
            };
            let git_dir = match GitStore::discover_git_dir(&project) {
                Ok(git_dir) => git_dir,
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            let remote = match read_remote_config(&git_dir) {
                Ok(Some(remote)) => remote,
                Ok(None) => {
                    eprintln!("git-memory: no memory remote configured — run `git memory remote add <url>`");
                    return Code::Usage;
                }
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            // Apply push policy: check for stale records before network mutation.
            let store = match GitStore::open(&project) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            let policy_result = match check_push_policy(&store) {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("git-memory: {error}");
                    return Code::Internal;
                }
            };
            for warning in &policy_result.warnings {
                eprintln!("git-memory: warning: {warning}");
            }
            if !policy_result.allowed {
                eprintln!(
                    "git-memory: push blocked by memory_push_stale policy ({} stale records)",
                    policy_result.stale_count
                );
                return Code::NonFastForward;
            }
            match push_to_remote(&git_dir, &remote, force) {
                Ok(()) => {
                    match output {
                        Output::Json => {
                            println!("{}", serde_json::json!({"pushed": true, "force": force}));
                        }
                        Output::Human => {
                            if force {
                                println!("Force-pushed memory refs to {}", remote.url);
                            } else {
                                println!("Pushed memory refs to {}", remote.url);
                            }
                        }
                    }
                    Code::Success
                }
                Err(error) => {
                    let code = store_error_to_code(error.kind);
                    eprintln!("git-memory: {error}");
                    code
                }
            }
        }
    }
}

fn store_error_to_code(kind: StoreErrorKind) -> Code {
    match kind {
        StoreErrorKind::TransportFailed
        | StoreErrorKind::SignatureInvalid
        | StoreErrorKind::NamespaceRejected => Code::TransportFailed,
        StoreErrorKind::FastForwardRequired
        | StoreErrorKind::Diverged
        | StoreErrorKind::MergeConflict => Code::NonFastForward,
        StoreErrorKind::AuthenticationFailed => Code::AuthFailed,
        _ => Code::Internal,
    }
}

fn absolute_project(project: Option<PathBuf>) -> Option<PathBuf> {
    match project {
        Some(project) if project.is_absolute() => Some(project),
        Some(project) => std::env::current_dir().ok().map(|cwd| cwd.join(project)),
        None => std::env::current_dir().ok(),
    }
}
/// Resolve an embedding provider from configuration when `--embed` is passed.
/// Returns `None` and prints a warning when no model is available — the
/// projection then degrades to FTS-only.
fn resolve_embed_provider() -> Option<std::sync::Arc<dyn git_memory_embed::EmbeddingProvider>> {
    use std::sync::Arc;
    let entry = crate::config::resolve_active_model();
    let opts = git_memory_embed::DownloadOpts::default();
    match git_memory_embed::verify_model_sync(entry, &opts) {
        Ok(git_memory_embed::ModelVerification::Present { path, .. }) => Some(Arc::new(
            git_memory_embed::LlamaCppProvider::new(entry, path),
        )),
        _ => {
            eprintln!(
                "git-memory: warning: embedding model `{}` is not available — \
                 vector search will degrade to FTS-only",
                entry.id
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::exit::Code;

    #[test]
    fn clap_help_and_version_are_successful() {
        assert_eq!(run(["git-memory", "--help"]), Code::Success);
        assert_eq!(run(["git-memory", "--version"]), Code::Success);
    }

    #[test]
    fn invalid_arguments_have_the_stable_usage_code() {
        assert_eq!(run(["git-memory", "unknown"]), Code::Usage);
    }
}
