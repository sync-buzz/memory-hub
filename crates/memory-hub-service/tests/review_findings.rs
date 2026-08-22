#![allow(clippy::expect_used, clippy::unwrap_used)]

//! What the 2026-08-18 review found, pinned.
//!
//! Each test was written first as a hypothesis about a defect — expressed as
//! the behaviour the corpus says should hold — and every one of them failed.
//! They are kept because the failures were not typos: each is a rule that is
//! easy to lose again, and cheap to state.

use std::path::Path;
use std::sync::Arc;

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_embed::{EmbeddingProvider, MockProvider};
use memory_hub_index::{SearchFilters, SearchRequest};
use memory_hub_schema::type_key;
use memory_hub_service::{ListingQuery, MemoryService, ScanChange};
use memory_hub_store::{Operation, Revision};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn service() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    open_service(None)
}

/// The same project, answering the meaning channel with a stub.
///
/// Left to itself the service resolves whatever model the machine has on disk,
/// so a test that reaches the vector channel passes where a GGUF was
/// downloaded and fails where none ever was. The stub is the same everywhere.
fn service_with_vectors() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>>
{
    open_service(Some(Arc::new(MockProvider::new(64).constant())))
}

fn open_service(
    provider: Option<Arc<dyn EmbeddingProvider>>,
) -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let mut service = MemoryService::open(
        project.path().to_path_buf(),
        memory_hub_service::RecordsIn::GitMetadata,
    );
    if provider.is_some() {
        service = service.with_provider(provider);
    }
    Ok((project, service))
}

fn put(key: &str, kind: &str, content: &str) -> Result<Operation, Box<dyn std::error::Error>> {
    Ok(Operation::put(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, kind, content)?),
    }))
}

fn seed(
    service: &MemoryService,
    operations: Vec<Operation>,
) -> Result<Revision, Box<dyn std::error::Error>> {
    let expected = service.current_revision()?;
    Ok(service
        .apply_transaction("seed", expected, operations)?
        .revision)
}

fn attached_project() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    attach(service()?)
}

/// An attached project whose meaning channel does not depend on the machine.
fn attached_project_with_vectors()
-> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    attach(service_with_vectors()?)
}

/// Declare the two types these tests use and create the folder `doc` names.
fn attach(
    (project, service): (tempfile::TempDir, MemoryService),
) -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(project.path().join("docs"))?;
    seed(
        &service,
        vec![
            put(
                &type_key("doc"),
                "__type__",
                &serde_json::to_string(&json!({
                    "kind_name": "doc",
                    "storage": "docs"
                }))?,
            )?,
            put(
                &type_key("note"),
                "__type__",
                &serde_json::to_string(&json!({"kind_name": "note"}))?,
            )?,
        ],
    )?;
    Ok((project, service))
}

fn write_doc(project: &Path, name: &str, body: &str) -> std::io::Result<()> {
    if let Some(parent) = project.join("docs").join(name).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(project.join("docs").join(name), body)
}

fn commit_all(project: &Path, message: &str) -> Result<(), Box<dyn std::error::Error>> {
    let repository = Repository::open(project)?;
    let mut index = repository.index()?;
    index.add_all(["*"], git2::IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree = repository.find_tree(index.write_tree()?)?;
    let signature = git2::Signature::now("Test", "test@example.invalid")?;
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repository.commit(
        Some("HEAD"),
        &signature,
        &signature,
        message,
        &tree,
        &parents,
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

// --- the scan at project open reuses one transaction id forever ---------

/// `Session::initialize` derives the scan's transaction id from the project
/// path alone, so every project open sends the same id. The second open that
/// has something different to write is refused as a reused transaction, and the
/// folder is never reconciled again.
#[test]
fn scan_at_open_can_run_more_than_once() -> TestResult {
    let (project, service) = attached_project()?;
    let id = "open-fixed-id";

    write_doc(project.path(), "guide.md", "# Guide\n")?;
    commit_all(project.path(), "add guide")?;
    let first = service.scan_attachments(id)?;
    assert_eq!(first.applied, 1);

    // A second session opens later; somebody edited the document meanwhile.
    write_doc(project.path(), "guide.md", "# Guide, revised\n")?;
    commit_all(project.path(), "edit guide")?;
    let second = service.scan_attachments(id)?;
    assert_eq!(
        second.applied, 1,
        "the edit is picked up by the next open, not refused as a reused transaction"
    );
    Ok(())
}

// --- unrelated documents that hash alike are silently paired ------------

/// Two documents with identical bytes are common — empty placeholders, a
/// copied template. When one record's file is gone (another branch, say) and an
/// unrelated new file has the same digest, the pair is read as a rename and the
/// key is carried onto a document nobody moved. The rename-candidate path has a
/// similarity floor for exactly this reason; the digest path has none.
#[test]
fn identical_bytes_at_unrelated_paths_are_not_silently_a_rename() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "auth/overview.md", "TODO\n")?;
    commit_all(project.path(), "add overview")?;
    service.scan_attachments("first")?;
    let key = envelope_of(&service, "auth-overview")?.key;
    assert_eq!(key, "auth-overview");

    // The document goes away — deleted, or simply on another branch.
    std::fs::remove_file(project.path().join("docs/auth/overview.md"))?;
    commit_all(project.path(), "remove overview")?;
    service.scan_attachments("second")?;

    // Somewhere else entirely, somebody adds a placeholder with the same body.
    write_doc(project.path(), "billing/invoices.md", "TODO\n")?;
    commit_all(project.path(), "add invoices")?;
    let report = service.scan_attachments("third")?;

    let moved: Vec<&ScanChange> = report
        .changes
        .iter()
        .filter(|change| matches!(change, ScanChange::Moved { .. }))
        .collect();
    assert!(
        moved.is_empty(),
        "an unrelated document with the same bytes must not carry a key onto itself: {moved:?}"
    );
    Ok(())
}

// --- an attached document's text is never searchable -------------------

/// The leading scenario of M5 is an existing `docs/` folder attached so an
/// agent can see the documentation. The record's `content` is empty by
/// construction, and the projection indexes `content` — so nothing in the
/// document's text is findable.
#[test]
fn an_attached_document_is_findable_by_its_text() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(
        project.path(),
        "auth.md",
        "# Authentication\n\nTokens are rotated by the sessiond supervisor.\n",
    )?;
    commit_all(project.path(), "add auth")?;
    service.scan_attachments("scan")?;

    let result = service.search(&SearchRequest {
        query: "sessiond".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters::default(),
        revision: service.current_revision()?,
    })?;
    // The full-text channel is what has to match: with a model on disk the
    // vector channel returns the nearest row of a two-row corpus whatever the
    // query was, which proves nothing about the text being indexed.
    assert!(
        result
            .hits
            .iter()
            .any(|hit| hit.id == "auth" && hit.fts_score.is_some()),
        "the document's own words find it through full text: {:?}",
        result
            .hits
            .iter()
            .map(|hit| (&hit.id, hit.fts_score))
            .collect::<Vec<_>>()
    );
    Ok(())
}

// --- a folder subtree filter never reaches the index -------------------

/// `sql_string_literal` refuses `%`, so the `folder LIKE 'x/%'` branch can
/// never be built and a subtree search silently collapses to an exact match.
#[test]
fn search_folder_subtree_reaches_below_the_folder() -> TestResult {
    let (_project, service) = service()?;
    let mut envelope = Envelope::new("deploy", "note", "rollout runbook")?;
    envelope.folder = Some("guides/release".to_owned());
    seed(
        &service,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;

    let result = service.search(&SearchRequest {
        query: "runbook".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters {
            folder: Some("guides".to_owned()),
            folder_subtree: true,
            ..SearchFilters::default()
        },
        revision: service.current_revision()?,
    })?;
    assert!(
        result.hits.iter().any(|hit| hit.id == "deploy"),
        "a subtree filter reaches records below the folder it names: {:?}",
        result.hits.iter().map(|hit| &hit.id).collect::<Vec<_>>()
    );
    Ok(())
}

/// A folder name the SQL-literal rule refuses — anything non-ASCII — drops the
/// predicate entirely instead of being applied in memory the way `kind` is, so
/// the filter silently selects the whole corpus.
#[test]
fn a_folder_filter_the_predicate_cannot_carry_is_still_applied() -> TestResult {
    let (_project, service) = service()?;
    let mut filed = Envelope::new("filed", "note", "rollout runbook")?;
    filed.folder = Some("архитектура".to_owned());
    let unfiled = Envelope::new("unfiled", "note", "rollout runbook elsewhere")?;
    seed(
        &service,
        vec![
            Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(filed),
            }),
            Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(unfiled),
            }),
        ],
    )?;

    let result = service.search(&SearchRequest {
        query: "runbook".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters {
            folder: Some("архитектура".to_owned()),
            ..SearchFilters::default()
        },
        revision: service.current_revision()?,
    })?;
    assert!(
        result.hits.iter().all(|hit| hit.id == "filed"),
        "a folder filter narrows the result even when it cannot be a SQL literal: {:?}",
        result.hits.iter().map(|hit| &hit.id).collect::<Vec<_>>()
    );
    Ok(())
}

// --- a derived key silently overwrites an unrelated record -------------

/// A key for a document new to Memory is derived from its locator. Nothing
/// checks the key is free, so a document whose slug matches an existing
/// record's key replaces that record — a different kind, a different body, and
/// no warning.
#[test]
fn a_new_document_does_not_overwrite_an_existing_record() -> TestResult {
    let (project, service) = attached_project()?;
    let expected = service.current_revision()?;
    service.apply_transaction(
        "existing-note",
        expected,
        vec![put("guide", "note", "a decision worth keeping")?],
    )?;

    write_doc(project.path(), "guide.md", "# Guide\n")?;
    commit_all(project.path(), "add guide")?;
    let report = service.scan_attachments("scan")?;
    assert_eq!(report.applied, 1);

    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(
        envelope.kind, "note",
        "the note that was there first is still there: {envelope:?}"
    );
    Ok(())
}

// --- changing an attachment's folder is treated as an ordinary edit -----

/// The guard refuses a type edit that would leave records behind, and compares
/// only the storage *place*. Moving the same type from one folder to another
/// leaves every record pointing at the old path, and the migration operation
/// declines to help because the place did not change.
#[test]
fn moving_an_attachment_to_another_folder_is_not_a_silent_edit() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    commit_all(project.path(), "add guide")?;
    service.scan_attachments("scan")?;

    let moved = json!({
        "kind_name": "doc",
        "storage": "handbook"
    });
    let expected = service.current_revision()?;
    let result = service.apply_transaction(
        "retarget",
        expected,
        vec![put(
            &type_key("doc"),
            "__type__",
            &serde_json::to_string(&moved)?,
        )?],
    );
    assert!(
        result.is_err(),
        "pointing a type at another folder leaves every record behind — that is a migration"
    );
    Ok(())
}

// --- a migration may not publish two records to one file ----------------

/// A published document is the record's key under the folder, and keys are
/// unique, so two records cannot land on one path. Asserted rather than
/// assumed: the last write would win and every record but one would be lost,
/// while all of them went on pointing at the same locator.
#[test]
fn migration_publishes_each_record_to_its_own_file() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![
            put(
                &type_key("doc"),
                "__type__",
                &serde_json::to_string(&json!({"kind_name": "doc"}))?,
            )?,
            put("first", "doc", "the first body")?,
            put("second", "doc", "the second body")?,
        ],
    )?;

    let target = Some("docs");
    let outcome = service.migrate_storage(
        "collapse",
        "doc",
        target,
        &["content_becomes_visible".to_owned()],
    );
    if outcome.is_err() {
        return Ok(());
    }
    let first = envelope_of(&service, "first")?;
    let second = envelope_of(&service, "second")?;
    let first_path = first.content_ref.as_ref().map(|r| r.path.clone());
    let second_path = second.content_ref.as_ref().map(|r| r.path.clone());
    assert_ne!(
        first_path, second_path,
        "two records may not be published to one file: {first_path:?} / {second_path:?}"
    );
    let _ = project;
    Ok(())
}

// --- a record key steers a write outside the repository -------------

/// Migration builds the locator by substituting the record's key into the
/// mask. Keys are arbitrary non-empty strings, and the file is written before
/// anything validates the locator, so `../` in a key escapes the repository.
#[test]
fn a_key_cannot_steer_a_migration_write_outside_the_repository() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![
            put(
                &type_key("doc"),
                "__type__",
                &serde_json::to_string(&json!({"kind_name": "doc"}))?,
            )?,
            put("../../escaped", "doc", "written outside the repository")?,
        ],
    )?;

    let target = Some("docs");
    let _ = service.migrate_storage(
        "escape",
        "doc",
        target,
        &["content_becomes_visible".to_owned()],
    );

    let escaped = project.path().join("docs/../../escaped.md");
    let existed = escaped.exists();
    if existed {
        std::fs::remove_file(&escaped)?;
    }
    assert!(
        !existed,
        "nothing may be written outside the project root: {}",
        escaped.display()
    );
    Ok(())
}

// --- the folder walk follows symbolic links ----------------------------

/// `collect` recurses on anything `is_dir()` says is a directory, which follows
/// a symbolic link. A link inside the attachment pointing outside the
/// repository pulls files nobody attached into the corpus; a link pointing at
/// an ancestor is an unbounded recursion.
#[cfg(unix)]
#[test]
fn the_folder_walk_does_not_follow_a_link_out_of_the_repository() -> TestResult {
    let (project, service) = attached_project()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(
        outside.path().join("secret.md"),
        "not part of this repository\n",
    )?;
    std::os::unix::fs::symlink(outside.path(), project.path().join("docs/linked"))?;

    let report = service.scan_attachments("scan")?;
    assert_eq!(
        report.scanned, 0,
        "a symlinked directory is not part of the attachment: {:?}",
        report.changes
    );
    Ok(())
}

// --- `Removed` is hidden from listing as well as `NotOnBranch` ---------

/// The branch spec hides "this branch does not have the document". A file
/// deleted on the branch that owns it is the other case — the one carried out
/// to a person — and hiding it too means the person is asked about a record
/// they can no longer see.
#[test]
fn a_removed_document_is_still_listed() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    commit_all(project.path(), "add guide")?;
    service.scan_attachments("scan")?;

    std::fs::remove_file(project.path().join("docs/guide.md"))?;
    service.scan_attachments("rescan")?;

    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(
        envelope.content_ref.as_ref().map(|r| r.presence.as_str()),
        Some("removed")
    );

    let listing = service.list_records(&ListingQuery::default(), None)?;
    assert!(
        listing.records.iter().any(|(key, _)| key == "guide"),
        "a deliberate deletion on this branch stays visible until somebody decides"
    );
    Ok(())
}

// --- a type's own field rules contradict the reference envelope --------

/// A document type that requires content is the obvious thing to declare, and
/// a reference record's `content` is empty by contract. Nothing in the schema
/// layer knows the difference, so every scanned document fails validation and
/// the whole scan transaction is refused.
#[test]
fn a_document_type_may_require_content() -> TestResult {
    let (project, service) = service()?;
    std::fs::create_dir_all(project.path().join("docs"))?;
    seed(
        &service,
        vec![put(
            &type_key("doc"),
            "__type__",
            &serde_json::to_string(&json!({
                "kind_name": "doc",
                "envelope": {"content": {"required": true}},
                "storage": "docs"
            }))?,
        )?],
    )?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    commit_all(project.path(), "add guide")?;

    let report = service.scan_attachments("scan")?;
    assert_eq!(
        report.applied, 1,
        "a document type may say its records have content and still be attachable"
    );
    Ok(())
}

// --- the file mask backtracks exponentially ----------------------------

// --- two documents can want the same derived key -----------------------

/// A key is slugged from the locator: every character that is not
/// `[a-z0-9_-]` becomes `-`. Ordinary documentation file names collide under
/// that rule, and two `New` conclusions with one key make a transaction that
/// touches a record twice — so the scan is refused outright and the folder is
/// never reconciled again.
#[test]
fn two_documents_whose_names_slug_alike_can_both_be_scanned() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "getting started.md", "# Getting started\n")?;
    write_doc(
        project.path(),
        "getting-started.md",
        "# Getting started too\n",
    )?;
    commit_all(project.path(), "add both")?;

    let report = service.scan_attachments("scan")?;
    assert_eq!(
        report.applied, 2,
        "two documents are two records, whatever their names slug to"
    );
    Ok(())
}

// --- a document that is not text is still a document ----------------------

/// A documentation folder holds diagrams and PDFs next to the Markdown. They
/// move, they are edited, they go missing, and a scan has to see all of that.
/// What cannot be done with them is full-text search — so the index records
/// that the content is binary rather than leaving the record looking like an
/// empty document.
#[test]
fn a_binary_document_is_scanned_and_indexed_as_binary() -> TestResult {
    // With vectors, because a binary document has no text: its body is empty
    // and a scanned document has no title, so neither BM25 nor the substring
    // pass can reach it. What the index recorded about it is only observable
    // through the meaning channel.
    let (project, service) = attached_project_with_vectors()?;
    std::fs::write(
        project.path().join("docs/diagram.md"),
        [0x00_u8, 0xff, 0xfe, 0x01, 0x02],
    )?;
    write_doc(project.path(), "guide.md", "# Guide\n\nordinary prose\n")?;
    commit_all(project.path(), "add both")?;

    let report = service.scan_attachments("scan")?;
    assert_eq!(
        report.scanned, 2,
        "a binary file is one of this type's documents"
    );
    assert_eq!(report.applied, 2);

    // Editing it is noticed the same way an edit to prose is.
    std::fs::write(
        project.path().join("docs/diagram.md"),
        [0x00_u8, 0xff, 0x03],
    )?;
    let second = service.scan_attachments("rescan")?;
    assert!(
        second
            .changes
            .iter()
            .any(|change| matches!(change, ScanChange::Edited { key, .. } if key == "diagram")),
        "an edit to a binary document is an edit: {:?}",
        second.changes
    );

    let result = service.search(&SearchRequest {
        query: "prose".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters::default(),
        revision: service.current_revision()?,
    })?;
    let binary = result.hits.iter().find(|hit| hit.id == "diagram");
    assert_eq!(
        binary.and_then(|hit| hit.content_kind.as_deref()),
        Some("binary"),
        "the index says what it could not read, rather than an empty body"
    );
    assert_eq!(
        result
            .hits
            .iter()
            .find(|hit| hit.id == "guide")
            .and_then(|hit| hit.content_kind.as_deref()),
        Some("text")
    );
    Ok(())
}

// --- a record of a folder type points at its content ----------------------

/// A type stored in a repository folder is a reference type by construction. A
/// record of that kind carrying its content would live in refs with no file
/// anywhere, invisible to the scan meant to keep it honest.
#[test]
fn a_record_of_a_folder_type_cannot_carry_its_own_content() -> TestResult {
    let (_project, service) = attached_project()?;
    let expected = service.current_revision()?;
    let error = service
        .apply_transaction(
            "inline",
            expected,
            vec![put("invented", "doc", "content with nowhere to live")?],
        )
        .expect_err("a folder type's record points at a file");
    assert_eq!(error.kind, "invalid_record");
    Ok(())
}
