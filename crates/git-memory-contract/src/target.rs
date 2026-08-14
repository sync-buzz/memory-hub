use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A black-box stdio server target.
///
/// Implementations return a process command only. There is intentionally no
/// trait for calling Rust store methods in-process.
pub trait ServerTarget: Sync {
    /// Stable label included in reports.
    fn label(&self) -> &str;

    /// Construct a fresh MCP server process for `project`.
    fn command(&self, project: &Path) -> Command;

    /// Construct the process used for the interrupted-write scenario.
    ///
    /// Targets may enable a process-level test failpoint here. The default uses
    /// the exact release command and therefore treats the commit outcome as
    /// intentionally ambiguous after termination.
    fn interruption_command(&self, project: &Path) -> Command {
        self.command(project)
    }

    /// Whether `interruption_command` acknowledges the pre-commit failpoint.
    fn has_synchronized_interruption(&self) -> bool {
        false
    }
}

/// Adapter for a shipped `git-memory` executable.
#[derive(Clone, Debug)]
pub struct ReleaseBinaryTarget {
    binary: PathBuf,
    extra_args: Vec<OsString>,
}

impl ReleaseBinaryTarget {
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            extra_args: Vec::new(),
        }
    }

    /// Arguments inserted after `mcp` and before `--project`.
    #[must_use]
    pub fn with_arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.extra_args.push(argument.as_ref().to_owned());
        self
    }
}

impl ServerTarget for ReleaseBinaryTarget {
    fn label(&self) -> &'static str {
        "release_binary"
    }

    fn command(&self, project: &Path) -> Command {
        let mut command = Command::new(&self.binary);
        command
            .arg("mcp")
            .args(&self.extra_args)
            .arg("--project")
            .arg(project);
        command
    }
}

/// Adapter for the deterministic fake server shipped with this harness.
#[derive(Clone, Debug)]
pub struct FakeServerTarget {
    binary: PathBuf,
}

impl FakeServerTarget {
    #[must_use]
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl ServerTarget for FakeServerTarget {
    fn label(&self) -> &'static str {
        "deterministic_fake"
    }

    fn command(&self, project: &Path) -> Command {
        let mut command = Command::new(&self.binary);
        command.arg("--project").arg(project);
        command
    }

    fn interruption_command(&self, project: &Path) -> Command {
        let mut command = self.command(project);
        command.env("GIT_MEMORY_CONTRACT_PAUSE_BEFORE_COMMIT", "1");
        command
    }

    fn has_synchronized_interruption(&self) -> bool {
        true
    }
}
