#![allow(clippy::expect_used, clippy::unwrap_used)]

//! The use cases, exercised without a process boundary.
//!
//! This is the point of the crate: before the split, the only way to test what
//! `memory_list_records` or `memory_import` actually do was to spawn the MCP
//! server, write JSON to its stdin and parse what came back. These tests call
//! the same logic directly.

use std::path::Path;

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{SearchFilters, SearchRequest};
use memory_hub_service::{ListingQuery, ListingSort, MemoryService, RecordsIn};
use memory_hub_store::{ExportMode, Operation, RecordId, Revision};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn service() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let service = MemoryService::open(project.path().to_path_buf(), RecordsIn::GitMetadata);
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
    let result = service.apply_transaction("seed", expected, operations)?;
    Ok(result.revision)
}

#[test]
fn a_written_record_is_readable_at_the_staged_revision() -> TestResult {
    let (_project, service) = service()?;
    let revision = seed(&service, vec![put("note-1", "note", "first body")?])?;

    // No checkpoint in between: reads serve the staged revision, which is the
    // whole point of the revision decision behind SYNC-2.
    let view = service.get_record("note-1", None)?;
    assert_eq!(view.revision, revision);
    let Some(StoredRecord::Plaintext { envelope }) = view.record else {
        return Err("expected a plaintext record".into());
    };
    assert_eq!(envelope.content, "first body");
    Ok(())
}

#[test]
fn a_transaction_needs_at_least_one_operation() -> TestResult {
    let (_project, service) = service()?;
    let expected = service.current_revision()?;
    let error = service
        .apply_transaction("empty", expected, Vec::new())
        .expect_err("an empty batch is rejected");
    assert_eq!(error.kind, "invalid_argument");
    Ok(())
}

#[test]
fn a_stale_expected_revision_is_a_conflict_not_an_overwrite() -> TestResult {
    let (_project, service) = service()?;
    let before = service.current_revision()?;
    seed(&service, vec![put("note-1", "note", "first")?])?;

    // Same key, same revision the first writer saw: the second write must not
    // silently win.
    let error = service
        .apply_transaction("second", before, vec![put("note-1", "note", "second")?])
        .expect_err("a same-key write against a stale revision conflicts");
    assert_eq!(error.kind, "conflict");
    Ok(())
}

#[test]
fn listing_filters_sorts_and_pages_over_the_whole_corpus() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            put("b-note", "note", "beta")?,
            put("a-note", "note", "alpha")?,
            put("c-spec", "spec", "gamma")?,
        ],
    )?;

    let notes = service.list_records(
        &ListingQuery {
            kind: Some("note".to_owned()),
            ..ListingQuery::default()
        },
        None,
    )?;
    assert_eq!(notes.total, 2, "only the notes match");
    assert_eq!(notes.records[0].0, "a-note", "sorted by key by default");
    assert_eq!(notes.counts.by_kind.get("spec"), None);

    let first_page = service.list_records(
        &ListingQuery {
            sort: ListingSort::Key,
            descending: true,
            ..ListingQuery::default()
        }
        .with_limit(1),
        None,
    )?;
    assert_eq!(first_page.records[0].0, "c-spec", "descending by key");
    assert!(first_page.has_more, "two more records remain");
    assert_eq!(
        first_page.counts.total, 3,
        "counts describe the whole selection, not the page"
    );
    Ok(())
}

#[test]
fn export_and_import_round_trip_a_corpus() -> TestResult {
    let (_source_dir, source) = service()?;
    seed(
        &source,
        vec![
            put("note-1", "note", "carry me")?,
            put("note-2", "note", "and me")?,
        ],
    )?;
    let bundle = source
        .export(&source.current_revision()?, ExportMode::Manifest)?
        .bundle;

    let (_target_dir, target) = service()?;
    let expected = target.current_revision()?;
    target.import("restore", expected, &bundle)?;

    let listing = target.list_records(&ListingQuery::default(), None)?;
    assert_eq!(listing.total, 2, "both records landed");
    let view = target.get_record("note-1", None)?;
    let Some(StoredRecord::Plaintext { envelope }) = view.record else {
        return Err("expected a plaintext record".into());
    };
    assert_eq!(envelope.content, "carry me");
    Ok(())
}

#[test]
fn import_rejects_a_bundle_from_a_future_schema() -> TestResult {
    let (_project, service) = service()?;
    let mut bundle = service
        .export(&service.current_revision()?, ExportMode::Manifest)?
        .bundle;
    bundle.schema_version += 1;

    let expected = service.current_revision()?;
    let error = service
        .import("restore", expected, &bundle)
        .expect_err("an unknown bundle version is refused");
    assert_eq!(error.kind, "invalid_argument");
    Ok(())
}

#[test]
fn search_finds_a_record_and_backlinks_find_who_points_at_it() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            put("target", "note", "the referenced note")?,
            put("source", "note", "this one mentions target in its body")?,
        ],
    )?;

    let result = service.search(&SearchRequest {
        query: "referenced".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters::default(),
        revision: service.current_revision()?,
    })?;
    // The count is deliberately not asserted: with an embedding model on disk
    // the vector channel adds semantically close records that BM25 alone would
    // not return, so a fixed number would pass or fail depending on the machine.
    // What must hold either way is which record ranks first.
    assert_eq!(result.hits[0].id, "target");

    let backlinks = service.backlinks("target", None)?;
    assert_eq!(backlinks.entries.len(), 1, "the mention is an inbound link");
    Ok(())
}

#[test]
fn deleting_a_record_removes_it_from_the_corpus() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![put("note-1", "note", "temporary")?])?;

    let expected = service.current_revision()?;
    service.apply_transaction(
        "drop",
        expected,
        vec![Operation::delete(RecordId::plaintext("note-1"))],
    )?;

    let view = service.get_record("note-1", None)?;
    assert!(view.record.is_none(), "the record is gone");
    Ok(())
}

// --- Backend routing (GITMEMO-24) -----------------------------------------

use memory_hub_schema::type_key;
use serde_json::{Value, json};

/// Declare a document type, optionally saying where its records live.
fn declare_type(
    kind_name: &str,
    storage: Option<&str>,
) -> Result<Operation, Box<dyn std::error::Error>> {
    let mut definition = json!({"kind_name": kind_name});
    if let Some(storage) = storage {
        definition["storage"] = json!(storage);
    }
    put(
        &type_key(kind_name),
        "__type__",
        &serde_json::to_string(&definition)?,
    )
}

/// Two types: one keeping its content in its records, one whose content is
/// files in the working tree.
fn service_with_external_content()
-> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    let (project, service) = service()?;
    seed(
        &service,
        vec![
            declare_type("note", None)?,
            declare_type("doc", Some("docs"))?,
        ],
    )?;
    Ok((project, service))
}

/// A delete names a key, not a kind. Where the record's content lives makes no
/// difference to removing the record — the envelope is in the same storage
/// either way.
#[test]
fn a_delete_removes_a_record_whatever_holds_its_content() -> TestResult {
    let (_project, service) = service_with_external_content()?;
    let expected = service.current_revision()?;
    service.apply_transaction("write", expected, vec![put("note-1", "note", "body")?])?;

    let expected = service.current_revision()?;
    service.apply_transaction(
        "remove",
        expected,
        vec![Operation::delete(RecordId::plaintext("note-1"))],
    )?;

    assert!(service.get_record("note-1", None)?.record.is_none());
    Ok(())
}

/// Corpus operations answer from the reference data Memory holds, so content
/// that is not reachable cannot make one of them fail or quietly return less.
/// Only fetching a body reaches outside.
#[test]
fn corpus_operations_do_not_depend_on_a_reachable_backend() -> TestResult {
    let (_project, service) = service_with_external_content()?;
    let expected = service.current_revision()?;
    service.apply_transaction("write", expected, vec![put("note-1", "note", "body")?])?;
    // A record whose content is a file nobody has here.
    let expected = service.current_revision()?;
    service.apply_transaction(
        "attach",
        expected,
        vec![reference_record("guide", "docs/guide.md", "last seen")?],
    )?;

    let listing = service.list_records(&ListingQuery::default(), None)?;
    assert!(
        listing.records.iter().any(|(key, _)| key == "note-1"),
        "listing answers over what Memory holds"
    );

    service.export(&service.current_revision()?, ExportMode::Manifest)?;
    assert!(service.records_summary()?.counts.total >= 1);
    assert!(
        matches!(
            service.resolve_content("guide")?,
            ContentResolution::Missing { .. }
        ),
        "reading the body is the one thing that reaches outside, and it says so"
    );
    Ok(())
}

// --- Reference records and per-record agreement (GITMEMO-25) ---------------

use memory_hub_core::{ContentHash, ContentRef};
use memory_hub_service::ContentResolution;

fn reference_record(
    key: &str,
    path: &str,
    last_known: &str,
) -> Result<Operation, Box<dyn std::error::Error>> {
    Ok(Operation::put(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::reference(
            key,
            "doc",
            path,
            ContentHash::for_content(last_known),
        )?),
    }))
}

/// A revision agrees on the whole store. When the content belongs to somebody
/// else there is no past state to pin, so agreement is per record, by digest.
#[test]
fn a_conditional_write_applies_only_while_the_content_it_was_based_on_stands() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![put("note-1", "note", "first")?])?;

    let based_on = ContentHash::for_content("first");
    let expected = service.current_revision()?;
    service.apply_transaction(
        "conditional",
        expected,
        vec![Operation::put_if_unchanged(
            StoredRecord::Plaintext {
                envelope: Box::new(Envelope::new("note-1", "note", "second")?),
            },
            based_on.clone(),
        )],
    )?;

    // The content moved on; a write still based on the old digest is refused.
    let expected = service.current_revision()?;
    let error = service
        .apply_transaction(
            "stale",
            expected,
            vec![Operation::put_if_unchanged(
                StoredRecord::Plaintext {
                    envelope: Box::new(Envelope::new("note-1", "note", "third")?),
                },
                based_on.clone(),
            )],
        )
        .expect_err("the content it was based on is gone");
    assert_eq!(error.kind, "conflict");
    assert_eq!(error.data["key"], "note-1");
    assert_eq!(error.data["expected_content_hash"], based_on.as_str());
    assert_eq!(
        error.data["actual_content_hash"],
        ContentHash::for_content("second").as_str()
    );
    Ok(())
}

/// A create is not an overwrite: there is nothing to disagree with, and
/// concurrent creates are the revision check's business.
#[test]
fn a_conditional_write_of_a_record_that_is_not_there_yet_applies() -> TestResult {
    let (_project, service) = service()?;
    let expected = service.current_revision()?;
    service.apply_transaction(
        "create",
        expected,
        vec![Operation::put_if_unchanged(
            StoredRecord::Plaintext {
                envelope: Box::new(Envelope::new("note-1", "note", "body")?),
            },
            ContentHash::for_content("something nobody stored"),
        )],
    )?;
    assert!(service.get_record("note-1", None)?.record.is_some());
    Ok(())
}

/// Deleted, on another branch, or simply not pulled — indistinguishable at
/// this moment. The record stays, its links stay live, and the caller is told
/// the body is not here.
#[test]
fn an_unresolvable_locator_is_missing_and_nothing_else_breaks() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![reference_record("guide", "docs/guide.md", "never written")?],
    )?;

    let ContentResolution::Missing { path, .. } = service.resolve_content("guide")? else {
        return Err("a locator pointing at nothing resolves to Missing".into());
    };
    assert_eq!(path, "docs/guide.md");

    let listing = service.list_records(&ListingQuery::default(), None)?;
    assert!(
        listing.records.iter().any(|(key, _)| key == "guide"),
        "the record is still in the corpus"
    );
    Ok(())
}

#[test]
fn a_resolved_locator_reports_whether_the_content_moved_on() -> TestResult {
    let (project, service) = service()?;
    std::fs::create_dir_all(project.path().join("docs"))?;
    std::fs::write(
        project.path().join("docs/guide.md"),
        "as Memory last saw it",
    )?;
    seed(
        &service,
        vec![reference_record(
            "guide",
            "docs/guide.md",
            "as Memory last saw it",
        )?],
    )?;

    let ContentResolution::Resolved {
        content, changed, ..
    } = service.resolve_content("guide")?
    else {
        return Err("the file is right there".into());
    };
    assert_eq!(content.as_text(), Some("as Memory last saw it"));
    assert!(!changed, "nobody has touched it");

    std::fs::write(project.path().join("docs/guide.md"), "somebody edited it")?;
    let ContentResolution::Resolved { changed, .. } = service.resolve_content("guide")? else {
        return Err("still right there".into());
    };
    assert!(changed, "the digest no longer matches what is on disk");
    Ok(())
}

/// Content first, record second. An interruption then leaves a file that
/// disagrees with the digest — visible to the next scan — rather than a digest
/// for content that was never written.
#[test]
fn writing_reference_content_writes_the_file_before_the_record() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![
            // The type says where its content lives; writing a body goes
            // through that storage rather than at a path pulled from the
            // record.
            declare_type("doc", Some("docs"))?,
            reference_record("guide", "docs/guide.md", "")?,
        ],
    )?;

    service.write_reference_content("publish", "guide", b"the published body")?;

    let on_disk = std::fs::read_to_string(project.path().join("docs/guide.md"))?;
    assert_eq!(on_disk, "the published body");

    let ContentResolution::Resolved { changed, .. } = service.resolve_content("guide")? else {
        return Err("the file was written".into());
    };
    assert!(
        !changed,
        "the record's digest was updated to what was written"
    );
    Ok(())
}

/// Two different requests, not two opinions about one — and the answer is in
/// the bundle, so an importer reads it instead of inferring it.
#[test]
fn export_modes_differ_only_for_records_whose_content_is_outside() -> TestResult {
    let (project, service) = service()?;
    std::fs::create_dir_all(project.path().join("docs"))?;
    std::fs::write(project.path().join("docs/guide.md"), "the real body")?;
    seed(
        &service,
        vec![
            put("note-1", "note", "inline body")?,
            reference_record("guide", "docs/guide.md", "the real body")?,
        ],
    )?;
    let revision = service.current_revision()?;

    let manifest = service.export(&revision, ExportMode::Manifest)?.bundle;
    assert_eq!(manifest.mode, ExportMode::Manifest);
    let guide = record_named(&manifest, "guide");
    assert_eq!(
        guide.content_ref,
        Some(ContentRef::new("docs/guide.md")),
        "a manifest keeps the locator"
    );
    assert!(guide.content.is_empty());

    let snapshot = service.export(&revision, ExportMode::Snapshot)?.bundle;
    assert_eq!(snapshot.mode, ExportMode::Snapshot);
    let guide = record_named(&snapshot, "guide");
    assert_eq!(
        guide.content, "the real body",
        "a snapshot carries what the locator resolved to"
    );
    assert_eq!(guide.content_ref, None);

    assert_eq!(
        record_named(&manifest, "note-1"),
        record_named(&snapshot, "note-1"),
        "a record whose content is its own is untouched by the mode"
    );
    Ok(())
}

/// A snapshot carries what it could read. One document somebody moved does not
/// fail the export, and no empty body is invented for it.
#[test]
fn a_snapshot_leaves_a_locator_it_could_not_resolve_alone() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![reference_record("guide", "docs/guide.md", "last seen")?],
    )?;

    let bundle = service
        .export(&service.current_revision()?, ExportMode::Snapshot)?
        .bundle;
    let guide = record_named(&bundle, "guide");
    assert_eq!(guide.content_ref, Some(ContentRef::new("docs/guide.md")));
    assert!(guide.content.is_empty());
    Ok(())
}

fn record_named(bundle: &memory_hub_store::ExportBundle, key: &str) -> Envelope {
    bundle
        .records
        .iter()
        .find_map(|(id, record)| match record {
            StoredRecord::Plaintext { envelope } if id.display_value() == key => {
                Some((**envelope).clone())
            }
            StoredRecord::Plaintext { .. } => None,
        })
        .expect("the bundle carries that record")
}

/// A bundle written before the mode field existed is a manifest by
/// construction — nothing could reference anything outside itself yet — so it
/// still imports, and imports as what it is.
#[test]
fn a_bundle_from_before_the_mode_field_imports_as_a_manifest() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![put("note-1", "note", "carry me")?])?;
    let bundle = service
        .export(&service.current_revision()?, ExportMode::Manifest)?
        .bundle;

    let mut wire = serde_json::to_value(&bundle)?;
    wire["schema_version"] = serde_json::json!(1);
    wire.as_object_mut()
        .ok_or("bundle is an object")?
        .remove("mode");
    let older: memory_hub_store::ExportBundle = serde_json::from_value(wire)?;
    assert_eq!(older.mode, ExportMode::Manifest);

    let (_target_dir, target) = fresh_service()?;
    let expected = target.current_revision()?;
    target.import("restore", expected, &older)?;
    assert_eq!(
        target.list_records(&ListingQuery::default(), None)?.total,
        1
    );
    Ok(())
}

/// `service` is shadowed by the binding above; this is the same helper under a
/// name the test can still reach.
fn fresh_service() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    service()
}

// --- Attached repository folder (GITMEMO-26) -------------------------------

use memory_hub_service::{PresenceFilter, ScanChange, Unresolved};

/// A type whose content is ordinary repository files: Git versions them, a
/// pull request shows them in its diff, and Memory writes nothing into them.
fn attached_project() -> Result<(tempfile::TempDir, MemoryService), Box<dyn std::error::Error>> {
    let (project, service) = service()?;
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
            // Something that stays in refs, so a test can point at a document
            // from outside the attachment.
            put(
                &type_key("note"),
                "__type__",
                &serde_json::to_string(&json!({"kind_name": "note"}))?,
            )?,
        ],
    )?;
    Ok((project, service))
}

/// Commit everything in the tree, so `HEAD` owns the documents.
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

/// A commit, so `HEAD` exists and moves.
fn commit_file(project: &Path, name: &str) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::write(project.join(name), name)?;
    let repository = Repository::open(project)?;
    let mut index = repository.index()?;
    index.add_path(std::path::Path::new(name))?;
    index.write()?;
    let tree = repository.find_tree(index.write_tree()?)?;
    let signature = git2::Signature::now("Test", "test@example.invalid")?;
    let parent = repository
        .head()
        .ok()
        .and_then(|head| head.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repository.commit(Some("HEAD"), &signature, &signature, name, &tree, &parents)?;
    Ok(())
}

fn write_doc(project: &Path, name: &str, body: &str) -> std::io::Result<()> {
    std::fs::write(project.join("docs").join(name), body)
}

fn snapshot_of_folder(project: &Path) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(project.join("docs"))? {
        let entry = entry?;
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read_to_string(entry.path())?,
        ));
    }
    entries.sort();
    Ok(entries)
}

/// The whole promise: a colleague who has never heard of Memory sees a
/// repository that has not changed. Not a marker, not an id, not a byte.
#[test]
fn attaching_a_folder_changes_no_file_in_it() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    write_doc(project.path(), "setup.md", "# Setup\n")?;
    let before = snapshot_of_folder(project.path())?;

    let report = service.scan_attachments("attach")?;
    assert_eq!(report.scanned, 2);
    assert_eq!(report.applied, 2);

    assert_eq!(
        snapshot_of_folder(project.path())?,
        before,
        "the working tree is untouched"
    );
    let listing = service.list_records(&ListingQuery::default(), None)?;
    assert!(listing.records.iter().any(|(key, _)| key == "guide"));
    assert!(listing.records.iter().any(|(key, _)| key == "setup"));
    Ok(())
}

/// Membership is decided by where a file is, and nothing else.
///
/// There used to be a file-name mask here, and dropping it is deliberate: a
/// person who puts a `.txt` next to their Markdown has not made a mistake, and
/// a corpus that silently ignored it would show them a project that is not
/// theirs. What we cannot render is a question for the viewer.
#[test]
fn membership_is_decided_by_the_folder_alone() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    write_doc(
        project.path(),
        "notes.txt",
        "plain text, still a document\n",
    )?;
    std::fs::create_dir_all(project.path().join("elsewhere"))?;
    std::fs::write(project.path().join("elsewhere/stray.md"), "# Outside\n")?;

    let report = service.scan_attachments("attach")?;
    assert_eq!(
        report.scanned, 2,
        "both files in the folder are this storage's documents"
    );
    assert_eq!(report.applied, 2);
    Ok(())
}

/// Same path, different bytes. The claim was checked against one text; the
/// text changed, so the check no longer says anything.
#[test]
fn an_edit_in_place_is_recognised_and_resets_freshness() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "first draft\n")?;
    service.scan_attachments("attach")?;

    // Somebody vouched for the record, then somebody else edited the file.
    let expected = service.current_revision()?;
    let mut envelope = envelope_of(&service, "guide")?;
    envelope.freshness.state = memory_hub_core::FreshnessState::Fresh;
    service.apply_transaction(
        "vouch",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;
    write_doc(project.path(), "guide.md", "second draft\n")?;

    let report = service.scan_attachments("rescan")?;
    assert!(
        report
            .changes
            .iter()
            .any(|change| matches!(change, ScanChange::Edited { key, .. } if key == "guide"))
    );
    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(
        envelope.content_hash,
        ContentHash::for_content("second draft\n")
    );
    assert_eq!(
        envelope.freshness.state,
        memory_hub_core::FreshnessState::Unverified,
        "a text that changed under a verified claim un-verifies it"
    );
    Ok(())
}

/// Same bytes at another path. The key is a birth name, not an address, so a
/// rename moves the locator and leaves every link pointing at it alone.
#[test]
fn a_rename_keeps_the_record_its_key_and_its_metadata() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "unchanged body\n")?;
    service.scan_attachments("attach")?;

    let expected = service.current_revision()?;
    let mut envelope = envelope_of(&service, "guide")?;
    envelope.title = Some("The guide".to_owned());
    envelope.tags = vec!["onboarding".to_owned()];
    service.apply_transaction(
        "annotate",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;

    std::fs::rename(
        project.path().join("docs/guide.md"),
        project.path().join("docs/handbook.md"),
    )?;
    let report = service.scan_attachments("rescan")?;
    assert!(report.changes.iter().any(|change| matches!(
        change,
        ScanChange::Moved { key, to, .. } if key == "guide" && to == "docs/handbook.md"
    )));

    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(
        envelope.content_ref.as_ref().map(|r| r.path.as_str()),
        Some("docs/handbook.md")
    );
    assert_eq!(envelope.title.as_deref(), Some("The guide"));
    assert_eq!(envelope.tags, vec!["onboarding".to_owned()]);
    Ok(())
}

/// A rename with an edit and a brand-new file are the same thing from here.
/// Choosing silently would either lose a record's history or invent a
/// relationship nobody made.
#[test]
fn a_rename_with_an_edit_is_carried_out_to_a_person() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "the original body\n")?;
    service.scan_attachments("attach")?;

    std::fs::remove_file(project.path().join("docs/guide.md"))?;
    write_doc(project.path(), "guide-v2.md", "the original body, edited\n")?;

    let report = service.scan_attachments("rescan")?;
    let Some(ScanChange::Unmatched {
        locator,
        candidates,
        ..
    }) = report
        .changes
        .iter()
        .find(|change| matches!(change, ScanChange::Unmatched { .. }))
    else {
        return Err("the stray file is ambiguous, not decided".into());
    };
    assert_eq!(locator, "docs/guide-v2.md");
    assert_eq!(candidates.first().map(|c| c.key.as_str()), Some("guide"));
    assert!(
        candidates[0].similarity > 0.5,
        "the names are close enough to rank first: {}",
        candidates[0].similarity
    );

    assert_eq!(
        envelope_of(&service, "guide")?
            .content_ref
            .as_ref()
            .map(|r| r.path.as_str()),
        Some("docs/guide.md"),
        "nothing was decided on the record's behalf"
    );
    Ok(())
}

/// Deleted, on another branch, or not pulled — indistinguishable at the moment
/// of looking, and two of the three are routine.
#[test]
fn a_vanished_file_marks_the_record_missing_and_never_deletes_it() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "body\n")?;
    service.scan_attachments("attach")?;

    let expected = service.current_revision()?;
    let mut envelope = envelope_of(&service, "guide")?;
    envelope.title = Some("The guide".to_owned());
    service.apply_transaction(
        "annotate",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;

    std::fs::remove_file(project.path().join("docs/guide.md"))?;
    let report = service.scan_attachments("gone")?;
    assert!(
        report
            .changes
            .iter()
            .any(|change| matches!(change, ScanChange::Missing { key, .. } if key == "guide"))
    );
    let envelope = envelope_of(&service, "guide")?;
    assert!(
        envelope
            .content_ref
            .as_ref()
            .is_some_and(|r| r.presence.is_absent())
    );
    assert_eq!(envelope.title.as_deref(), Some("The guide"));

    // And it comes back with everything it had — the branch-switch case.
    write_doc(project.path(), "guide.md", "body\n")?;
    let report = service.scan_attachments("back")?;
    assert!(
        report
            .changes
            .iter()
            .any(|change| matches!(change, ScanChange::Returned { key, .. } if key == "guide"))
    );
    let envelope = envelope_of(&service, "guide")?;
    assert!(
        envelope
            .content_ref
            .as_ref()
            .is_some_and(|r| r.presence.is_present())
    );
    assert_eq!(envelope.title.as_deref(), Some("The guide"));
    Ok(())
}

/// Switching to a branch without the folder at all is the same case, at the
/// scale of every record at once.
#[test]
fn a_branch_without_the_folder_leaves_every_record_intact() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "body\n")?;
    write_doc(project.path(), "setup.md", "other body\n")?;
    service.scan_attachments("attach")?;

    std::fs::remove_dir_all(project.path().join("docs"))?;
    let report = service.scan_attachments("switched")?;
    assert_eq!(report.scanned, 0);
    assert_eq!(report.applied, 2, "both records report their file is gone");

    let listing = service.list_records(&ListingQuery::default(), None)?;
    assert_eq!(
        listing.total, 0,
        "nothing is listed: the documents are elsewhere and schema is not a document"
    );
    assert_eq!(
        listing.counts.service, 0,
        "schema was not asked for, so none of it is in the selection"
    );
    let everything = ListingQuery {
        presence: PresenceFilter::Any,
        ..ListingQuery::default()
    };
    assert_eq!(
        service.list_records(&everything, None)?.total,
        2,
        "hidden is not deleted: both documents are still in the corpus"
    );
    let schema = ListingQuery {
        kind: Some("__type__".to_owned()),
        ..ListingQuery::default()
    };
    let listed = service.list_records(&schema, None)?;
    assert_eq!(
        listed.total, 2,
        "asking for the kind is how the tools that maintain schema reach it"
    );
    assert_eq!(
        listed.counts.service, 2,
        "and what comes back says what it is"
    );
    Ok(())
}

fn envelope_of(service: &MemoryService, key: &str) -> Result<Envelope, Box<dyn std::error::Error>> {
    match service.get_record(key, None)?.record {
        Some(StoredRecord::Plaintext { envelope }) => Ok(*envelope),
        _ => Err(format!("no plaintext record for {key}").into()),
    }
}

/// No automation resolves an ambiguity or clears a long-standing `missing`.
/// Until the rules have been learned from use, any such automation deletes
/// data on a guess — so both states surface in `doctor` and wait for a person.
#[test]
fn doctor_reports_what_an_attached_folder_needs_a_person_for() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "the original body\n")?;
    write_doc(project.path(), "setup.md", "setup body\n")?;
    service.scan_attachments("attach")?;
    // Committed, so the branch owns them and deleting one is a deliberate act
    // rather than a branch that never had it.
    commit_all(project.path(), "documents")?;

    // One document renamed with an edit, one simply gone.
    std::fs::remove_file(project.path().join("docs/guide.md"))?;
    write_doc(project.path(), "guide-v2.md", "the original body, edited\n")?;
    std::fs::remove_file(project.path().join("docs/setup.md"))?;
    service.scan_attachments("rescan")?;

    let unresolved = service.doctor()?.attachments;
    assert!(
        unresolved.iter().any(|item| matches!(
            item,
            Unresolved::RemovedFile { key, .. } if key == "setup"
        )),
        "a document deleted on the branch that has it is a decision: {unresolved:?}"
    );
    assert!(
        unresolved.iter().any(|item| matches!(
            item,
            Unresolved::UnmatchedFile { locator, candidates, .. }
                if locator == "docs/guide-v2.md" && !candidates.is_empty()
        )),
        "the ambiguous file is carried out with its candidates: {unresolved:?}"
    );
    Ok(())
}

// --- Folders (GITMEMO-27) --------------------------------------------------

fn write_doc_at(project: &Path, relative: &str, body: &str) -> std::io::Result<()> {
    let path = project.join("docs").join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)
}

/// A documentation folder always has nested directories. Without a folder the
/// records from `guides/` and `api/` collapse into one list and attaching such
/// a directory loses its point.
#[test]
fn an_attached_tree_keeps_its_shape() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/api/auth.md", "# Auth\n")?;
    write_doc_at(project.path(), "guides/setup.md", "# Setup\n")?;
    write_doc_at(project.path(), "readme.md", "# Docs\n")?;

    let report = service.scan_attachments("attach")?;
    assert_eq!(report.scanned, 3, "the walk reaches every depth");

    // The folder is the document's own directory — the same string a person
    // sees in the file tree and in a pull request.
    assert_eq!(
        envelope_of(&service, "guides-api-auth")?.folder.as_deref(),
        Some("docs/guides/api")
    );
    assert_eq!(
        envelope_of(&service, "readme")?.folder.as_deref(),
        Some("docs")
    );
    Ok(())
}

/// With nested folders a key derived from the file name alone collides, so it
/// is derived from the whole locator below the attachment.
#[test]
fn two_documents_with_the_same_name_get_different_keys() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/api/auth.md", "# Guides\n")?;
    write_doc_at(project.path(), "cli/auth.md", "# CLI\n")?;

    service.scan_attachments("attach")?;
    assert_eq!(
        envelope_of(&service, "guides-api-auth")?.folder.as_deref(),
        Some("docs/guides/api")
    );
    assert_eq!(
        envelope_of(&service, "cli-auth")?.folder.as_deref(),
        Some("docs/cli")
    );
    Ok(())
}

/// Renaming a directory is a move of every document in it, and it costs no new
/// concept: the folder is derived from the locator, so both travel together.
/// The keys do not, which is what keeps the links intact.
#[test]
fn renaming_a_directory_moves_every_record_and_breaks_no_link() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/api/index.md", "# API\n")?;
    write_doc_at(project.path(), "guides/cli/index.md", "# API\n")?;
    service.scan_attachments("attach")?;

    // Something points at one of them by key.
    let expected = service.current_revision()?;
    let mut pointer = Envelope::new("d-uses-api", "note", "see the api page")?;
    pointer.links = vec![memory_hub_core::RecordLink {
        key: "guides-api-index".to_owned(),
        relation: None,
        extensions: std::collections::BTreeMap::default(),
    }];
    service.apply_transaction(
        "point",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(pointer),
        })],
    )?;

    std::fs::rename(
        project.path().join("docs/guides"),
        project.path().join("docs/handbook"),
    )?;
    let report = service.scan_attachments("rename")?;
    assert_eq!(
        report.applied, 2,
        "both documents moved, and nothing else did: {:?}",
        report.changes
    );

    // The two share their bytes exactly, so pairing has to come from the
    // locator: `api/index.md` must not land on `cli/index.md`.
    let api = envelope_of(&service, "guides-api-index")?;
    assert_eq!(api.folder.as_deref(), Some("docs/handbook/api"));
    assert_eq!(
        api.content_ref.as_ref().map(|r| r.path.as_str()),
        Some("docs/handbook/api/index.md")
    );
    let cli = envelope_of(&service, "guides-cli-index")?;
    assert_eq!(cli.folder.as_deref(), Some("docs/handbook/cli"));

    let backlinks = service.backlinks("guides-api-index", None)?;
    assert!(
        backlinks
            .entries
            .iter()
            .any(|entry| entry.source_id == "d-uses-api"),
        "the key never moved, so the link never broke"
    );
    Ok(())
}

/// A folder is a name, never a location, and for a reference record the name
/// is already on disk. Two versions of one fact cannot be allowed to disagree.
#[test]
fn a_reference_records_folder_may_not_disagree_with_its_locator() -> TestResult {
    let (_project, _service) = service()?;
    let mut envelope = Envelope::reference(
        "guide",
        "note",
        "docs/guides/guide.md",
        ContentHash::for_content("body"),
    )?;
    assert_eq!(envelope.folder.as_deref(), Some("docs/guides"));

    envelope.folder = Some("somewhere/else".to_owned());
    let error = envelope.validate().expect_err("one fact, one place");
    assert_eq!(error.field, "folder");
    assert!(error.message.contains("docs/guides"), "{}", error.message);
    Ok(())
}

/// Folders are implicit: one exists while a record is in it. Emptying it
/// leaves nothing behind, exactly as Git does with directories.
#[test]
fn a_folder_stops_existing_when_its_last_record_leaves() -> TestResult {
    let (_project, service) = service()?;
    let mut filed = Envelope::new("d-one", "note", "body")?;
    filed.folder = Some("architecture/storage".to_owned());
    seed(
        &service,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(filed.clone()),
        })],
    )?;

    let in_folder = |folder: &str, subtree: bool| {
        let query = ListingQuery {
            folder: Some(folder.to_owned()),
            folder_subtree: subtree,
            ..ListingQuery::default()
        };
        service
            .list_records(&query, None)
            .map(|listing| listing.total)
    };
    assert_eq!(in_folder("architecture/storage", false)?, 1);
    assert_eq!(in_folder("architecture", true)?, 1);
    assert_eq!(
        in_folder("architecture", false)?,
        0,
        "no folder records exist"
    );

    // Move it out: one field, and the folder is gone with it.
    let expected = service.current_revision()?;
    let mut moved = filed;
    moved.folder = Some("architecture/engines".to_owned());
    service.apply_transaction(
        "move",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(moved),
        })],
    )?;
    assert_eq!(in_folder("architecture/storage", true)?, 0);
    assert_eq!(in_folder("architecture/engines", false)?, 1);
    Ok(())
}

#[test]
fn the_root_is_a_folder_you_can_ask_for() -> TestResult {
    let (_project, service) = service()?;
    let mut filed = Envelope::new("d-filed", "note", "body")?;
    filed.folder = Some("architecture".to_owned());
    seed(
        &service,
        vec![
            put("d-loose", "note", "body")?,
            Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(filed),
            }),
        ],
    )?;

    let root = ListingQuery {
        folder: Some(String::new()),
        ..ListingQuery::default()
    };
    let listing = service.list_records(&root, None)?;
    assert_eq!(listing.total, 1);
    assert_eq!(listing.records[0].0, "d-loose");

    let everything = ListingQuery {
        folder: Some(String::new()),
        folder_subtree: true,
        ..ListingQuery::default()
    };
    assert_eq!(
        service.list_records(&everything, None)?.total,
        2,
        "the root and everything below it is the whole corpus"
    );
    Ok(())
}

/// A file added while an unrelated record happens to be missing is a new file,
/// not a question. Without a similarity floor the answer would depend on how
/// many branches somebody has been switching between.
#[test]
fn a_new_document_is_not_confused_with_an_unrelated_missing_one() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "feature-x.md", "# Feature X\n")?;
    service.scan_attachments("attach")?;

    // Switched away from the branch that had it.
    std::fs::remove_file(project.path().join("docs/feature-x.md"))?;
    service.scan_attachments("switch")?;

    write_doc_at(project.path(), "changelog.md", "# Changelog\n")?;
    let report = service.scan_attachments("add")?;
    assert!(
        report.changes.iter().any(|change| matches!(
            change,
            ScanChange::New { locator, .. } if locator == "docs/changelog.md"
        )),
        "an unrelated absent record is not a candidate: {:?}",
        report.changes
    );
    assert!(service.get_record("changelog", None)?.record.is_some());
    Ok(())
}

/// A scan records the commit the tree had, so a later one can tell the branch
/// moved without walking anything.
#[test]
fn a_scan_remembers_the_commit_the_tree_had() -> TestResult {
    let (project, service) = attached_project()?;
    commit_file(project.path(), "one")?;
    write_doc_at(project.path(), "guide.md", "# Guide\n")?;

    let first = service.scan_attachments("first")?;
    assert!(first.code_revision.is_some(), "HEAD is readable");

    commit_file(project.path(), "two")?;
    let second = service.scan_attachments("second")?;
    assert_eq!(second.previous_code_revision, first.code_revision);
    assert_ne!(second.code_revision, first.code_revision);
    Ok(())
}

// --- Branches (GITMEMO-32) -------------------------------------------------

fn checkout(project: &Path, branch: &str, create: bool) -> Result<(), Box<dyn std::error::Error>> {
    let repository = Repository::open(project)?;
    if create {
        let head = repository.head()?.peel_to_commit()?;
        repository.branch(branch, &head, false)?;
    }
    let reference = format!("refs/heads/{branch}");
    let object = repository.revparse_single(&reference)?;
    repository.checkout_tree(&object, Some(git2::build::CheckoutBuilder::new().force()))?;
    repository.set_head(&reference)?;
    Ok(())
}

/// The one that matters: memory does not branch and code does, so switching
/// branches must move nothing in the corpus. Real branches, real checkouts —
/// mocking `HEAD` would prove nothing about the case this exists for.
#[test]
fn switching_branches_hides_a_document_and_moves_nothing() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "shared.md", "# Shared\n")?;
    commit_all(project.path(), "shared")?;
    checkout(project.path(), "feature", true)?;
    write_doc(project.path(), "feature-only.md", "# Feature\n")?;
    commit_all(project.path(), "feature doc")?;

    service.scan_attachments("on-feature")?;
    let on_feature = service.list_records(&ListingQuery::default(), None)?.total;
    let mut annotated = envelope_of(&service, "feature-only")?;
    annotated.title = Some("Only on the feature branch".to_owned());
    annotated.tags = vec!["draft".to_owned()];
    let expected = service.current_revision()?;
    service.apply_transaction(
        "annotate",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(annotated),
        })],
    )?;

    checkout(project.path(), "master", false)
        .or_else(|_| checkout(project.path(), "main", false))?;
    assert!(
        service.scan_is_stale(),
        "the branch moved, and one ref read says so"
    );
    let report = service.scan_attachments("on-main")?;
    assert_eq!(
        report.applied, 1,
        "one document is not on this branch: {:?}",
        report.changes
    );

    // Hidden, not deleted, and the corpus did not shrink.
    let listed = service.list_records(&ListingQuery::default(), None)?;
    assert!(!listed.records.iter().any(|(key, _)| key == "feature-only"));
    let everything = ListingQuery {
        presence: PresenceFilter::Any,
        ..ListingQuery::default()
    };
    assert_eq!(
        service.list_records(&everything, None)?.total,
        on_feature,
        "nothing was created and nothing was deleted by switching"
    );

    // Absence is a routine state, so it is not something doctor calls a fault.
    let report = service.doctor()?;
    assert_eq!(report.hidden, 1);
    assert!(
        report.attachments.is_empty(),
        "another branch having the document is not a decision anybody has to make: {:?}",
        report.attachments
    );

    // Asked for explicitly, it comes back saying why.
    let hidden = ListingQuery {
        presence: PresenceFilter::Absent,
        ..ListingQuery::default()
    };
    let listing = service.list_records(&hidden, None)?;
    assert_eq!(listing.total, 1);
    assert_eq!(
        listing.records[0]
            .1
            .content_ref
            .as_ref()
            .map(|reference| reference.presence),
        Some(memory_hub_core::Presence::NotOnBranch)
    );

    // And back again, with everything it was given while it was here.
    checkout(project.path(), "feature", false)?;
    service.scan_attachments("back")?;
    let envelope = envelope_of(&service, "feature-only")?;
    assert!(
        envelope
            .content_ref
            .as_ref()
            .is_some_and(|r| r.presence.is_present())
    );
    assert_eq!(
        envelope.title.as_deref(),
        Some("Only on the feature branch")
    );
    assert_eq!(envelope.tags, vec!["draft".to_owned()]);
    Ok(())
}

/// A link is a statement about the project, not about a branch. Dropping the
/// backlink would show a document as unreferenced on `main` when it is
/// referenced everywhere else.
#[test]
fn a_link_from_a_hidden_record_survives_and_says_so() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "target.md", "# Target\n")?;
    commit_all(project.path(), "target")?;
    checkout(project.path(), "feature", true)?;
    write_doc(project.path(), "pointer.md", "see docs/target.md\n")?;
    commit_all(project.path(), "pointer")?;
    service.scan_attachments("on-feature")?;

    let expected = service.current_revision()?;
    let mut pointer = envelope_of(&service, "pointer")?;
    pointer.links = vec![memory_hub_core::RecordLink {
        key: "target".to_owned(),
        relation: None,
        extensions: std::collections::BTreeMap::default(),
    }];
    service.apply_transaction(
        "link",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(pointer),
        })],
    )?;

    checkout(project.path(), "master", false)
        .or_else(|_| checkout(project.path(), "main", false))?;
    service.scan_attachments("on-main")?;

    let backlinks = service.backlinks("target", None)?;
    let entry = backlinks
        .entries
        .iter()
        .find(|entry| entry.source_id == "pointer")
        .ok_or("the link is a statement about the project, not the branch")?;
    assert_eq!(entry.source_presence.as_deref(), Some("not_on_branch"));
    Ok(())
}

// --- Storage migration (GITMEMO-29) ----------------------------------------

/// The storage a project declares for documents in the working tree.
const INTO_DOCS: Option<&str> = Some("docs");

/// Naming no storage: content comes back in with the records.
const BACK_TO_RECORDS: Option<&str> = None;

/// Storage is a field of a type definition, and definitions get edited.
/// Letting the data follow an edited field is data loss wearing the clothes of
/// a setting.
#[test]
fn editing_where_a_type_is_stored_is_refused_while_it_has_records() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![declare_type("doc", None)?, put("doc-1", "doc", "a body")?],
    )?;

    let expected = service.current_revision()?;
    let error = service
        .apply_transaction("edit", expected, vec![declare_type("doc", Some("docs"))?])
        .expect_err("a move is not an edit");
    assert_eq!(error.kind, "invalid_argument");
    assert_eq!(error.data["recovery_action"], "migrate_storage");
    assert_eq!(error.data["records"], 1);
    assert_eq!(
        error.data["from"],
        Value::Null,
        "the type named no storage: its content was in its records"
    );
    assert_eq!(error.data["to"], "docs");
    Ok(())
}

/// A kind with nothing in it has nothing to move.
#[test]
fn editing_where_an_empty_type_is_stored_is_allowed() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![declare_type("doc", None)?])?;
    let expected = service.current_revision()?;
    service.apply_transaction("edit", expected, vec![declare_type("doc", Some("docs"))?])?;
    Ok(())
}

/// The plan is the point: what moves, which way, and what is being asked of
/// the caller — before anything is written.
#[test]
fn a_migration_states_its_plan_before_it_does_anything() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![declare_type("doc", None)?, put("guide", "doc", "the body")?],
    )?;

    let plan = service.plan_migration("doc", INTO_DOCS)?;
    assert_eq!(plan.from, None);
    assert_eq!(plan.to, Some("docs".to_owned()));
    assert_eq!(plan.keys, vec!["guide".to_owned()]);
    assert_eq!(
        plan.warnings.iter().map(|w| w.code).collect::<Vec<_>>(),
        vec!["content_becomes_visible"],
        "publishing into the working tree is a change of visibility"
    );
    assert!(
        !project.path().join("docs").exists(),
        "planning wrote nothing"
    );

    // And it refuses to run until that is accepted by name.
    let error = service
        .migrate_storage("go", "doc", INTO_DOCS, &[])
        .expect_err("consent is not a boolean nobody reads");
    assert_eq!(error.kind, "confirmation_required");
    assert_eq!(
        error.data["unacknowledged"],
        json!(["content_becomes_visible"])
    );
    Ok(())
}

#[test]
fn an_acknowledged_migration_publishes_the_content_and_repoints_the_records() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![declare_type("doc", None)?, put("guide", "doc", "the body")?],
    )?;

    service.migrate_storage(
        "go",
        "doc",
        INTO_DOCS,
        &["content_becomes_visible".to_owned()],
    )?;

    assert_eq!(
        std::fs::read_to_string(project.path().join("docs/guide.md"))?,
        "the body"
    );
    let envelope = envelope_of(&service, "guide")?;
    assert!(envelope.content.is_empty(), "the record keeps no copy");
    assert_eq!(
        envelope.content_ref.as_ref().map(|r| r.path.as_str()),
        Some("docs/guide.md")
    );
    assert_eq!(envelope.folder.as_deref(), Some("docs"));

    // Running it again finds the work done and changes nothing.
    let plan = service.plan_migration("doc", INTO_DOCS)?;
    assert_eq!(plan.from, plan.to, "the type has already moved");
    Ok(())
}

/// Moving back is not retroactive privacy, and saying so is part of the
/// operation rather than a footnote somewhere.
#[test]
fn moving_back_into_refs_says_what_it_does_not_do() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![declare_type("doc", None)?, put("guide", "doc", "the body")?],
    )?;
    service.migrate_storage(
        "out",
        "doc",
        INTO_DOCS,
        &["content_becomes_visible".to_owned()],
    )?;

    let plan = service.plan_migration("doc", BACK_TO_RECORDS)?;
    let codes: Vec<&str> = plan.warnings.iter().map(|w| w.code).collect();
    assert!(codes.contains(&"does_not_hide_published_history"));
    assert!(codes.contains(&"files_are_left_in_place"));
    let history = plan
        .warnings
        .iter()
        .find(|w| w.code == "does_not_hide_published_history")
        .ok_or("the warning is there")?;
    assert!(
        history.message.contains("never as retroactive privacy"),
        "{}",
        history.message
    );

    service.migrate_storage(
        "back",
        "doc",
        BACK_TO_RECORDS,
        &codes
            .iter()
            .map(|code| (*code).to_owned())
            .collect::<Vec<_>>(),
    )?;

    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(envelope.content, "the body", "the record carries it again");
    assert_eq!(envelope.content_ref, None);
    assert_eq!(envelope.folder, None);
    assert!(
        project.path().join("docs/guide.md").exists(),
        "Memory does not delete a file it did not put there on this run"
    );
    Ok(())
}

/// Content is written before the records that point at it, so a run cut short
/// leaves files a repeat agrees with, and the single transaction at the end
/// either happened or did not.
#[test]
fn an_interrupted_migration_is_repeatable_without_duplication() -> TestResult {
    let (project, service) = service()?;
    seed(
        &service,
        vec![
            declare_type("doc", None)?,
            put("guide", "doc", "the body")?,
            put("setup", "doc", "another body")?,
        ],
    )?;

    // A run that got as far as writing one file and then died.
    std::fs::create_dir_all(project.path().join("docs"))?;
    std::fs::write(project.path().join("docs/guide.md"), "the body")?;

    service.migrate_storage(
        "resume",
        "doc",
        INTO_DOCS,
        &["content_becomes_visible".to_owned()],
    )?;

    assert_eq!(
        service
            .list_records(&ListingQuery::default(), None)?
            .records
            .iter()
            .filter(|(_, envelope)| envelope.kind == "doc")
            .count(),
        2,
        "two records, not three — nothing was duplicated"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("docs/guide.md"))?,
        "the body"
    );
    assert_eq!(
        std::fs::read_to_string(project.path().join("docs/setup.md"))?,
        "another body"
    );
    Ok(())
}

/// The links of a reference record live in its envelope, and the envelope
/// lives in refs. So if refs is encrypted the links are encrypted with it, and
/// no separate rule about "links pointing outside" is needed — the worry
/// assumed a public document has public metadata, and it has none.
#[test]
fn a_reference_records_links_live_in_refs_with_everything_else() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    service.scan_attachments("attach")?;

    let expected = service.current_revision()?;
    let mut envelope = envelope_of(&service, "guide")?;
    envelope.links = vec![memory_hub_core::RecordLink {
        key: "some-decision".to_owned(),
        relation: None,
        extensions: std::collections::BTreeMap::default(),
    }];
    envelope.tags = vec!["internal".to_owned()];
    service.apply_transaction(
        "annotate",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;

    // Nothing of that is in the file. The file is the content and only the
    // content, which is exactly why attaching one changes nothing in the tree.
    let on_disk = std::fs::read_to_string(project.path().join("docs/guide.md"))?;
    assert_eq!(on_disk, "# Guide\n");
    assert!(!on_disk.contains("some-decision"));
    assert!(!on_disk.contains("internal"));

    let envelope = envelope_of(&service, "guide")?;
    assert_eq!(envelope.links.len(), 1);
    assert_eq!(envelope.tags, vec!["internal".to_owned()]);
    Ok(())
}

/// A directory exists on disk with no permission from us, and a tree drawn
/// from records alone shows none of the ways that can happen.
#[test]
fn a_directory_is_a_folder_even_when_no_record_is_in_it() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    // Empty, and a file outside the mask: two directories no record can reveal.
    std::fs::create_dir_all(project.path().join("docs/api"))?;
    std::fs::create_dir_all(project.path().join("docs/assets"))?;
    std::fs::write(project.path().join("docs/assets/logo.svg"), "<svg/>")?;
    service.scan_attachments("attach")?;

    let folders = service.list_folders(None, false)?;
    let at = |path: &str| {
        folders
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} is missing from the listing"))
    };

    assert!(at("docs/api").in_storage, "an empty directory is a folder");
    assert!(!at("docs/api").in_records);
    assert_eq!(at("docs/api").records, 0);
    assert!(
        at("docs/assets").in_storage,
        "a directory holding an SVG is a folder like any other"
    );
    assert_eq!(
        at("docs/assets").records,
        1,
        "and the SVG is a document — there is no mask left to exclude it"
    );
    let guides = at("docs/guides");
    assert!(guides.in_storage && guides.in_records);
    assert_eq!(guides.records, 1);
    Ok(())
}

/// The folder outlives the branch that has its documents: the directory is
/// still there, and so is the record, hidden rather than gone.
#[test]
fn a_folder_whose_documents_this_branch_hides_stays_visible() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    service.scan_attachments("attach")?;

    // Never committed here, so `HEAD` does not have it: the document belongs to
    // another branch, which is hidden rather than deleted.
    std::fs::remove_file(project.path().join("docs/guides/intro.md"))?;
    service.scan_attachments("switched")?;

    let folders = service.list_folders(Some("docs/guides"), false)?;
    let [guides] = folders.as_slice() else {
        return Err("expected exactly the folder that was asked for".into());
    };
    assert!(guides.in_storage, "the directory is still on disk");
    assert!(
        guides.in_records,
        "the record is hidden, not deleted, and still says where it is filed"
    );
    assert_eq!(
        guides.records, 0,
        "and the count agrees with what opening the folder shows"
    );
    Ok(())
}

/// Two records standing for one folder is a question with no answer. It is
/// refused where the caller still knows what it meant.
#[test]
fn a_folder_may_have_only_one_record_that_is_it() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![folder_record("api-guides", "docs/guides")?])?;

    let expected = service.current_revision()?;
    let error = service
        .apply_transaction(
            "second",
            expected.clone(),
            vec![folder_record("guides-index", "docs/guides")?],
        )
        .expect_err("a second record for the same folder is refused");
    assert_eq!(error.kind, "invalid_record");
    assert_eq!(
        error.data.get("existing_key").and_then(|v| v.as_str()),
        Some("api-guides"),
        "the refusal names the record that is already there"
    );

    // The same conflict inside one batch, where neither record exists yet.
    let error = service
        .apply_transaction(
            "batch",
            expected,
            vec![
                folder_record("one", "docs/api")?,
                folder_record("two", "docs/api")?,
            ],
        )
        .expect_err("one batch cannot carry two records for one folder");
    assert_eq!(error.kind, "invalid_record");
    Ok(())
}

/// Replacing the record that is a folder is not the same as adding a second
/// one, and the rule must not refuse it.
#[test]
fn the_record_that_is_a_folder_can_be_replaced() -> TestResult {
    let (_project, service) = service()?;
    seed(&service, vec![folder_record("api-guides", "docs/guides")?])?;

    let expected = service.current_revision()?;
    service.apply_transaction(
        "swap",
        expected,
        vec![
            Operation::delete(RecordId::plaintext("api-guides")),
            folder_record("guides-index", "docs/guides")?,
        ],
    )?;
    let folders = service.list_folders(Some("docs/guides"), false)?;
    assert_eq!(
        folders.first().and_then(|entry| entry.described.as_deref()),
        Some("guides-index")
    );
    Ok(())
}

/// A folder with nothing else in it is a real folder, because the record that
/// is it is in it. Deleting that record deletes a description, not a folder.
#[test]
fn a_folder_exists_while_the_record_that_is_it_is_there() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            folder_record("api-guides", "docs/guides")?,
            filed("note-1", "note", "docs/guides")?,
        ],
    )?;
    let folders = service.list_folders(Some("docs/guides"), false)?;
    let [guides] = folders.as_slice() else {
        return Err("expected the folder".into());
    };
    assert_eq!(guides.described.as_deref(), Some("api-guides"));
    assert_eq!(guides.records, 2, "the record that is the folder is in it");

    let expected = service.current_revision()?;
    service.apply_transaction(
        "forget",
        expected,
        vec![Operation::delete(RecordId::plaintext("api-guides"))],
    )?;
    let folders = service.list_folders(Some("docs/guides"), false)?;
    let [guides] = folders.as_slice() else {
        return Err("the folder is still there: a document is filed in it".into());
    };
    assert_eq!(
        guides.described, None,
        "the description is what was deleted"
    );
    assert_eq!(guides.records, 1);

    let expected = service.current_revision()?;
    service.apply_transaction(
        "empty",
        expected,
        vec![Operation::delete(RecordId::plaintext("note-1"))],
    )?;
    assert!(
        service.list_folders(Some("docs/guides"), false)?.is_empty(),
        "nothing is filed there and no directory exists: the folder is gone"
    );
    Ok(())
}

/// The description of a folder is knowledge, and knowledge has to be findable.
#[test]
fn the_record_that_is_a_folder_is_searched_like_any_document() -> TestResult {
    let (_project, service) = service()?;
    let mut envelope = Envelope::new("api-guides", "note", "How authentication works here")?;
    envelope.folder = Some("docs/guides".to_owned());
    envelope.is_folder = true;
    envelope.title = Some("API guides".to_owned());
    seed(
        &service,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;
    service.sync_index()?;

    let result = service.search(&SearchRequest {
        query: "authentication".to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters::default(),
        revision: service.current_revision()?,
    })?;
    assert!(
        result.hits.iter().any(|hit| hit.id == "api-guides"),
        "a folder nobody can find is a folder nobody described"
    );
    Ok(())
}

/// Renaming a folder of `refs` is one transaction over everything under it —
/// the record that is the folder included, with no branch of its own.
#[test]
fn renaming_a_refs_folder_carries_everything_under_it() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            folder_record("storage-index", "decisions/storage")?,
            filed("d-engines", "note", "decisions/storage")?,
            filed("d-nested", "note", "decisions/storage/engines")?,
            filed("d-elsewhere", "note", "decisions/naming")?,
        ],
    )?;

    service.rename_folder("decisions/storage", "decisions/persistence", "rename")?;

    let folder_of = |key: &str| -> Result<Option<String>, Box<dyn std::error::Error>> {
        Ok(envelope_of(&service, key)?.folder)
    };
    assert_eq!(
        folder_of("storage-index")?.as_deref(),
        Some("decisions/persistence")
    );
    assert_eq!(
        folder_of("d-engines")?.as_deref(),
        Some("decisions/persistence")
    );
    assert_eq!(
        folder_of("d-nested")?.as_deref(),
        Some("decisions/persistence/engines"),
        "everything below the folder moves with it"
    );
    assert_eq!(
        folder_of("d-elsewhere")?.as_deref(),
        Some("decisions/naming"),
        "a folder that merely starts with the same letters is a different folder"
    );
    Ok(())
}

/// Renaming there means renaming a directory, which a person does the ordinary
/// way. Doing it from here would leave the records disagreeing with the files.
#[test]
fn renaming_a_directory_of_an_attached_folder_is_refused() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    service.scan_attachments("attach")?;

    let error = service
        .rename_folder("docs/guides", "docs/handbook", "rename")
        .expect_err("an attached directory is not renamed through Memory");
    assert_eq!(error.kind, "invalid_argument");
    Ok(())
}

/// A renamed directory moves its files, and the scan follows them. What it must
/// also carry is the record filed there that has no file of its own.
#[test]
fn renaming_a_directory_carries_the_records_filed_in_it() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    write_doc_at(project.path(), "guides/setup.md", "other body\n")?;
    service.scan_attachments("attach")?;
    commit_all(project.path(), "docs")?;

    // A decision filed next to the documents, with no file anywhere.
    let expected = service.current_revision()?;
    service.apply_transaction(
        "file-a-note",
        expected,
        vec![filed("d-guides", "note", "docs/guides")?],
    )?;

    std::fs::rename(
        project.path().join("docs/guides"),
        project.path().join("docs/handbook"),
    )?;
    service.scan_attachments("renamed")?;

    assert_eq!(
        envelope_of(&service, "d-guides")?.folder.as_deref(),
        Some("docs/handbook"),
        "metadata does not travel unless somebody carries it"
    );
    Ok(())
}

/// A record whose bytes are a file in an attached folder can be the folder it
/// is in — that file is usually already there — and it moves with its
/// directory like any other document, keeping the key every link uses.
#[test]
fn a_document_can_be_the_folder_it_lives_in() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/README.md", "What is in here\n")?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    service.scan_attachments("attach")?;
    commit_all(project.path(), "docs")?;

    let key = service
        .list_records(&ListingQuery::default(), None)?
        .records
        .iter()
        .find(|(_, envelope)| {
            envelope
                .content_ref
                .as_ref()
                .is_some_and(|reference| reference.path.ends_with("guides/README.md"))
        })
        .map(|(key, _)| key.clone())
        .ok_or("the README was not scanned")?;

    let mut envelope = envelope_of(&service, &key)?;
    envelope.is_folder = true;
    let expected = service.current_revision()?;
    service.apply_transaction(
        "mark",
        expected,
        vec![Operation::put(StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        })],
    )?;
    assert_eq!(
        service
            .list_folders(Some("docs/guides"), false)?
            .first()
            .and_then(|entry| entry.described.as_deref()),
        Some(key.as_str())
    );

    std::fs::rename(
        project.path().join("docs/guides"),
        project.path().join("docs/handbook"),
    )?;
    service.scan_attachments("renamed")?;

    let moved = envelope_of(&service, &key)?;
    assert_eq!(
        moved.folder.as_deref(),
        Some("docs/handbook"),
        "it moves with its directory, like the document it is"
    );
    assert!(moved.is_folder, "and it is still the folder it is in");
    assert_eq!(
        service
            .list_folders(Some("docs/handbook"), false)?
            .first()
            .and_then(|entry| entry.described.as_deref()),
        Some(key.as_str()),
        "the key does not follow the path, so every link still resolves"
    );
    Ok(())
}

/// Answering a question about the subject matter with a JSON schema answers
/// the wrong question.
#[test]
fn a_type_definition_is_not_an_answer_about_the_subject_matter() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            put(
                &type_key("payment"),
                "__type__",
                &serde_json::to_string(&json!({"kind_name": "payment"}))?,
            )?,
            put("p-1", "payment", "how a payment is captured")?,
        ],
    )?;
    service.sync_index()?;

    let search = |filters: SearchFilters| -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Ok(service
            .search(&SearchRequest {
                query: "payment".to_owned(),
                limit: 10,
                offset: 0,
                filters,
                revision: service.current_revision()?,
            })?
            .hits
            .iter()
            .map(|hit| hit.id.clone())
            .collect())
    };

    let hits = search(SearchFilters::default())?;
    assert!(hits.contains(&"p-1".to_owned()), "the document is found");
    assert!(
        !hits.contains(&type_key("payment")),
        "and the schema that describes it is not"
    );

    let asked = search(SearchFilters {
        include_service: true,
        ..SearchFilters::default()
    })?;
    assert!(
        asked.contains(&type_key("payment")),
        "the tools that maintain schema can still reach it"
    );
    Ok(())
}

/// A file moved one level deeper shares a prefix with its parent, and reading
/// that as the parent being renamed would rewrite every record filed under it.
#[test]
fn one_file_moving_deeper_is_not_a_directory_being_renamed() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc_at(project.path(), "guides/intro.md", "body\n")?;
    write_doc_at(project.path(), "guides/setup.md", "other body\n")?;
    service.scan_attachments("attach")?;
    commit_all(project.path(), "docs")?;

    let expected = service.current_revision()?;
    service.apply_transaction(
        "file-a-note",
        expected,
        vec![filed("d-guides", "note", "docs/guides")?],
    )?;

    // One document moves into a directory below; the other stays where it is.
    std::fs::create_dir_all(project.path().join("docs/guides/api"))?;
    std::fs::rename(
        project.path().join("docs/guides/intro.md"),
        project.path().join("docs/guides/api/intro.md"),
    )?;
    service.scan_attachments("moved")?;

    assert_eq!(
        envelope_of(&service, "d-guides")?.folder.as_deref(),
        Some("docs/guides"),
        "the directory is still there, so nothing was renamed"
    );
    Ok(())
}

/// Renaming a folder that *contains* an attachment would leave the records
/// claiming a path the directory on disk contradicts.
#[test]
fn renaming_a_folder_above_an_attachment_is_refused() -> TestResult {
    let (_project, service) = attached_project()?;
    let expected = service.current_revision()?;
    service.apply_transaction(
        "file-a-note",
        expected,
        vec![filed("d-note", "note", "docs")?],
    )?;

    let error = service
        .rename_folder("docs", "documentation", "rename")
        .expect_err("the attachment root is under this folder");
    assert_eq!(error.kind, "invalid_argument");
    Ok(())
}

/// A record that is the folder it is filed in.
fn folder_record(key: &str, folder: &str) -> Result<Operation, Box<dyn std::error::Error>> {
    let mut envelope = Envelope::new(key, "note", "what is in this folder")?;
    envelope.folder = Some(folder.to_owned());
    envelope.is_folder = true;
    Ok(Operation::put(StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    }))
}

/// An ordinary record, filed somewhere.
fn filed(key: &str, kind: &str, folder: &str) -> Result<Operation, Box<dyn std::error::Error>> {
    let mut envelope = Envelope::new(key, kind, "body")?;
    envelope.folder = Some(folder.to_owned());
    Ok(Operation::put(StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    }))
}

// --- What a source can do --------------------------------------------------

use memory_hub_service::{Attachment, DocumentSource, FolderSource};

fn docs_source(project: &std::path::Path) -> FolderSource<'_> {
    FolderSource::new(
        project,
        Attachment::new("docs".to_owned()),
    )
}

/// A write goes through the source that owns the locator. Reaching around it
/// worked only while every source was a folder.
#[test]
fn a_source_writes_and_reads_back_its_own_document() -> TestResult {
    let (project, _service) = service()?;
    let source = docs_source(project.path());

    assert!(source.capabilities().writable);
    source.write("docs/guide.md", b"the body")?;

    assert_eq!(source.read("docs/guide.md")?.as_deref(), Some("the body"));
    assert_eq!(
        std::fs::read_to_string(project.path().join("docs/guide.md"))?,
        "the body",
        "and it is an ordinary file, where a person can find it"
    );
    Ok(())
}

/// Bytes, not text: a documentation folder holds diagrams beside the Markdown.
#[test]
fn a_source_writes_bytes_that_are_not_text() -> TestResult {
    let (project, _service) = service()?;
    let source = docs_source(project.path());
    let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe];

    source.write("docs/diagram.png", &png)?;

    assert_eq!(
        std::fs::read(project.path().join("docs/diagram.png"))?,
        png,
        "written unchanged"
    );
    let listed = source.list()?;
    assert!(
        listed.iter().any(|doc| doc.locator == "docs/diagram.png"),
        "and it is a document like any other: {listed:?}"
    );
    Ok(())
}

/// A locator outside the storage is refused by the storage, not by whoever
/// happened to build the path.
#[test]
fn a_source_refuses_a_locator_that_is_not_its_own() -> TestResult {
    let (project, _service) = service()?;
    let source = docs_source(project.path());

    let error = source
        .write("elsewhere/stray.md", b"not here")
        .expect_err("outside the folder");

    assert_eq!(error.kind, "invalid_argument");
    assert!(!project.path().join("elsewhere/stray.md").exists());
    Ok(())
}

/// The listing carries what a viewer needs to draw a row, so it does not have
/// to ask again per document.
#[test]
fn the_listing_says_which_documents_can_be_written() -> TestResult {
    let (project, _service) = service()?;
    let source = docs_source(project.path());
    source.write("docs/editable.md", b"yours")?;
    source.write("docs/frozen.md", b"not yours")?;

    let frozen = project.path().join("docs/frozen.md");
    let mut permissions = std::fs::metadata(&frozen)?.permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&frozen, permissions)?;

    let documents = source.list()?;
    let at = |locator: &str| {
        documents
            .iter()
            .find(|doc| doc.locator == locator)
            .unwrap_or_else(|| panic!("{locator} is missing"))
            .writable
    };

    assert!(at("docs/editable.md"));
    assert!(!at("docs/frozen.md"), "a read-only file is not writable");
    Ok(())
}

/// A type that keeps its content in its records is writable by definition; one
/// pointing at a storage is only as writable as that storage.
#[test]
fn a_type_says_whether_its_documents_can_be_written() -> TestResult {
    let (_project, service) = service()?;
    seed(
        &service,
        vec![
            declare_type("note", None)?,
            declare_type("doc", Some("docs"))?,
        ],
    )?;

    let types = service.list_types()?;
    let at = |kind: &str| {
        types
            .iter()
            .find(|summary| summary.kind_name == kind)
            .unwrap_or_else(|| panic!("{kind} is missing"))
    };

    assert_eq!(at("note").storage, None);
    assert!(at("note").writable);
    assert_eq!(at("doc").storage.as_deref(), Some("docs"));
    assert!(at("doc").writable);
    Ok(())
}

// --- What a body actually is -----------------------------------------------

use memory_hub_service::{Content, media_type_for};

/// Reading as text first reported a diagram as missing — wrong, and the
/// opposite of what the person who put it there sees in their folder.
#[test]
fn a_document_that_is_not_text_is_read_as_bytes() -> TestResult {
    let (project, service) = attached_project()?;
    let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0xff, 0xfe];
    std::fs::write(project.path().join("docs/diagram.png"), png)?;
    service.scan_attachments("attach")?;

    let key = service
        .list_records(&ListingQuery::default(), None)?
        .records
        .into_iter()
        .map(|(key, _)| key)
        .find(|key| key.contains("diagram"))
        .ok_or("the diagram became a record")?;

    let ContentResolution::Resolved { content, .. } = service.resolve_content(&key)? else {
        return Err("the file is right there".into());
    };
    assert_eq!(
        content,
        Content::Bytes(png.to_vec()),
        "bytes, unchanged, not a failure and not replacement characters"
    );
    Ok(())
}

/// The record says what its body is before anybody fetches it.
#[test]
fn a_scanned_document_records_what_it_is() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    std::fs::write(project.path().join("docs/diagram.png"), [0x89, b'P'])?;
    service.scan_attachments("attach")?;

    let records = service
        .list_records(&ListingQuery::default(), None)?
        .records;
    let media_type = |needle: &str| {
        records
            .iter()
            .find(|(key, _)| key.contains(needle))
            .and_then(|(_, envelope)| envelope.media_type.clone())
    };

    assert_eq!(media_type("guide").as_deref(), Some("text/markdown"));
    assert_eq!(media_type("diagram").as_deref(), Some("image/png"));
    Ok(())
}

/// By name, never by content: reading every file on every scan to find out what
/// it is would be expensive and would change under somebody mid-save.
#[test]
fn a_media_type_comes_from_the_name() {
    assert_eq!(media_type_for("docs/guide.md"), Some("text/markdown"));
    assert_eq!(media_type_for("docs/GUIDE.MD"), Some("text/markdown"));
    assert_eq!(media_type_for("assets/clip.mp4"), Some("video/mp4"));
    assert_eq!(media_type_for("assets/logo.svg"), Some("image/svg+xml"));

    // An extension this build does not know says nothing, rather than
    // guessing something a viewer would act on.
    assert_eq!(media_type_for("docs/notes.dwg"), None);
    assert_eq!(media_type_for("docs/LICENSE"), None);
}

// --- Deleting a document, and removing the type over its folder ------------

/// A record owns its body wherever the project put it, so deleting the record
/// takes the document — and the deletion has to stick.
///
/// The bug this pins: leaving the file behind is not a smaller deletion, it is
/// a deletion that undoes itself. The next scan finds a document belonging to
/// no record and hands back a record for it, with a key derived from the path
/// and none of the links the deleted one had.
#[test]
fn deleting_a_record_takes_its_document_and_the_scan_does_not_bring_it_back() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    write_doc(project.path(), "setup.md", "# Setup\n")?;
    commit_all(project.path(), "the documents")?;
    service.scan_attachments("attach")?;

    let expected = service.current_revision()?;
    service.apply_transaction(
        "remove",
        expected,
        vec![Operation::delete(RecordId::plaintext("guide"))],
    )?;

    assert!(service.get_record("guide", None)?.record.is_none());
    assert!(
        !project.path().join("docs/guide.md").exists(),
        "the document went with its record"
    );
    assert!(
        project.path().join("docs/setup.md").exists(),
        "and nothing else did"
    );

    let report = service.scan_attachments("rescan")?;
    assert!(
        !report
            .changes
            .iter()
            .any(|change| matches!(change, ScanChange::New { .. })),
        "the deletion is not undone by the next scan: {:?}",
        report.changes
    );
    assert!(
        service.get_record("guide", None)?.record.is_none(),
        "and the record stays gone"
    );
    Ok(())
}

/// Removing a type is the other operation, and the difference is the working
/// tree.
///
/// The records of an attached type describe files the repository had before
/// Memory did. So the type goes, its records go, the declaration that pointed
/// at the folder goes — and every file stays exactly where it was.
#[test]
fn removing_a_type_over_a_folder_detaches_it_and_keeps_every_file() -> TestResult {
    let (project, service) = attached_project()?;
    write_doc(project.path(), "guide.md", "# Guide\n")?;
    write_doc(project.path(), "setup.md", "# Setup\n")?;
    commit_all(project.path(), "the documents")?;
    service.scan_attachments("attach")?;
    let before = snapshot_of_folder(project.path())?;

    let removal = service.remove_type("doc", "detach")?;

    assert_eq!(removal.removed, 2, "both records of the type went");
    assert_eq!(removal.detached.as_deref(), Some("docs"));
    assert_eq!(
        snapshot_of_folder(project.path())?,
        before,
        "the folder is exactly as it was"
    );
    assert!(service.get_record("guide", None)?.record.is_none());
    assert!(
        service.schema_registry()?.get("doc").is_none(),
        "the definition went with them"
    );
    // The type that keeps its content in its records is untouched by all of it.
    assert!(service.schema_registry()?.get("note").is_some());
    Ok(())
}

/// A type this project does not hold is a refusal, not a silent success: a
/// caller that misspelled a kind has not removed anything and should be told.
#[test]
fn removing_a_type_the_project_does_not_hold_is_refused() -> TestResult {
    let (_project, service) = attached_project()?;
    let failure = service.remove_type("nonesuch", "detach").unwrap_err();
    assert_eq!(failure.kind, "invalid_argument");
    Ok(())
}
