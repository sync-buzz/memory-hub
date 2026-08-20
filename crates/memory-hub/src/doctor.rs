use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use memory_hub_reconcile::{ReconcileErrorKind, Reconciler};
use memory_hub_store::{
    GitStore, RemoteMemory, probe_remote_memory, read_code_origin_url, read_remote_config,
};
use serde::Serialize;

use crate::model;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
}

impl Report {
    pub(crate) fn is_healthy(&self) -> bool {
        self.status == Status::Ok
    }
}

pub(crate) fn inspect(project: Option<&Path>) -> Report {
    let requested_project = project.map_or_else(default_project, Path::to_path_buf);
    let display_project = requested_project.display().to_string();
    let mut checks = Vec::with_capacity(8);

    checks.push(project_check(&requested_project));

    let git_check = git_version_check();
    let git_available = git_check.status == Status::Ok;
    checks.push(git_check);

    if git_available && requested_project.is_dir() {
        checks.push(repository_check(&requested_project));
        // Before anything that opens a store: `GitStore::open` creates the
        // staged ref, which would make an untouched clone look like a project
        // that has memory.
        checks.push(presence_check(&requested_project));
        checks.push(reconciliation_check(&requested_project));
        checks.push(encryption_check(&requested_project));
        checks.push(remote_privacy_check(&requested_project));
    } else {
        checks.push(Check::error(
            "git.repository",
            "repository_check_skipped",
            "repository check was skipped because a prerequisite failed",
        ));
        checks.push(Check::error(
            "memory.presence",
            "presence_check_skipped",
            "memory presence check was skipped because a prerequisite failed",
        ));
        checks.push(Check::error(
            "memory.reconciliation",
            "reconciliation_check_skipped",
            "reconciliation check was skipped because a prerequisite failed",
        ));
        checks.push(Check::error(
            "memory.encryption",
            "encryption_check_skipped",
            "encryption check was skipped because a prerequisite failed",
        ));
        checks.push(Check::error(
            "memory.remote_privacy",
            "remote_privacy_check_skipped",
            "remote privacy check was skipped because a prerequisite failed",
        ));
    }

    checks.push(model_check());

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

/// Tell "this project has no memory" apart from "its memory is on the remote".
///
/// `git clone` copies no `refs/memory/*`, so from inside a fresh clone the two
/// states are indistinguishable — and only one of them is a defect the user
/// can fix. This is the one place where the invisibility of memory stops being
/// a property and becomes a bug, so the check asks the remote, and only when
/// there is nothing local to explain the emptiness.
///
/// The question is asked without a configured memory remote too: before anyone
/// configures one, the code `origin` is the only address the repository knows,
/// and it is the address the memory almost certainly sits next to.
fn presence_check(project: &Path) -> Check {
    let git_dir = match GitStore::discover_git_dir(project) {
        Ok(dir) => dir,
        Err(error) => {
            return Check::ok_with_kind(
                "memory.presence",
                "git_dir_unavailable",
                format!("memory presence check skipped: {error}"),
                None,
            );
        }
    };

    match local_record_count(project, &git_dir) {
        Ok(0) => empty_memory_check(&git_dir),
        Ok(count) => Check::ok(
            "memory.presence",
            format!("memory is present in this repository: {count} record(s)"),
            None,
        ),
        Err(error) => Check::ok_with_kind(
            "memory.presence",
            "records_unavailable",
            format!("memory presence check skipped: {error}"),
            None,
        ),
    }
}

/// Count what memory this repository actually holds.
///
/// Records, not refs: `refs/memory/staged` is created by the first store that
/// opens the repository — including a previous `doctor` run — so its presence
/// says nothing about whether any memory was ever written. Listing the refs
/// first keeps the common empty case from opening a store at all.
fn local_record_count(project: &Path, git_dir: &Path) -> Result<usize, String> {
    let refs = git_output(
        project,
        "git for-each-ref refs/memory/",
        ["for-each-ref", "--format=%(refname)", "refs/memory/"],
    )?;
    if refs.is_empty() {
        return Ok(0);
    }
    let store = GitStore::open(git_dir).map_err(|error| error.to_string())?;
    let view = store.current().map_err(|error| error.to_string())?;
    view.records()
        .map(|records| records.len())
        .map_err(|error| error.to_string())
}

/// Explain an empty memory by asking the remote whether it has one.
fn empty_memory_check(git_dir: &Path) -> Check {
    let (url, configured) = match read_remote_config(git_dir) {
        Ok(Some(remote)) => (Some(remote.url), true),
        Ok(None) => (read_code_origin_url(git_dir).ok().flatten(), false),
        Err(error) => {
            return Check::ok_with_kind(
                "memory.presence",
                "config_unavailable",
                format!("memory presence check skipped: {error}"),
                None,
            );
        }
    };

    let Some(url) = url else {
        return Check::ok_with_kind(
            "memory.presence",
            "no_memory_anywhere",
            "this repository has no memory and no remote to ask about one — \
             memory begins with the first record written through `memory-hub mcp`"
                .to_string(),
            None,
        );
    };

    match probe_remote_memory(git_dir, &url) {
        Ok(RemoteMemory::Present) if configured => Check::error(
            "memory.presence",
            "memory_not_fetched",
            format!(
                "memory exists on {url} but not in this repository: `git clone` \
                 does not copy refs/memory/* — run `memory-hub fetch`"
            ),
        ),
        Ok(RemoteMemory::Present) => Check::error(
            "memory.presence",
            "memory_not_fetched",
            format!(
                "memory exists on the code remote {url} but not in this \
                 repository: `git clone` does not copy refs/memory/* — run \
                 `memory-hub remote add {url}`, then `memory-hub fetch`"
            ),
        ),
        Ok(RemoteMemory::Absent) => Check::ok_with_kind(
            "memory.presence",
            "no_memory_anywhere",
            format!(
                "this repository has no memory and {url} carries none — memory \
                 begins with the first record written through `memory-hub mcp`"
            ),
            None,
        ),
        Err(error) => Check::ok_with_kind(
            "memory.presence",
            "remote_unreachable",
            format!(
                "this repository has no memory and {url} could not be asked \
                 whether it has any ({error}) — memory does not arrive with \
                 `git clone`, so run `memory-hub fetch` once the remote answers"
            ),
            None,
        ),
    }
}

/// Report the reconciliation gap without closing it.
///
/// `doctor` is a diagnostic: it must not create checkpoints, advance the
/// cursor or mark records stale as a side effect of being run. Trailing code
/// commits are healthy — `memory-hub reconcile` (or the next Memory mutation)
/// catches them up. Divergence is the only failure, because it needs an
/// explicit decision from the user.
fn reconciliation_check(project: &Path) -> Check {
    match Reconciler::open(project).and_then(|reconciler| reconciler.inspect()) {
        Ok(inspection) if inspection.diverged => Check::error(
            "memory.reconciliation",
            "code_history_diverged",
            format!(
                "code history diverged from the reconciliation cursor {} — \
                 run `memory-hub reconcile --full-rebuild`",
                inspection.cursor.as_deref().unwrap_or("<none>")
            ),
        ),
        Ok(inspection) => Check::ok(
            "memory.reconciliation",
            match inspection.behind {
                0 => format!(
                    "code history is reconciled at {}",
                    inspection.head.as_deref().unwrap_or("unborn HEAD")
                ),
                behind => format!(
                    "{behind} code commit(s) pending reconciliation at {}",
                    inspection.head.as_deref().unwrap_or("unborn HEAD")
                ),
            },
            None,
        ),
        Err(error) => Check::error(
            "memory.reconciliation",
            reconcile_kind(error.kind),
            error.message,
        ),
    }
}

const fn reconcile_kind(kind: ReconcileErrorKind) -> &'static str {
    match kind {
        ReconcileErrorKind::InvalidProject => "invalid_project",
        ReconcileErrorKind::Repository => "repository_error",
        ReconcileErrorKind::Cursor => "cursor_error",
        ReconcileErrorKind::Diverged => "code_history_diverged",
        ReconcileErrorKind::Store => "store_error",
    }
}

fn encryption_check(project: &Path) -> Check {
    let store = match GitStore::open(project) {
        Ok(store) => store,
        Err(error) => {
            return Check::ok_with_kind(
                "memory.encryption",
                "store_unavailable",
                format!("encrypted-mode check skipped: {error}"),
                None,
            );
        }
    };
    let snapshot = match store.current() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return Check::ok_with_kind(
                "memory.encryption",
                "snapshot_unavailable",
                format!("encrypted-mode check skipped: {error}"),
                None,
            );
        }
    };
    let encrypted = match store.has_encrypted_records(snapshot.revision()) {
        Ok(encrypted) => encrypted,
        Err(error) => {
            return Check::ok_with_kind(
                "memory.encryption",
                "records_unavailable",
                format!("encrypted-mode check skipped: {error}"),
                None,
            );
        }
    };

    if encrypted {
        Check::ok_with_kind(
            "memory.encryption",
            "rotation_limitation",
            "encrypted mode is active: recipient rotation (add/remove) \
             re-encrypts future snapshots only — already-downloaded plaintext, \
             exported records, and old Git history remain readable by former \
             recipients. Old SSH/X25519 keys are not revoked; rotate keys at \
             the provider if compromise is suspected."
                .to_string(),
            None,
        )
    } else {
        Check::ok_with_kind(
            "memory.encryption",
            "plaintext_history_is_permanent",
            "encrypted mode is not active; records are stored in plaintext. \
             Turning encryption on later protects future writes only — the \
             plaintext blobs already in Git history stay readable to anyone \
             with the repository, so treat it as changing what happens next, \
             never as making the past private."
                .to_string(),
            None,
        )
    }
}

fn remote_privacy_check(project: &Path) -> Check {
    let git_dir = match GitStore::discover_git_dir(project) {
        Ok(dir) => dir,
        Err(error) => {
            return Check::ok_with_kind(
                "memory.remote_privacy",
                "git_dir_unavailable",
                format!("remote privacy check skipped: {error}"),
                None,
            );
        }
    };
    let remote = match read_remote_config(&git_dir) {
        Ok(Some(remote)) => remote,
        Ok(None) => {
            return Check::ok_with_kind(
                "memory.remote_privacy",
                "no_remote_configured",
                "no memory remote configured — refs/memory/* are local-only. \
                 Note: `git push --mirror` or `git push --all` will NOT publish \
                 memory refs unless an explicit refspec is used."
                    .to_string(),
                None,
            );
        }
        Err(error) => {
            return Check::ok_with_kind(
                "memory.remote_privacy",
                "config_unavailable",
                format!("remote privacy check skipped: {error}"),
                None,
            );
        }
    };

    Check::ok_with_kind(
        "memory.remote_privacy",
        "remote_configured",
        format!(
            "memory remote is configured: {}. \
             Caveats: (1) `git push --mirror` will publish refs/memory/* to \
             every remote — avoid it or exclude memory refs. (2) GitHub does \
             not protect custom refs (refs/memory/*); cryptographic integrity \
             via SSH commit signing is the only protection. (3) Recipient \
             rotation re-encrypts future snapshots only — former recipients \
             can still read old Git history and any downloaded plaintext.",
            remote.url
        ),
        None,
    )
}

fn model_check() -> Check {
    let check = model::doctor_check();
    let data = CheckData {
        git_dir: None,
        git_version: None,
        model_id: Some(check.model_id),
    };
    match check.status {
        model::DoctorModelStatus::Ok => Check::ok("memory.model", check.message, Some(data)),
        model::DoctorModelStatus::Missing | model::DoctorModelStatus::Broken => {
            let kind = check.status.kind().unwrap_or("model_check_error");
            Check::ok_with_kind("memory.model", kind, check.message, Some(data))
        }
        model::DoctorModelStatus::Error => Check::error(
            "memory.model",
            check.status.kind().unwrap_or("model_check_error"),
            check.message,
        ),
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
                    model_id: None,
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
    match git_output(
        project,
        "git rev-parse --absolute-git-dir",
        ["rev-parse", "--absolute-git-dir"],
    ) {
        Ok(git_dir) => Check::ok(
            "git.repository",
            format!("Git repository discovered: {git_dir}"),
            Some(CheckData {
                git_dir: Some(git_dir),
                git_version: None,
                model_id: None,
            }),
        ),
        Err(message) => Check::error("git.repository", "not_a_git_repository", message),
    }
}

/// Run a Git command in the project and return its trimmed standard output.
///
/// `command` is the label a failure is reported under: the caller knows what
/// it asked for, and a wrong label in a diagnostic sends the reader after the
/// wrong command.
fn git_output<I, S>(project: &Path, command: &str, args: I) -> Result<String, String>
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
            command,
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

    /// Build a check that is overall `Ok` but carries a diagnostic `kind`.
    ///
    /// Used for model missing/broken: the system is still healthy (FTS-only
    /// degradation), but we surface the issue as a warning kind so scripts
    /// consuming the JSON can detect it.
    fn ok_with_kind(
        id: &'static str,
        kind: &'static str,
        message: String,
        data: Option<CheckData>,
    ) -> Self {
        Self {
            id,
            status: Status::Ok,
            kind: Some(kind),
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
    writeln!(output, "Memory Hub doctor {}", report.version)?;
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
            "/a-memory-hub-path-that-must-not-exist",
        )));
        assert_eq!(report.schema_version, SCHEMA_VERSION);
        assert_eq!(report.status, Status::Error);
        assert_eq!(report.checks[0].kind, Some("project_not_found"));
    }
}
