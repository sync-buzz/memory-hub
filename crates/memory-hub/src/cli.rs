use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use memory_hub_index::Projection;
use memory_hub_reconcile::DivergenceMode;
use memory_hub_store::{
    GitStore, MemoryRemote, StoreErrorKind, check_push_policy, fetch_and_merge, push_to_remote,
    read_remote_config, remove_remote_config, write_remote_config,
};

use crate::doctor;
use crate::exit::Code;
use crate::model;

#[derive(Debug, Parser)]
#[command(
    name = "memory-hub",
    bin_name = "memory-hub",
    version,
    about = "Standalone project memory: records in Git objects or in plain files",
    long_about = None,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Where a new project keeps its records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RecordsIn {
    /// A folder of files beside the project.
    Folder,
    /// Git objects under private refs.
    Refs,
}

/// What a declared storage is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum StorageSort {
    /// A directory of the working tree, holding documents people edit.
    RepoFolder,
    /// A directory of record files.
    Folder,
    /// Git objects under private refs.
    Refs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the public Memory Hub MCP interface over standard input/output.
    Mcp {
        /// Repository or Git directory. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,
    },

    /// Declare where this project keeps its memory.
    Init {
        /// Repository or a path inside it. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Where records live. Defaults to a folder beside the project, which
        /// needs no Git and can be read with any editor. Choose `refs` to keep
        /// them in Git objects instead: versioned, pushable, and invisible in
        /// the working tree.
        #[arg(long, value_enum, default_value_t = RecordsIn::Folder)]
        records: RecordsIn,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },

    /// Declare another storage this project can keep content in.
    DeclareStorage {
        /// The name a type will use to point at it.
        name: String,

        /// What it is.
        #[arg(long, value_enum)]
        kind: StorageSort,

        /// Where it is, relative to the project. Required by every kind that
        /// has a location.
        #[arg(long, value_name = "PATH")]
        path: Option<PathBuf>,

        /// Repository or a path inside it. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },

    /// Check whether Memory Hub can operate in a repository.
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

    /// First-run setup wizard: choose and download an embedding model.
    Setup {
        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
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

    /// Manage the installation registry (consumers, repositories, uninstall).
    Registry {
        #[command(subcommand)]
        subcommand: RegistryCommand,

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human, global = true)]
        output: Output,
    },

    /// Uninstall memory-hub: list consumers and repositories, then remove
    /// the installation. Canonical data (refs, config, models) is preserved
    /// by default — use --purge to remove everything.
    Uninstall {
        /// Remove all memory-hub data (config, models, registry).
        /// Without this flag, only the binary is removed from the registry.
        #[arg(long)]
        purge: bool,

        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,

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

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// Show the installation registry (consumers, repositories, installation).
    List,

    /// Register a consumer (e.g. "sync") with a required major version.
    RegisterConsumer {
        /// Consumer name (e.g. "sync", "custom-client").
        name: String,
        /// Required memory interface major version.
        major: u16,
    },

    /// Unregister a consumer. Does not delete memory-hub or any data.
    UnregisterConsumer {
        /// Consumer name to remove.
        name: String,
    },

    /// Register a repository path (for uninstall warnings).
    AddRepository {
        /// Absolute path to a project repository.
        path: PathBuf,
    },

    /// Remove a repository from the registry.
    RemoveRepository {
        /// Absolute path to remove.
        path: PathBuf,
    },
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
                eprintln!("memory-hub: unable to write command output: {write_error}");
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
                    eprintln!("memory-hub: unable to resolve current directory: {error}");
                    return Code::Internal;
                }
            };
            // First-run hint: if no model is on disk, suggest running setup.
            if !model::any_model_on_disk() {
                eprintln!("memory-hub: no embedding model found — MCP will run in FTS-only mode.");
                eprintln!(
                    "  Run 'memory-hub setup' to download a model, or 'memory-hub model list' to see options."
                );
                eprintln!();
            }
            if let Err(error) = memory_hub_mcp::serve(&project) {
                eprintln!("memory-hub: MCP server failed: {error}");
                return Code::Internal;
            }
            Code::Success
        }
        Command::Init {
            project,
            records,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let storages = std::collections::BTreeMap::from([(
                "main".to_owned(),
                match records {
                    RecordsIn::Folder => memory_hub_service::StorageConfig::folder(
                        memory_hub_service::DEFAULT_RECORDS_PATH,
                    ),
                    RecordsIn::Refs => memory_hub_service::StorageConfig::refs(),
                },
            )]);
            match memory_hub_service::MemoryService::init(&project, storages) {
                Ok(config) => {
                    let (name, storage) = match config.record_storage() {
                        Ok(pair) => pair,
                        Err(error) => {
                            eprintln!("memory-hub: {}", error.message);
                            return Code::Internal;
                        }
                    };
                    match output {
                        Output::Json => println!(
                            "{}",
                            serde_json::json!({
                                "initialised": true,
                                "records_storage": name,
                                "kind": storage.kind,
                                "path": storage.path,
                            })
                        ),
                        Output::Human => println!(
                            "Memory initialised. Records live in `{name}`{}.",
                            storage
                                .path
                                .as_ref()
                                .map_or_else(String::new, |path| format!(" at `{path}`"))
                        ),
                    }
                    Code::Success
                }
                Err(error) => {
                    eprintln!("memory-hub: {}", error.message);
                    Code::Internal
                }
            }
        }
        Command::DeclareStorage {
            name,
            kind,
            path,
            project,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let path = path.map(|path| path.to_string_lossy().into_owned());
            let storage = match (kind, path) {
                (StorageSort::Refs, _) => memory_hub_service::StorageConfig::refs(),
                (StorageSort::RepoFolder, Some(path)) => {
                    memory_hub_service::StorageConfig::repo_folder(path)
                }
                (StorageSort::Folder, Some(path)) => {
                    memory_hub_service::StorageConfig::folder(path)
                }
                (_, None) => {
                    eprintln!("memory-hub: this kind of storage must say where it is (--path)");
                    return Code::Usage;
                }
            };
            let mut service = memory_hub_service::MemoryService::open(project);
            match service.declare_storage(&name, storage) {
                Ok(config) => {
                    match output {
                        Output::Json => println!(
                            "{}",
                            serde_json::json!({
                                "declared": config.storages.keys().collect::<Vec<_>>(),
                            })
                        ),
                        Output::Human => println!(
                            "Storage `{name}` declared. This project now has: {}.",
                            config
                                .storages
                                .keys()
                                .map(String::as_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    }
                    Code::Success
                }
                Err(error) => {
                    eprintln!("memory-hub: {}", error.message);
                    Code::Internal
                }
            }
        }
        Command::Doctor { project, output } => {
            let report = doctor::inspect(project.as_deref());
            let render_result = match output {
                Output::Human => doctor::render_human(&report),
                Output::Json => doctor::render_json(&report),
            };

            if let Err(error) = render_result {
                eprintln!("memory-hub: unable to write doctor report: {error}");
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
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let mode = if full_rebuild {
                DivergenceMode::FullRebuild
            } else {
                DivergenceMode::Report
            };
            // Through the service rather than a `Reconciler` assembled here:
            // the policy that decides what a record may say needs the
            // project's storage declaration, and the service is what holds it.
            // Built by hand, this reconcile validated records against rules
            // that could not see where storages are.
            let service = memory_hub_service::MemoryService::open(project.clone());
            match service.reconcile(mode) {
                Ok(report) => {
                    let provider = if embed {
                        resolve_embed_provider()
                    } else {
                        None
                    };
                    // The reader for content that lives outside the records
                    // comes from the service: a projection built without it
                    // would index every attached document as an empty body and
                    // then look fresh to the next search.
                    let content = service.content_resolver();
                    let index_result = GitStore::open(&project)
                        .map_err(|error| error.to_string())
                        .and_then(|store| {
                            Projection::synchronize_store_with(&store, provider, Some(content))
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                        });
                    if let Err(error) = index_result {
                        eprintln!("memory-hub: index synchronization failed: {error}");
                        return Code::Internal;
                    }
                    let rendered = match output {
                        Output::Json => serde_json::to_string(&report),
                        Output::Human => Ok(format!(
                            "Memory Hub reconciled {} code commit(s); HEAD {}",
                            report.processed.len(),
                            report.head.as_deref().unwrap_or("unborn")
                        )),
                    };
                    match rendered {
                        Ok(rendered) => println!("{rendered}"),
                        Err(error) => {
                            eprintln!("memory-hub: unable to render reconcile report: {error}");
                            return Code::Internal;
                        }
                    }
                    Code::Success
                }
                Err(error) => {
                    eprintln!("memory-hub: reconcile failed: {error}");
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
        Command::Setup { output } => model::setup(model::Output::from(output)),
        Command::Remote {
            subcommand,
            project,
            output,
        } => {
            let Some(project) = absolute_project(project) else {
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let git_dir = match GitStore::discover_git_dir(&project) {
                Ok(git_dir) => git_dir,
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            match subcommand {
                RemoteCommand::Add { url, refspec } => {
                    let remote = MemoryRemote { url, refspec };
                    if let Err(error) = write_remote_config(&git_dir, &remote) {
                        eprintln!("memory-hub: {error}");
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
                        eprintln!("memory-hub: {error}");
                        Code::Internal
                    }
                },
                RemoteCommand::Remove => {
                    if let Err(error) = remove_remote_config(&git_dir) {
                        eprintln!("memory-hub: {error}");
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
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let store = match GitStore::open(&project) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            let git_dir = store.git_dir().to_path_buf();
            let remote = match read_remote_config(&git_dir) {
                Ok(Some(remote)) => remote,
                Ok(None) => {
                    eprintln!(
                        "memory-hub: no memory remote configured — run `memory-hub remote add <url>`"
                    );
                    return Code::Usage;
                }
                Err(error) => {
                    eprintln!("memory-hub: {error}");
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
                    eprintln!("memory-hub: {error}");
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
                eprintln!("memory-hub: unable to resolve current directory");
                return Code::Internal;
            };
            let git_dir = match GitStore::discover_git_dir(&project) {
                Ok(git_dir) => git_dir,
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            let remote = match read_remote_config(&git_dir) {
                Ok(Some(remote)) => remote,
                Ok(None) => {
                    eprintln!(
                        "memory-hub: no memory remote configured — run `memory-hub remote add <url>`"
                    );
                    return Code::Usage;
                }
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            // Apply push policy: check for stale records before network mutation.
            let store = match GitStore::open(&project) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            let policy_result = match check_push_policy(&store) {
                Ok(result) => result,
                Err(error) => {
                    eprintln!("memory-hub: {error}");
                    return Code::Internal;
                }
            };
            for warning in &policy_result.warnings {
                eprintln!("memory-hub: warning: {warning}");
            }
            if !policy_result.allowed {
                eprintln!(
                    "memory-hub: push blocked by memory_push_stale policy ({} stale records)",
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
                    eprintln!("memory-hub: {error}");
                    code
                }
            }
        }
        Command::Registry { subcommand, output } => crate::registry_cli::handle(subcommand, output),
        Command::Uninstall { purge, yes, output } => {
            crate::registry_cli::handle_uninstall(purge, yes, output)
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
fn resolve_embed_provider() -> Option<std::sync::Arc<dyn memory_hub_embed::EmbeddingProvider>> {
    use std::sync::Arc;
    let entry = crate::config::resolve_active_model();
    let opts = memory_hub_embed::DownloadOpts::default();
    if let Ok(memory_hub_embed::ModelVerification::Present { path, .. }) =
        memory_hub_embed::verify_model_sync(entry, &opts)
    {
        Some(Arc::new(memory_hub_embed::LlamaCppProvider::new(
            entry, path,
        )))
    } else {
        eprintln!(
            "memory-hub: warning: embedding model `{}` is not available — \
             vector search will degrade to FTS-only",
            entry.id
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::exit::Code;

    #[test]
    fn clap_help_and_version_are_successful() {
        assert_eq!(run(["memory-hub", "--help"]), Code::Success);
        assert_eq!(run(["memory-hub", "--version"]), Code::Success);
    }

    #[test]
    fn invalid_arguments_have_the_stable_usage_code() {
        assert_eq!(run(["memory-hub", "unknown"]), Code::Usage);
    }
}
