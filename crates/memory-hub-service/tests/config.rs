//! What a project's storage declaration has to satisfy.

#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use memory_hub_service::{Holds, ProjectConfig, StorageConfig, StorageKind};

fn storage(kind: StorageKind, path: Option<&str>, holds: &[Holds]) -> StorageConfig {
    StorageConfig {
        kind,
        path: path.map(str::to_owned),
        holds: holds.iter().copied().collect::<BTreeSet<_>>(),
        new_files: None,
    }
}

fn config(
    entries: Vec<(&str, StorageConfig)>,
) -> Result<ProjectConfig, memory_hub_service::ServiceError> {
    ProjectConfig::new(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect::<BTreeMap<_, _>>(),
    )
}

#[test]
fn a_project_names_its_storages() {
    let declared = config(vec![
        (
            "main",
            storage(StorageKind::Refs, None, &[Holds::Records, Holds::Content]),
        ),
        (
            "docs",
            storage(StorageKind::RepoFolder, Some("docs"), &[Holds::Content]),
        ),
    ])
    .unwrap();

    let (name, records) = declared.record_storage().unwrap();
    assert_eq!(name, "main");
    assert_eq!(records.kind, StorageKind::Refs);
    assert!(declared.storage("docs").unwrap().holds(Holds::Content));
}

#[test]
fn exactly_one_storage_holds_records() {
    // None: there is nowhere for an envelope to go.
    let error = config(vec![(
        "docs",
        storage(StorageKind::RepoFolder, Some("docs"), &[Holds::Content]),
    )])
    .unwrap_err();
    assert_eq!(error.kind, "invalid_argument");

    // Two: "the revision" would mean two different things at once.
    let error = config(vec![
        ("main", storage(StorageKind::Refs, None, &[Holds::Records])),
        (
            "spare",
            storage(
                StorageKind::Folder,
                Some(".memory/records"),
                &[Holds::Records],
            ),
        ),
    ])
    .unwrap_err();
    assert!(
        error.data["holders"].is_array(),
        "both are named, so the person can pick: {:?}",
        error.data
    );
}

#[test]
fn a_folder_of_somebody_elses_documents_cannot_hold_records() {
    let error = config(vec![(
        "docs",
        storage(StorageKind::RepoFolder, Some("docs"), &[Holds::Records]),
    )])
    .unwrap_err();
    assert_eq!(error.data["field"], "storages.docs.holds");
}

#[test]
fn a_storage_says_where_it_is_exactly_when_it_has_a_where() {
    let error = config(vec![(
        "main",
        storage(StorageKind::Folder, None, &[Holds::Records]),
    )])
    .unwrap_err();
    assert_eq!(error.data["field"], "storages.main.path");

    let error = config(vec![(
        "main",
        storage(StorageKind::Refs, Some("somewhere"), &[Holds::Records]),
    )])
    .unwrap_err();
    assert_eq!(error.data["field"], "storages.main.path");
}

#[test]
fn a_path_cannot_leave_the_project() {
    for path in ["../outside", "/absolute", "docs/../..", ".git/hooks"] {
        let error = config(vec![
            ("main", storage(StorageKind::Refs, None, &[Holds::Records])),
            (
                "stray",
                storage(StorageKind::RepoFolder, Some(path), &[Holds::Content]),
            ),
        ])
        .unwrap_err();
        assert_eq!(
            error.data["field"], "storages.stray.path",
            "`{path}` must be refused"
        );
    }
}

#[test]
fn a_type_pointing_at_a_storage_that_is_not_there_is_told_what_is() {
    let declared = config(vec![(
        "main",
        storage(StorageKind::Refs, None, &[Holds::Records, Holds::Content]),
    )])
    .unwrap();

    let error = declared.storage("dcos").unwrap_err();
    assert_eq!(error.data["storage"], "dcos");
    assert_eq!(
        error.data["declared"],
        serde_json::json!(["main"]),
        "a typo is answered with the list, not with a shrug"
    );
}

#[test]
fn it_survives_a_round_trip_through_the_file() {
    let project = tempfile::tempdir().unwrap();
    let declared = config(vec![
        (
            "main",
            storage(
                StorageKind::Folder,
                Some(".memory/records"),
                &[Holds::Records],
            ),
        ),
        (
            "docs",
            storage(StorageKind::RepoFolder, Some("docs"), &[Holds::Content]),
        ),
    ])
    .unwrap();

    let path = declared.save_new(project.path()).unwrap();
    assert!(path.ends_with(".memory/config.json"));
    assert_eq!(ProjectConfig::load(project.path()).unwrap(), declared);

    // Writing over an existing declaration would move a project's memory
    // without moving the memory.
    assert_eq!(
        declared.save_new(project.path()).unwrap_err().kind,
        "conflict"
    );
}

#[test]
fn a_project_with_no_declaration_says_so() {
    let project = tempfile::tempdir().unwrap();
    let error = ProjectConfig::load(project.path()).unwrap_err();
    assert_eq!(error.kind, "not_initialised");
}

// --- Initialisation --------------------------------------------------------

use memory_hub_service::MemoryService;

#[test]
fn init_prepares_the_storage_it_declares() {
    let project = tempfile::tempdir().unwrap();
    git2::Repository::init(project.path()).unwrap();

    let config = MemoryService::init(
        project.path(),
        BTreeMap::from([("main".to_owned(), StorageConfig::refs())]),
    )
    .unwrap();

    assert_eq!(config.record_storage().unwrap().0, "main");
    assert!(
        project.path().join(".git/refs/memory/staged").exists()
            || project.path().join(".git/packed-refs").exists(),
        "the refs storage was prepared, not merely described"
    );
    assert!(ProjectConfig::load(project.path()).is_ok());
}

#[test]
fn init_prepares_a_folder_storage_without_git() {
    // No repository here at all: this is the case the folder storage exists for.
    let project = tempfile::tempdir().unwrap();

    MemoryService::init(
        project.path(),
        BTreeMap::from([("main".to_owned(), StorageConfig::folder(".memory"))]),
    )
    .unwrap();

    assert!(project.path().join(".memory/records").is_dir());
    assert!(project.path().join(".memory/config.json").is_file());
}

#[test]
fn a_project_is_initialised_once() {
    let project = tempfile::tempdir().unwrap();
    let storages = || BTreeMap::from([("main".to_owned(), StorageConfig::folder(".memory"))]);

    MemoryService::init(project.path(), storages()).unwrap();
    let error = MemoryService::init(project.path(), storages()).unwrap_err();

    assert_eq!(
        error.kind, "conflict",
        "initialising twice would move a project's memory without moving the memory"
    );
}

#[test]
fn records_cannot_be_put_in_somebody_elses_folder() {
    let project = tempfile::tempdir().unwrap();

    let error = MemoryService::init(
        project.path(),
        BTreeMap::from([("docs".to_owned(), StorageConfig::repo_folder("docs"))]),
    )
    .unwrap_err();

    assert_eq!(error.kind, "invalid_argument");
    assert!(
        !project.path().join(".memory/config.json").exists(),
        "and nothing was written for a declaration that does not hold together"
    );
}

#[test]
fn a_storage_can_be_declared_after_init() {
    let project = tempfile::tempdir().unwrap();
    let mut service = MemoryService::open(project.path().to_path_buf());
    MemoryService::init(
        project.path(),
        BTreeMap::from([("main".to_owned(), StorageConfig::folder(".memory"))]),
    )
    .unwrap();

    // A project told once where its memory lives would send people to edit
    // the file by hand the first time they attach a folder.
    let config = service
        .declare_storage("docs", StorageConfig::repo_folder("docs"))
        .unwrap();

    assert_eq!(
        config.storage("docs").unwrap().kind,
        StorageKind::RepoFolder
    );
    assert_eq!(
        ProjectConfig::load(project.path()).unwrap(),
        config,
        "and it is on disk, not only in this service"
    );
}

#[test]
fn a_declared_name_is_not_quietly_reused() {
    let project = tempfile::tempdir().unwrap();
    let mut service = MemoryService::open(project.path().to_path_buf());
    MemoryService::init(
        project.path(),
        BTreeMap::from([("main".to_owned(), StorageConfig::folder(".memory"))]),
    )
    .unwrap();

    let error = service
        .declare_storage("main", StorageConfig::repo_folder("docs"))
        .unwrap_err();

    assert_eq!(
        error.kind, "conflict",
        "redeclaring a name would move records without moving them"
    );
}

#[test]
fn a_second_storage_cannot_take_over_the_records() {
    let project = tempfile::tempdir().unwrap();
    let mut service = MemoryService::open(project.path().to_path_buf());
    MemoryService::init(
        project.path(),
        BTreeMap::from([("main".to_owned(), StorageConfig::folder(".memory"))]),
    )
    .unwrap();

    let error = service
        .declare_storage("spare", StorageConfig::refs())
        .unwrap_err();

    assert_eq!(error.kind, "invalid_argument");
    assert_eq!(
        ProjectConfig::load(project.path()).unwrap().storages.len(),
        1,
        "and nothing was written for a declaration that does not hold together"
    );
}
