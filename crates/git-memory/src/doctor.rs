use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Ok,
    Error,
}

#[derive(Debug, Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    status: Status,
    version: &'static str,
    project: String,
    checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
struct Check {
    id: &'static str,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'static str>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<CheckData>,
}

#[derive(Debug, Serialize)]
struct CheckData {
    #[serde(skip_serializing_if = "Option::is_none")]
    git_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_version: Option<String>,
}

impl Report {
    pub(crate) fn is_healthy(&self) -> bool {
        self.status == Status::Ok
    }
}

pub(crate) fn inspect(project: Option<&Path>) -> Report {
    let requested_project = project.map_or_else(default_project, Path::to_path_buf);
    let display_project = requested_project.display().to_string();
    let mut checks = Vec::with_capacity(3);

    checks.push(project_check(&requested_project));

    let git_check = git_version_check();
    let git_available = git_check.status == Status::Ok;
    checks.push(git_check);

    if git_available && requested_project.is_dir() {
        checks.push(repository_check(&requested_project));
    } else {
        checks.push(Check::error(
            "git.repository",
            "repository_check_skipped",
            "repository check was skipped because a prerequisite failed",
        ));
    }

    let status = if checks.iter().all(|check| check.status == Status::Ok) {
        Status::Ok
    } else {
        Status::Error
    };

    Report {
        schema_version: SCHEMA_VERSION,
        status,
        version: env!("CARGO_PKG_VERSION"),
        project: display_project,
        checks,
    }
}

fn default_project() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn project_check(project: &Path) -> Check {
    if project.is_dir() {
        return Check::ok(
            "project.directory",
            format!("project directory is accessible: {}", project.display()),
            None,
        );
    }

    let (kind, message) = if project.exists() {
        (
            "project_not_directory",
            format!("project path is not a directory: {}", project.display()),
        )
    } else {
        (
            "project_not_found",
            format!("project directory does not exist: {}", project.display()),
        )
    };
    Check::error("project.directory", kind, message)
}

fn git_version_check() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = normalized_output(&output.stdout);
            Check::ok(
                "git.executable",
                format!("Git is available: {version}"),
                Some(CheckData {
                    git_dir: None,
                    git_version: Some(version),
                }),
            )
        }
        Ok(output) => Check::error(
            "git.executable",
            "git_unavailable",
            command_failure("git --version", output.status.code(), &output.stderr),
        ),
        Err(error) => Check::error(
            "git.executable",
            "git_unavailable",
            format!("unable to execute Git: {error}"),
        ),
    }
}

fn repository_check(project: &Path) -> Check {
    match git_output(project, ["rev-parse", "--absolute-git-dir"]) {
        Ok(git_dir) => Check::ok(
            "git.repository",
            format!("Git repository discovered: {git_dir}"),
            Some(CheckData {
                git_dir: Some(git_dir),
                git_version: None,
            }),
        ),
        Err(message) => Check::error("git.repository", "not_a_git_repository", message),
    }
}

fn git_output<I, S>(project: &Path, args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("git")
        .arg("-C")
        .arg(project)
        .args(args)
        .output()
        .map_err(|error| format!("unable to execute Git: {error}"))?;

    if output.status.success() {
        Ok(normalized_output(&output.stdout))
    } else {
        Err(command_failure(
            "git rev-parse --absolute-git-dir",
            output.status.code(),
            &output.stderr,
        ))
    }
}

fn normalized_output(output: &[u8]) -> String {
    String::from_utf8_lossy(output).trim().to_owned()
}

fn command_failure(command: &str, code: Option<i32>, stderr: &[u8]) -> String {
    let detail = normalized_output(stderr);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    match code {
        Some(code) => format!("{command} failed with exit code {code}{suffix}"),
        None => format!("{command} was terminated by a signal{suffix}"),
    }
}

impl Check {
    fn ok(id: &'static str, message: String, data: Option<CheckData>) -> Self {
        Self {
            id,
            status: Status::Ok,
            kind: None,
            message,
            data,
        }
    }

    fn error(id: &'static str, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            id,
            status: Status::Error,
            kind: Some(kind),
            message: message.into(),
            data: None,
        }
    }
}

pub(crate) fn render_human(report: &Report) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "Git Memory doctor {}", report.version)?;
    writeln!(output, "Project: {}", report.project)?;
    for check in &report.checks {
        let marker = if check.status == Status::Ok {
            "ok"
        } else {
            "error"
        };
        writeln!(output, "[{marker}] {}: {}", check.id, check.message)?;
    }
    writeln!(
        output,
        "Result: {}",
        if report.is_healthy() {
            "healthy"
        } else {
            "unhealthy"
        }
    )
}

pub(crate) fn render_json(report: &Report) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
    writeln!(output)
}

#[cfg(test)]
mod tests {
    use super::{SCHEMA_VERSION, Status, command_failure, inspect, normalized_output};

    #[test]
    fn trims_command_output() {
        assert_eq!(
            normalized_output(b"git version 1.2.3\r\n"),
            "git version 1.2.3"
        );
    }

    #[test]
    fn command_failure_keeps_code_and_detail() {
        assert_eq!(
            command_failure("git test", Some(128), b"fatal: no repo\n"),
            "git test failed with exit code 128: fatal: no repo"
        );
    }

    #[test]
    fn missing_project_is_a_structured_failure() {
        let report = inspect(Some(std::path::Path::new(
            "/a-git-memory-path-that-must-not-exist",
        )));
        assert_eq!(report.schema_version, SCHEMA_VERSION);
        assert_eq!(report.status, Status::Error);
        assert_eq!(report.checks[0].kind, Some("project_not_found"));
    }
}
