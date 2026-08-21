use std::fs;
use std::path::Path;
use std::process::Command;

use git2::{Oid, Repository, Signature};
use memory_hub_core::{Envelope, FreshnessState, StoredRecord};
use memory_hub_reconcile::{DivergenceMode, ReconcileErrorKind, Reconciler};
use memory_hub_store::{GitStore, Operation, RecordId, Transaction};

fn commit_file(
    repository: &Repository,
    root: &Path,
    path: &str,
    content: &str,
) -> Result<Oid, Box<dyn std::error::Error>> {
    let file = root.join(path);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(file, content)?;
    let mut index = repository.index()?;
    index.add_path(Path::new(path))?;
    index.write()?;
    let tree_oid = index.write_tree()?;
    let tree = repository.find_tree(tree_oid)?;
    let signature = Signature::now("Test", "test@example.invalid")?;
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents = parent.iter().collect::<Vec<_>>();
    Ok(repository.commit(Some("HEAD"), &signature, &signature, path, &tree, &parents)?)
}

fn tracked_record(key: &str, path: &str) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    let mut envelope = Envelope::new(key, "note", "content")?;
    envelope.source_paths.observed.push(path.to_owned());
    envelope.freshness.state = FreshnessState::Fresh;
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    })
}

#[test]
fn catches_up_every_commit_and_stales_matching_paths() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let repository = Repository::init(project.path())?;
    commit_file(&repository, project.path(), "src/lib.rs", "one")?;
    let reconciler = Reconciler::open(project.path())?;
    let initialized = reconciler.reconcile(DivergenceMode::Report)?;
    assert!(initialized.initialized);

    let store = GitStore::open(project.path())?;
    let base = store.current()?.revision().clone();
    store.apply(&Transaction {
        id: "seed".into(),
        expected_revision: base,
        operations: vec![Operation::put(tracked_record("design", "src/lib.rs")?)],
    })?;
    commit_file(&repository, project.path(), "src/lib.rs", "two")?;
    commit_file(&repository, project.path(), "README.md", "docs")?;

    let report = reconciler.reconcile(DivergenceMode::Report)?;
    assert_eq!(report.processed.len(), 2);
    assert_eq!(report.processed[0].stale_keys, ["design"]);
    assert!(report.processed[1].stale_keys.is_empty());
    let record = store
        .current()?
        .get(&RecordId::plaintext("design"))?
        .ok_or("record missing")?;
    let StoredRecord::Plaintext { envelope } = record;
    assert_eq!(envelope.freshness.state, FreshnessState::Stale);
    assert_eq!(
        envelope.freshness.reason.as_deref(),
        Some("code_paths_changed")
    );
    assert!(
        reconciler
            .reconcile(DivergenceMode::Report)?
            .processed
            .is_empty()
    );
    Ok(())
}

#[test]
fn divergence_requires_an_explicit_full_rebuild() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let repository = Repository::init(project.path())?;
    let original = commit_file(&repository, project.path(), "src/lib.rs", "one")?;
    let reconciler = Reconciler::open(project.path())?;
    reconciler.reconcile(DivergenceMode::Report)?;
    commit_file(&repository, project.path(), "src/lib.rs", "discarded")?;
    reconciler.reconcile(DivergenceMode::Report)?;

    repository.set_head_detached(original)?;
    commit_file(&repository, project.path(), "src/lib.rs", "rewritten")?;
    let Err(error) = reconciler.reconcile(DivergenceMode::Report) else {
        panic!("rewritten history must be reported");
    };
    assert_eq!(error.kind, ReconcileErrorKind::Diverged);
    assert_eq!(error.data["policy"], "require_full_rebuild");

    let rebuilt = reconciler.reconcile(DivergenceMode::FullRebuild)?;
    assert!(rebuilt.rebuilt_after_divergence);
    assert_eq!(rebuilt.processed.len(), 1);
    assert!(
        reconciler
            .reconcile(DivergenceMode::Report)?
            .processed
            .is_empty()
    );
    Ok(())
}

#[test]
fn linked_worktree_and_packed_refs_use_independent_cursors()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    let repository = Repository::init(project.path())?;
    commit_file(&repository, project.path(), "src/lib.rs", "primary")?;
    Reconciler::open(project.path())?.reconcile(DivergenceMode::Report)?;

    let linked_path = project.path().join("linked");
    let output = Command::new("git")
        .arg("-C")
        .arg(project.path())
        .args(["worktree", "add", "-b", "linked"])
        .arg(&linked_path)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(project.path())
        .args(["pack-refs", "--all"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let linked = Repository::open(&linked_path)?;
    let linked_reconciler = Reconciler::open(&linked_path)?;
    assert!(
        linked_reconciler
            .reconcile(DivergenceMode::Report)?
            .initialized
    );
    commit_file(&linked, &linked_path, "src/lib.rs", "linked")?;
    assert_eq!(
        linked_reconciler
            .reconcile(DivergenceMode::Report)?
            .processed
            .len(),
        1
    );

    let primary_cursor = repository.path().join("memory-hub/reconcile-cursor.json");
    let linked_cursor = linked.path().join("memory-hub/reconcile-cursor.json");
    assert_ne!(primary_cursor, linked_cursor);
    assert!(primary_cursor.is_file());
    assert!(linked_cursor.is_file());
    Ok(())
}
