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
}
