//! Hookless reconciliation between code commits and Memory checkpoints.
//!
//! [`Reconciler`] is the module's small interface. Cursor persistence, Git
//! traversal, path diffs, freshness updates, crash recovery, and checkpoint
//! ordering remain inside its implementation.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt;
use git2::{ErrorCode, Oid, Repository, Sort};
use memory_hub_core::{Envelope, FreshnessState, StoredRecord};
use memory_hub_store::{GitStore, Operation, RecordId, StoreError, Transaction, TransactionPolicy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CURSOR_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceMode {
    #[default]
    Report,
    FullRebuild,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileErrorKind {
    InvalidProject,
    Repository,
    Cursor,
    Diverged,
    Store,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReconcileError {
    pub kind: ReconcileErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReconcileError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconciledCommit {
    pub code_revision: String,
    pub changed_paths: Vec<String>,
    pub stale_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcileReport {
    pub schema_version: u32,
    pub cursor_before: Option<String>,
    pub head: Option<String>,
    pub initialized: bool,
    pub rebuilt_after_divergence: bool,
    pub processed: Vec<ReconciledCommit>,
}

/// Read-only view of the gap between code history and the Memory cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReconcileInspection {
    pub schema_version: u32,
    /// Last code commit Memory processed, if reconciliation ever ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Current code `HEAD`, or `None` for an unborn branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Code commits recorded after the cursor. Always `0` when diverged.
    pub behind: usize,
    /// The cursor is not an ancestor of `HEAD` — history was rebased or reset,
    /// and catching up requires an explicit full rebuild.
    pub diverged: bool,
}

#[derive(Clone, Debug)]
pub struct Reconciler {
    project: PathBuf,
    /// Attached to every store this reconciler opens. Reconciliation rewrites
    /// records — freshness, mostly — and a rewrite is a write like any other,
    /// so it is checked like any other. Absent means the caller only reads.
    policy: Option<Arc<dyn TransactionPolicy>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Cursor {
    schema_version: u32,
    code_revision: String,
}

impl Reconciler {
    /// Open reconciliation for one explicit repository root or Git directory.
    ///
    /// # Errors
    ///
    /// Returns [`ReconcileError`] when the path is relative or not a Git
    /// repository.
    pub fn open(project: impl AsRef<Path>) -> Result<Self, ReconcileError> {
        let project = project.as_ref();
        if !project.is_absolute() {
            return Err(error(
                ReconcileErrorKind::InvalidProject,
                "project must be an absolute repository root or Git directory",
                json!({"field": "project"}),
            ));
        }
        let repository =
            Repository::discover(project).map_err(|source| git_error("discover", source))?;
        let project = repository
            .workdir()
            .unwrap_or_else(|| repository.path())
            .to_path_buf();
        Ok(Self {
            project,
            policy: None,
        })
    }

    /// Open this project's store with whatever rules were attached.
    fn open_store(&self) -> Result<GitStore, ReconcileError> {
        let store = GitStore::open(&self.project).map_err(store_error)?;
        Ok(match &self.policy {
            Some(policy) => store.with_policy(Arc::clone(policy)),
            None => store,
        })
    }

    /// Attach the rules every write this reconciler makes is checked against.
    ///
    /// A reconciler without them still reports and still checkpoints; what it
    /// cannot do is rewrite a record without anybody looking.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn TransactionPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Report how far Memory trails code history without changing anything.
    ///
    /// Diagnostics need this: running a full reconcile to answer "are we in
    /// sync?" creates checkpoints, advances the cursor and marks records
    /// stale, which is a surprising side effect for a read-only command.
    ///
    /// # Errors
    ///
    /// Returns [`ReconcileError`] when the repository, HEAD, or the cursor
    /// file cannot be read.
    pub fn inspect(&self) -> Result<ReconcileInspection, ReconcileError> {
        let repository = Repository::open(&self.project).map_err(|e| git_error("open", e))?;
        let cursor = load_cursor(&cursor_path(&repository))?;
        let head = head_oid(&repository)?;
        let cursor_revision = cursor.as_ref().map(|value| value.code_revision.clone());
        let (Some(head), Some(cursor)) = (head, cursor.as_ref()) else {
            return Ok(ReconcileInspection {
                schema_version: 1,
                cursor: cursor_revision,
                head: head.map(|oid| oid.to_string()),
                behind: 0,
                diverged: false,
            });
        };
        let cursor_oid = parse_cursor_oid(cursor)?;
        if cursor_oid == head {
            return Ok(ReconcileInspection {
                schema_version: 1,
                cursor: cursor_revision,
                head: Some(head.to_string()),
                behind: 0,
                diverged: false,
            });
        }
        let diverged = !repository
            .graph_descendant_of(head, cursor_oid)
            .map_err(|e| git_error("check code ancestry", e))?;
        let behind = if diverged {
            0
        } else {
            commits_between(&repository, cursor_oid, head)?.len()
        };
        Ok(ReconcileInspection {
            schema_version: 1,
            cursor: cursor_revision,
            head: Some(head.to_string()),
            behind,
            diverged,
        })
    }

    /// Reconcile every code commit missing from the local cursor.
    ///
    /// `Report` returns a structured divergence error without changing Memory.
    /// `FullRebuild` explicitly invalidates plaintext freshness, checkpoints
    /// the current code revision, and advances the cursor.
    ///
    /// # Errors
    ///
    /// Returns [`ReconcileError`] for Git/store/cursor failures or divergence
    /// that requires an explicit full rebuild.
    pub fn reconcile(&self, mode: DivergenceMode) -> Result<ReconcileReport, ReconcileError> {
        let repository = Repository::open(&self.project).map_err(|e| git_error("open", e))?;
        let cursor_path = cursor_path(&repository);
        let _lock = lock_cursor(&cursor_path)?;
        let cursor = load_cursor(&cursor_path)?;
        let cursor_before = cursor.as_ref().map(|value| value.code_revision.clone());
        let head = head_oid(&repository)?;
        let Some(head) = head else {
            return Ok(report(cursor_before, None, false, false, Vec::new()));
        };
        let head_text = head.to_string();
        let store = self.open_store()?;

        let Some(cursor) = cursor else {
            save_cursor(&cursor_path, head)?;
            return Ok(report(
                None,
                Some(head_text.clone()),
                true,
                false,
                vec![ReconciledCommit {
                    code_revision: head_text,
                    changed_paths: Vec::new(),
                    stale_keys: Vec::new(),
                }],
            ));
        };
        let cursor_oid = parse_cursor_oid(&cursor)?;
        if cursor_oid == head {
            return Ok(report(
                cursor_before,
                Some(head_text),
                false,
                false,
                Vec::new(),
            ));
        }
        if !repository
            .graph_descendant_of(head, cursor_oid)
            .map_err(|e| git_error("check code ancestry", e))?
        {
            return match mode {
                DivergenceMode::Report => Err(divergence_error(cursor_oid, head)),
                DivergenceMode::FullRebuild => {
                    rebuild_after_divergence(&store, &cursor_path, cursor_oid, head)
                }
            };
        }

        let commits = commits_between(&repository, cursor_oid, head)?;
        let mut processed = Vec::with_capacity(commits.len());
        // Path-bearing records are read once for the whole run. Re-reading the
        // corpus per commit made catch-up cost O(commits × records), which is
        // paid in full on the first session start after a long gap.
        let mut catalog = PathCatalog::load(&store)?;
        for oid in commits {
            let changed_paths = changed_paths(&repository, oid)?;
            let stale_keys = catalog.mark_stale(&store, oid, &changed_paths)?;
            let code_revision = oid.to_string();
            save_cursor(&cursor_path, oid)?;
            processed.push(ReconciledCommit {
                code_revision,
                changed_paths,
                stale_keys,
            });
        }
        Ok(report(
            cursor_before,
            Some(head_text),
            false,
            false,
            processed,
        ))
    }
}

fn report(
    cursor_before: Option<String>,
    head: Option<String>,
    initialized: bool,
    rebuilt_after_divergence: bool,
    processed: Vec<ReconciledCommit>,
) -> ReconcileReport {
    ReconcileReport {
        schema_version: 1,
        cursor_before,
        head,
        initialized,
        rebuilt_after_divergence,
        processed,
    }
}

fn head_oid(repository: &Repository) -> Result<Option<Oid>, ReconcileError> {
    match repository.head() {
        Ok(head) => head
            .peel_to_commit()
            .map(|commit| Some(commit.id()))
            .map_err(|error| git_error("peel HEAD", error)),
        Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
            Ok(None)
        }
        Err(error) => Err(git_error("read HEAD", error)),
    }
}

fn commits_between(
    repository: &Repository,
    cursor: Oid,
    head: Oid,
) -> Result<Vec<Oid>, ReconcileError> {
    let mut walk = repository
        .revwalk()
        .map_err(|error| git_error("create code walk", error))?;
    walk.push(head)
        .and_then(|()| walk.hide(cursor))
        .map_err(|error| git_error("configure code walk", error))?;
    walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
        .map_err(|error| git_error("sort code walk", error))?;
    walk.map(|item| item.map_err(|error| git_error("walk code history", error)))
        .collect()
}

fn changed_paths(repository: &Repository, oid: Oid) -> Result<Vec<String>, ReconcileError> {
    let commit = repository
        .find_commit(oid)
        .map_err(|error| git_error("find code commit", error))?;
    let current = commit
        .tree()
        .map_err(|error| git_error("find code tree", error))?;
    let parent_tree = if commit.parent_count() == 0 {
        None
    } else {
        Some(
            commit
                .parent(0)
                .and_then(|parent| parent.tree())
                .map_err(|error| git_error("find parent code tree", error))?,
        )
    };
    let diff = repository
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&current), None)
        .map_err(|error| git_error("diff code trees", error))?;
    let mut paths = BTreeSet::new();
    diff.foreach(
        &mut |delta, _| {
            for path in [delta.old_file().path(), delta.new_file().path()]
                .into_iter()
                .flatten()
            {
                paths.insert(path.to_string_lossy().into_owned());
            }
            true
        },
        None,
        None,
        None,
    )
    .map_err(|error| git_error("read code diff", error))?;
    Ok(paths.into_iter().collect())
}

/// The records a code change can make stale, held in memory for one run.
///
/// Only plaintext records that declare source paths can ever overlap a code
/// diff, so the rest are dropped at load time and never looked at again. What
/// remains is updated in place as commits are processed: a record marked stale
/// by one commit is already stale for the next, exactly as re-reading the
/// corpus would have shown.
struct PathCatalog {
    candidates: Vec<Envelope>,
}

impl PathCatalog {
    fn load(store: &GitStore) -> Result<Self, ReconcileError> {
        let snapshot = store.current().map_err(store_error)?;
        let candidates = snapshot
            .records()
            .map_err(store_error)?
            .into_iter()
            .filter_map(|(_, record)| match record {
                StoredRecord::Plaintext { envelope }
                    if !envelope.source_paths.observed.is_empty()
                        || !envelope.source_paths.scope.is_empty() =>
                {
                    Some(*envelope)
                }
                StoredRecord::Plaintext { .. } => None,
            })
            .collect();
        Ok(Self { candidates })
    }

    /// Mark every record whose paths the commit touched, and write them in one
    /// transaction. Returns the affected keys, sorted.
    fn mark_stale(
        &mut self,
        store: &GitStore,
        code_revision: Oid,
        changed_paths: &[String],
    ) -> Result<Vec<String>, ReconcileError> {
        if changed_paths.is_empty() || self.candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut keys = Vec::new();
        for envelope in &mut self.candidates {
            if !paths_overlap(envelope, changed_paths)
                || (envelope.freshness.state == FreshnessState::Stale
                    && envelope.freshness.reason.as_deref() == Some("code_paths_changed"))
            {
                continue;
            }
            envelope.freshness.state = FreshnessState::Stale;
            envelope.freshness.reason = Some("code_paths_changed".to_owned());
            keys.push(envelope.key.clone());
        }
        if keys.is_empty() {
            return Ok(keys);
        }
        // The revision is re-read rather than remembered: a checkpoint or a
        // concurrent writer may have moved it since the last transaction.
        let snapshot = store.current().map_err(store_error)?;
        let expected_revision = snapshot.revision().clone();
        // The catalog decides *which* records a code change touched; what gets
        // written is read fresh. Writing the catalog's own copy would carry a
        // whole envelope from the start of the run over anything somebody
        // edited during it — the freshness flag is all this owns.
        let current = snapshot.records().map_err(store_error)?;
        let mut operations = Vec::new();
        for key in &keys {
            let id = RecordId::plaintext(key);
            let Some((_, StoredRecord::Plaintext { envelope })) =
                current.iter().find(|(candidate, _)| *candidate == id)
            else {
                continue;
            };
            let mut envelope = (**envelope).clone();
            envelope.freshness.state = FreshnessState::Stale;
            envelope.freshness.reason = Some("code_paths_changed".to_owned());
            operations.push(Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(envelope),
            }));
        }
        if operations.is_empty() {
            keys.sort();
            return Ok(keys);
        }
        let transaction = Transaction {
            id: format!("reconcile-code-{code_revision}"),
            expected_revision,
            operations,
        };
        store.apply(&transaction).map_err(store_error)?;
        keys.sort();
        Ok(keys)
    }
}

fn paths_overlap(envelope: &memory_hub_core::Envelope, changed: &[String]) -> bool {
    changed.iter().any(|changed_path| {
        envelope
            .source_paths
            .observed
            .iter()
            .any(|path| path == changed_path)
            || envelope.source_paths.scope.iter().any(|scope| {
                scope == changed_path
                    || changed_path
                        .strip_prefix(scope)
                        .is_some_and(|suffix| suffix.starts_with('/'))
            })
    })
}

fn rebuild_after_divergence(
    store: &GitStore,
    cursor_path: &Path,
    old_cursor: Oid,
    head: Oid,
) -> Result<ReconcileReport, ReconcileError> {
    let snapshot = store.current().map_err(store_error)?;
    let mut operations = Vec::new();
    let mut stale_keys = Vec::new();
    for (_, mut record) in snapshot.records().map_err(store_error)? {
        let StoredRecord::Plaintext { envelope } = &mut record;
        envelope.freshness.state = FreshnessState::Unverified;
        envelope.freshness.code_revision = None;
        envelope.freshness.reason = Some("code_history_diverged".to_owned());
        stale_keys.push(envelope.key.clone());
        operations.push(Operation::put(record));
    }
    if !operations.is_empty() {
        store
            .apply(&Transaction {
                id: format!("reconcile-divergence-{old_cursor}-{head}"),
                expected_revision: snapshot.revision().clone(),
                operations,
            })
            .map_err(store_error)?;
    }
    stale_keys.sort();
    let head_text = head.to_string();
    save_cursor(cursor_path, head)?;
    Ok(report(
        Some(old_cursor.to_string()),
        Some(head_text.clone()),
        false,
        true,
        vec![ReconciledCommit {
            code_revision: head_text,
            changed_paths: Vec::new(),
            stale_keys,
        }],
    ))
}

fn cursor_path(repository: &Repository) -> PathBuf {
    repository.path().join("memory-hub/reconcile-cursor.json")
}

fn lock_cursor(cursor_path: &Path) -> Result<File, ReconcileError> {
    let parent = cursor_path.parent().ok_or_else(|| {
        error(
            ReconcileErrorKind::Cursor,
            "cursor path has no parent",
            Value::Null,
        )
    })?;
    fs::create_dir_all(parent).map_err(cursor_io_error)?;
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(parent.join("reconcile.lock"))
        .map_err(cursor_io_error)?;
    lock.lock_exclusive().map_err(cursor_io_error)?;
    Ok(lock)
}

fn load_cursor(path: &Path) -> Result<Option<Cursor>, ReconcileError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(cursor_io_error(error)),
    };
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|source| {
        error(
            ReconcileErrorKind::Cursor,
            "reconciliation cursor is invalid",
            json!({"detail": source.to_string()}),
        )
    })?;
    if cursor.schema_version != CURSOR_SCHEMA_VERSION {
        return Err(error(
            ReconcileErrorKind::Cursor,
            "reconciliation cursor version is unsupported",
            json!({
                "received": cursor.schema_version,
                "supported": CURSOR_SCHEMA_VERSION
            }),
        ));
    }
    Ok(Some(cursor))
}

fn save_cursor(path: &Path, oid: Oid) -> Result<(), ReconcileError> {
    let parent = path.parent().ok_or_else(|| {
        error(
            ReconcileErrorKind::Cursor,
            "cursor path has no parent",
            Value::Null,
        )
    })?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(cursor_io_error)?;
    serde_json::to_writer(
        &mut temporary,
        &Cursor {
            schema_version: CURSOR_SCHEMA_VERSION,
            code_revision: oid.to_string(),
        },
    )
    .map_err(|source| {
        error(
            ReconcileErrorKind::Cursor,
            "serialize reconciliation cursor",
            json!({"detail": source.to_string()}),
        )
    })?;
    temporary.as_file().sync_all().map_err(cursor_io_error)?;
    temporary
        .persist(path)
        .map_err(|error| cursor_io_error(error.error))?;
    Ok(())
}

fn parse_cursor_oid(cursor: &Cursor) -> Result<Oid, ReconcileError> {
    Oid::from_str(&cursor.code_revision).map_err(|_| {
        error(
            ReconcileErrorKind::Cursor,
            "reconciliation cursor is not a Git commit id",
            json!({"code_revision": cursor.code_revision}),
        )
    })
}

fn divergence_error(cursor: Oid, head: Oid) -> ReconcileError {
    error(
        ReconcileErrorKind::Diverged,
        "code history diverged from the reconciliation cursor",
        json!({
            "cursor": cursor.to_string(),
            "head": head.to_string(),
            "policy": "require_full_rebuild",
            "recovery_action": "reconcile_with_full_rebuild"
        }),
    )
}

fn store_error(source: StoreError) -> ReconcileError {
    error(
        ReconcileErrorKind::Store,
        source.message,
        json!({"store_kind": source.kind, "store_data": source.data}),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn git_error(operation: &str, source: git2::Error) -> ReconcileError {
    error(
        ReconcileErrorKind::Repository,
        format!("Git operation `{operation}` failed"),
        json!({
            "operation": operation,
            "class": format!("{:?}", source.class()).to_ascii_lowercase(),
            "code": format!("{:?}", source.code()).to_ascii_lowercase()
        }),
    )
}

#[allow(clippy::needless_pass_by_value)]
fn cursor_io_error(source: std::io::Error) -> ReconcileError {
    error(
        ReconcileErrorKind::Cursor,
        "reconciliation cursor I/O failed",
        json!({"detail": source.to_string()}),
    )
}

fn error(kind: ReconcileErrorKind, message: impl Into<String>, data: Value) -> ReconcileError {
    ReconcileError {
        kind,
        message: message.into(),
        data,
    }
}
