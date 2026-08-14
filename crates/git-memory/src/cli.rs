use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Check whether Git Memory can operate in a repository.
    Doctor {
        /// Repository or a path inside it. Defaults to the current directory.
        #[arg(long, value_name = "PATH")]
        project: Option<PathBuf>,

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
