#![allow(clippy::expect_used, clippy::unwrap_used)]

//! What the memory-hub review found, pinned.
//!
//! Each of these was a place where two different situations were being given
//! the same answer — "with the records" for a storage that is not the records,
//! `false` for a type that is perfectly writable, a media type for a document
//! only on the day it arrived. The tests state the distinction so it cannot be
//! collapsed again.

use std::path::Path;

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_schema::type_key;
use memory_hub_service::{MemoryService, ScanChange};
use memory_hub_store::{Operation, Revision};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A project whose `main` holds the records, with `docs` a folder of the
/// working tree and `vault` a second record-shaped folder that holds only
/// content — declared by hand, because it is a shape the tools refuse to
/// create and a hand-edited file can still produce.
fn service() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let config = json!({
        "config_version": 1,
        "storages": {
            "main": {"kind": "refs", "holds": ["records", "content"]},
            "docs": {"kind": "repo_folder", "path": "docs", "holds": ["content"]},
            "vault": {"kind": "folder", "path": "vault", "holds": ["content"]},
        },
    });
    std::fs::create_dir_all(project.path().join(".memory"))?;
    std::fs::write(
        project.path().join(".memory/config.json"),
        serde_json::to_vec_pretty(&config)?,
    )?;
    let service = MemoryService::open(project.path().to_path_buf());
    Ok((project, service))
}

fn put(key: &str, kind: &str, content: &str) -> Result<Operation, Box<dyn std::error::Error>> {
    Ok(Operation::put(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, kind, content)?),
    }))
}

/// A transaction id is answered once and then remembered, so each seed needs
/// its own.
static SEED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn seed(
    service: &MemoryService,
    operations: Vec<Operation>,
) -> Result<Revision, Box<dyn std::error::Error>> {
    let id = SEED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let expected = service.current_revision()?;
    Ok(service
        .apply_transaction(&format!("seed-{id}"), expected, operations)?
        .revision)
}

fn declare_type(
    service: &MemoryService,
    kind: &str,
    storage: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut definition = json!({"kind_name": kind});
    if let Some(storage) = storage
        && let Some(object) = definition.as_object_mut()
    {
        object.insert("storage".to_owned(), json!(storage));
    }
    seed(
        service,
        vec![put(
            &type_key(kind),
            "__type__",
            &serde_json::to_string(&definition)?,
        )?],
    )?;
    Ok(())
}

fn envelope_of(service: &MemoryService, key: &str) -> Result<Envelope, Box<dyn std::error::Error>> {
    let view = service.get_record(key, None)?;
    match view.record {
        Some(StoredRecord::Plaintext { envelope }) => Ok(*envelope),
        _ => Err(format!("no plaintext record for {key}").into()),
    }
}

fn write_doc(project: &Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    let path = project.join("docs").join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

// --- saying "with the records" out loud is not saying nothing -------------

/// A type may name the storage that holds records — that is the canonical way
/// to say its bodies sit beside its metadata. Written that way, it was reported
/// as unwritable: the answer came from looking for a folder, finding none, and
/// treating the absence as a refusal.
#[test]
fn a_type_that_names_the_records_storage_can_still_be_written() -> TestResult {
    let (_project, service) = service()?;
    declare_type(&service, "note", Some("main"))?;
    declare_type(&service, "memo", None)?;
    declare_type(&service, "doc", Some("docs"))?;

    let types = service.list_types()?;
    let writable = |kind: &str| {
        types
            .iter()
            .find(|summary| summary.kind_name == kind)
            .map(|summary| summary.writable)
    };

    assert_eq!(
        writable("note"),
        Some(true),
        "naming the records storage is naming a place a record can be written"
    );
    assert_eq!(writable("memo"), Some(true), "and so is naming nothing");
    assert_eq!(
        writable("doc"),
        Some(true),
        "and so is a folder of the tree"
    );
    Ok(())
}

// --- a storage that cannot hold bodies says so ----------------------------

/// `vault` holds content and is not a folder of the working tree, so there is
/// nowhere in it to put a body. The migration is refused rather than reporting
/// success and inlining every body into its record — the same effect as
/// `storage: null`, which is the one thing the caller did not ask for.
#[test]
fn migrating_into_a_storage_that_cannot_hold_bodies_is_refused() -> TestResult {
    let (_project, service) = service()?;
    declare_type(&service, "note", None)?;
    seed(&service, vec![put("note-1", "note", "a body")?])?;

    let error = service
        .plan_migration("note", Some("vault"))
        .expect_err("a storage that cannot hold bodies is not a destination");
    assert_eq!(error.kind, "unsupported");
    assert_eq!(error.data["storage"], json!("vault"));

    // And the record is untouched: the refusal came before anything was
    // written, not after half of it was.
    assert_eq!(envelope_of(&service, "note-1")?.content, "a body");
    Ok(())
}

// --- the media type follows the file name ---------------------------------

/// It is read off the file name, and a move is how a file name changes. Set
/// only when a document first arrived, a record renamed from `.md` to `.png`
/// went on announcing itself as text.
#[test]
fn a_renamed_document_carries_its_new_media_type() -> TestResult {
    let (project, service) = service()?;
    std::fs::create_dir_all(project.path().join("docs"))?;
    declare_type(&service, "doc", Some("docs"))?;

    let png = b"\x89PNG\r\n\x1a\n and then some bytes";
    write_doc(project.path(), "figure.md", png)?;
    let first = service.scan_attachments("scan-1")?;
    let key = first
        .changes
        .iter()
        .find_map(|change| match change {
            ScanChange::New { key, .. } => Some(key.clone()),
            _ => None,
        })
        .ok_or("the document was seen")?;
    assert_eq!(
        envelope_of(&service, &key)?.media_type.as_deref(),
        Some("text/markdown"),
        "read off the name it arrived under"
    );

    std::fs::rename(
        project.path().join("docs/figure.md"),
        project.path().join("docs/figure.png"),
    )?;
    service.scan_attachments("scan-2")?;

    assert_eq!(
        envelope_of(&service, &key)?.media_type.as_deref(),
        Some("image/png"),
        "and off the name it has now"
    );
    Ok(())
}

/// Publishing a body into a folder gives the record a media type on the same
/// terms a scan would; bringing it back into the record takes it away, because
/// it described a file and there is no longer a file.
#[test]
fn a_migration_keeps_the_media_type_true() -> TestResult {
    let (project, service) = service()?;
    std::fs::create_dir_all(project.path().join("docs"))?;
    declare_type(&service, "doc", None)?;
    seed(&service, vec![put("doc-1", "doc", "a body")?])?;
    assert_eq!(envelope_of(&service, "doc-1")?.media_type, None);

    service.migrate_storage(
        "out",
        "doc",
        Some("docs"),
        &["content_becomes_visible".to_owned()],
    )?;
    assert_eq!(
        envelope_of(&service, "doc-1")?.media_type.as_deref(),
        Some("text/markdown"),
        "there is a file now, and its name says what it is"
    );

    service.migrate_storage(
        "back",
        "doc",
        None,
        &[
            "does_not_hide_published_history".to_owned(),
            "files_are_left_in_place".to_owned(),
        ],
    )?;
    assert_eq!(
        envelope_of(&service, "doc-1")?.media_type,
        None,
        "and no file now"
    );
    Ok(())
}

// --- the gate does not depend on how the project is stored ----------------

/// Every read of records answers `not_initialised` without a declaration. The
/// registry is records too, and read through a different door.
#[test]
fn a_project_without_a_declaration_refuses_reads_at_every_door() -> TestResult {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let service = MemoryService::open(project.path().to_path_buf());

    for kind in [
        service.record_store().err().map(|error| error.kind),
        service.schema_registry().err().map(|error| error.kind),
        service.list_types().err().map(|error| error.kind),
        service
            .get_record("anything", None)
            .err()
            .map(|error| error.kind),
    ] {
        assert_eq!(kind.as_deref(), Some("not_initialised"));
    }
    Ok(())
}
