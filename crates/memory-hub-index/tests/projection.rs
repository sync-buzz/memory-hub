use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{Projection, ProjectionState};
use memory_hub_store::{GitStore, Operation, RecordId, Transaction};
use std::fs::OpenOptions;
use std::time::{Duration, Instant};

fn record(key: &str, content: &str) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content)?),
    })
}

#[tokio::test]
async fn rebuild_and_incremental_update_follow_indexed_revisions()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let projection_dir = project.path().join("derived-index");
    let projection = Projection::open(&projection_dir).await?;
    assert_eq!(projection.status()?.state, ProjectionState::Lagging);

    let empty = store.current()?;
    projection.rebuild(&empty).await?;
    assert!(projection.records(empty.revision()).await?.is_empty());

    let first = store.apply(&Transaction {
        id: "first".into(),
        expected_revision: empty.revision().clone(),
        operations: vec![
            Operation::put(record("alpha", "one")?),
            Operation::put(record("remove", "gone")?),
        ],
    })?;
    projection
        .update(&store, empty.revision(), &first.revision)
        .await?;
    assert_eq!(projection.records(&first.revision).await?.len(), 2);

    let second = store.apply(&Transaction {
        id: "second".into(),
        expected_revision: first.revision.clone(),
        operations: vec![
            Operation::put(record("alpha", "two")?),
            Operation::delete(RecordId::plaintext("remove")),
        ],
    })?;
    projection
        .update(&store, &first.revision, &second.revision)
        .await?;
    let rows = projection.records(&second.revision).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content.as_deref(), Some("two"));
    assert!(projection.records(&first.revision).await.is_err());
    Ok(())
}

#[tokio::test]
async fn deleted_index_rebuilds_from_git_without_client_callback()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    store.apply(&Transaction {
        id: "seed".into(),
        expected_revision: store.current()?.revision().clone(),
        operations: vec![Operation::put(record("durable", "canonical")?)],
    })?;
    let snapshot = store.current()?;
    let projection_dir = project.path().join("derived-index");
    let projection = Projection::open(&projection_dir).await?;
    projection.rebuild(&snapshot).await?;
    std::fs::remove_dir_all(projection_dir.join("lance"))?;

    let projection = Projection::open(&projection_dir).await?;
    projection.synchronize(&store).await?;
    let rows = projection.records(snapshot.revision()).await?;
    assert_eq!(rows[0].id, "durable");
    Ok(())
}

#[tokio::test]
async fn corrupt_status_is_recovered_automatically() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    store.apply(&Transaction {
        id: "seed-corrupt-status".into(),
        expected_revision: store.current()?.revision().clone(),
        operations: vec![Operation::put(record("survivor", "from git")?)],
    })?;
    let projection_dir = project.path().join("derived-index");
    let projection = Projection::open(&projection_dir).await?;
    projection.rebuild(&store.current()?).await?;
    std::fs::write(projection_dir.join("status.json"), "not json")?;

    let status = projection.synchronize(&store).await?;
    assert_eq!(status.state, ProjectionState::Fresh);
    assert_eq!(
        projection.records(store.current()?.revision()).await?.len(),
        1
    );
    Ok(())
}

/// An index written by an older build has a column set this one does not
/// expect. The status version is what says so, and it has to lead to a rebuild
/// rather than to an error or — worse — to reading a table that is missing a
/// column this build filters on.
#[tokio::test]
async fn an_index_from_an_older_schema_is_rebuilt() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    store.apply(&Transaction {
        id: "seed-old-schema".into(),
        expected_revision: store.current()?.revision().clone(),
        operations: vec![Operation::put(record("survivor", "from git")?)],
    })?;
    let projection_dir = project.path().join("derived-index");
    let projection = Projection::open(&projection_dir).await?;
    projection.rebuild(&store.current()?).await?;

    // Exactly what an index built before the folder column looks like.
    let status_path = projection_dir.join("status.json");
    let mut status: serde_json::Value = serde_json::from_slice(&std::fs::read(&status_path)?)?;
    status["schema_version"] = serde_json::json!(1);
    std::fs::write(&status_path, serde_json::to_vec(&status)?)?;

    let status = projection.synchronize(&store).await?;
    assert_eq!(status.state, ProjectionState::Fresh);
    assert_eq!(
        projection.records(store.current()?.revision()).await?.len(),
        1,
        "the corpus is projected again, not lost"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_recovery_is_serialized_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let snapshot = store.current()?;
    let projection = Projection::open(project.path().join("derived-index")).await?;
    projection.rebuild(&snapshot).await?;
    let left = projection.clone();
    let right = projection.clone();
    let left_snapshot = snapshot.clone();
    let right_snapshot = snapshot.clone();
    let (left_result, right_result) =
        tokio::join!(left.recover(&left_snapshot), right.recover(&right_snapshot));
    left_result?;
    right_result?;
    assert!(projection.records(snapshot.revision()).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn reader_waits_for_the_projection_writer_lock() -> Result<(), Box<dyn std::error::Error>> {
    use fs2::FileExt;

    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let projection_dir = project.path().join("derived-index");
    let projection = Projection::open(&projection_dir).await?;
    let snapshot = store.current()?;
    projection.rebuild(&snapshot).await?;

    let writer = OpenOptions::new()
        .read(true)
        .write(true)
        .open(projection_dir.join("projection.lock"))?;
    writer.lock_exclusive()?;
    let reader_projection = projection.clone();
    let revision = snapshot.revision().clone();
    let reader = tokio::spawn(async move { reader_projection.records(&revision).await });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!reader.is_finished(), "reader bypassed the writer lock");
    writer.unlock()?;
    assert!(reader.await??.is_empty());
    Ok(())
}

#[tokio::test]
#[ignore = "benchmark: run explicitly with --ignored --nocapture"]
async fn incremental_update_cost_tracks_delta_not_corpus() -> Result<(), Box<dyn std::error::Error>>
{
    let mut incremental = Vec::new();
    for corpus in [250, 1_500, 5_000] {
        let (rebuild, update) = benchmark_case(corpus).await?;
        eprintln!(
            "corpus={corpus} delta=1 rebuild_ms={} incremental_ms={}",
            rebuild.as_millis(),
            update.as_millis()
        );
        assert!(update < rebuild);
        incremental.push(update);
    }
    let fastest = incremental.iter().min().ok_or("missing benchmark")?;
    let slowest = incremental.iter().max().ok_or("missing benchmark")?;
    assert!(
        *slowest < *fastest * 8,
        "delta=1 cost grew with the 20x corpus increase"
    );
    Ok(())
}

async fn benchmark_case(corpus: usize) -> Result<(Duration, Duration), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let operations = (0..corpus)
        .map(|index| record(&format!("record-{index:05}"), "stable body").map(Operation::put))
        .collect::<Result<Vec<_>, _>>()?;
    let seeded = store.apply(&Transaction {
        id: "benchmark-seed".into(),
        expected_revision: store.current()?.revision().clone(),
        operations,
    })?;
    let projection = Projection::open(project.path().join("derived-index")).await?;
    let started = Instant::now();
    projection
        .rebuild(&store.snapshot(&seeded.revision)?)
        .await?;
    let rebuild = started.elapsed();
    let changed = store.apply(&Transaction {
        id: "benchmark-one-record".into(),
        expected_revision: seeded.revision.clone(),
        operations: vec![Operation::put(record("record-00000", "changed body")?)],
    })?;
    let started = Instant::now();
    projection
        .update(&store, &seeded.revision, &changed.revision)
        .await?;
    Ok((rebuild, started.elapsed()))
}
