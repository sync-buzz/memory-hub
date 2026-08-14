use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use git_memory_reconcile::{DivergenceMode, Reconciler};

use crate::doctor;
use crate::exit::Code;

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

        /// Select human-readable or stable JSON output.
        #[arg(long, value_enum, default_value_t = Output::Human)]
        output: Output,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum Output {
    #[default]
    Human,
    Json,
}

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
            match Reconciler::open(project).and_then(|reconciler| reconciler.reconcile(mode)) {
                Ok(report) => {
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
    }
}

fn absolute_project(project: Option<PathBuf>) -> Option<PathBuf> {
    match project {
        Some(project) if project.is_absolute() => Some(project),
        Some(project) => std::env::current_dir().ok().map(|cwd| cwd.join(project)),
        None => std::env::current_dir().ok(),
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
