//! Where the files are, and how the folder is read as a whole.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use memory_hub_core::StoredRecord;
use memory_hub_engine::{ApplyResult, RecordId, Revision, StoreError, StoreErrorKind};

use crate::paths;

/// Records, one file each.
const RECORDS_DIR: &str = "records";
/// The disposable read model. Under the store, because a store decides where
/// its own projection lives.
const INDEX_DIR: &str = "index";
/// Serialises writers. Readers never take it: a half-written record is
/// impossible — files arrive by rename — so a reader has nothing to wait for.
const LOCK_FILE: &str = "lock";
/// Transactions already applied, so a retry after a severed connection returns
/// the first answer instead of doing the work twice.
const APPLIED_FILE: &str = "applied.json";
/// How many transaction receipts to keep. Enough to answer a client that
/// reconnects and retries; not a log.
const APPLIED_LIMIT: usize = 128;

#[derive(Debug)]
pub(crate) struct Layout {
    root: PathBuf,
}

/// Holds the writer lock for as long as a transaction runs.
#[derive(Debug)]
pub(crate) struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Unlocking is what the file handle does when it closes; an explicit
        // failure here has nowhere to go and nothing to fix.
        let _ = FileExt::unlock(&self.0);
    }
}

impl Layout {
    pub(crate) fn create(root: &Path) -> Result<Self, StoreError> {
        let root = root.to_path_buf();
        for directory in [root.join(RECORDS_DIR), root.join(INDEX_DIR)] {
            fs::create_dir_all(&directory).map_err(|error| io(&directory, "create", &error))?;
        }
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn index_root(&self) -> PathBuf {
        self.root.join(INDEX_DIR)
    }

    pub(crate) fn records_dir(&self) -> PathBuf {
        self.root.join(RECORDS_DIR)
    }

    pub(crate) fn path_in_root(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    /// Take the writer lock, waiting for whoever holds it.
    pub(crate) fn lock(&self) -> Result<LockGuard, StoreError> {
        let path = self.root.join(LOCK_FILE);
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| io(&path, "open", &error))?;
        FileExt::lock_exclusive(&file).map_err(|error| io(&path, "lock", &error))?;
        Ok(LockGuard(file))
    }

    /// Every record the folder holds.
    pub(crate) fn read_all(&self) -> Result<BTreeMap<RecordId, StoredRecord>, StoreError> {
        let mut records = BTreeMap::new();
        let base = self.records_dir();
        collect(&base, &base, &mut records)?;
        Ok(records)
    }

    /// The state token for a corpus.
    ///
    /// A digest of what is there, not a counter: it needs no file of its own,
    /// it survives the folder being copied, and comparing two of them is the
    /// whole of compare-and-swap.
    pub(crate) fn digest(records: &BTreeMap<RecordId, StoredRecord>) -> Revision {
        let mut hasher = blake3::Hasher::new();
        for (id, record) in records {
            hasher.update(id.display_value().as_bytes());
            hasher.update(b"\0");
            hasher.update(content_digest(record).as_bytes());
            hasher.update(b"\n");
        }
        Revision::new(hasher.finalize().to_hex().to_string())
    }

    /// The answer a transaction id already got, if it got one.
    pub(crate) fn replay(&self, transaction_id: &str) -> Result<Option<ApplyResult>, StoreError> {
        Ok(self.applied()?.remove(transaction_id))
    }

    pub(crate) fn applied(&self) -> Result<BTreeMap<String, ApplyResult>, StoreError> {
        let path = self.path_in_root(APPLIED_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(BTreeMap::new());
        };
        serde_json::from_str(&text).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Repository,
                "the record of applied transactions is unreadable",
                serde_json::json!({"path": path.display().to_string(), "detail": error.to_string()}),
            )
        })
    }

    /// Remember what a transaction answered, forgetting the oldest.
    pub(crate) fn remember(
        &self,
        transaction_id: &str,
        result: &ApplyResult,
    ) -> Result<(), StoreError> {
        let mut applied = self.applied()?;
        applied.insert(transaction_id.to_owned(), result.clone());
        while applied.len() > APPLIED_LIMIT {
            let Some(oldest) = applied.keys().next().cloned() else {
                break;
            };
            applied.remove(&oldest);
        }
        let path = self.path_in_root(APPLIED_FILE);
        let bytes = serde_json::to_vec_pretty(&applied).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Repository,
                "the record of applied transactions could not be written",
                serde_json::json!({
                    "path": path.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;
        write_atomic(&path, &bytes)
    }

    pub(crate) fn record_file(&self, id: &RecordId) -> Result<PathBuf, StoreError> {
        Ok(self.records_dir().join(paths::record_path(id)?))
    }
}

/// Walk the records folder, at any depth.
fn collect(
    base: &Path,
    directory: &Path,
    into: &mut BTreeMap<RecordId, StoredRecord>,
) -> Result<(), StoreError> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Links are not followed: one pointing outside would pull somebody
        // else's file into the corpus, and one pointing at an ancestor would
        // not terminate.
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect(base, &path, into)?;
            continue;
        }
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let Some(id) = paths::record_id(relative) else {
            continue;
        };
        let text = fs::read_to_string(&path).map_err(|error| io(&path, "read", &error))?;
        let record: StoredRecord = serde_json::from_str(&text).map_err(|error| {
            StoreError::new(
                StoreErrorKind::InvalidRecord,
                "a record file does not hold a record",
                serde_json::json!({
                    "path": relative.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;
        into.insert(id, record);
    }
    Ok(())
}

/// What a record's bytes hash to, for the corpus digest.
fn content_digest(record: &StoredRecord) -> String {
    match record {
        StoredRecord::Plaintext { envelope } => envelope.content_hash.as_str().to_owned(),
    }
}

/// Write a file so that a reader sees either the old bytes or the new ones.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io(parent, "create", &error))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|error| io(&temporary, "write", &error))?;
    fs::rename(&temporary, path).map_err(|error| io(path, "rename", &error))
}

pub(crate) fn io(path: &Path, action: &str, error: &std::io::Error) -> StoreError {
    StoreError::new(
        StoreErrorKind::Repository,
        format!("could not {action} `{}`", path.display()),
        serde_json::json!({"path": path.display().to_string(), "detail": error.to_string()}),
    )
}
