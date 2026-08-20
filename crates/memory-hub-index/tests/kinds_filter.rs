//! Narrowing a search to several kinds is one query, not one per kind.
//!
//! A caller filtering by type has a set of them, not a single one: a person
//! searching the three types they work in, a window offering a list of
//! checkboxes. Answered with a single `kind`, such a caller has to fan out and
//! fuse the results itself — and it cannot, because rank is only comparable
//! inside one answer. So the set belongs in the request.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{Projection, SearchFilters, SearchRequest};
use memory_hub_store::{GitStore, Operation, Revision, Transaction};

fn record(
    key: &str,
    kind: &str,
    content: &str,
) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, kind, content)?),
    })
}

async fn seeded() -> Result<(tempfile::TempDir, Projection, Revision), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let projection = Projection::open(project.path().join("index")).await?;

    let empty = store.current()?;
    projection.rebuild(&empty).await?;
    let written = store.apply(&Transaction {
        id: "seed".to_owned(),
        expected_revision: empty.revision().clone(),
        operations: vec![
            Operation::put(record("d-1", "decision", "sidecar bundling decided")?),
            Operation::put(record("o-1", "observation", "sidecar restarts observed")?),
            Operation::put(record("q-1", "question", "sidecar ownership unresolved")?),
        ],
    })?;
    projection
        .update(&store, empty.revision(), &written.revision)
        .await?;

    let revision = written.revision.clone();
    Ok((project, projection, revision))
}

fn ask(filters: SearchFilters, revision: &Revision) -> SearchRequest {
    SearchRequest {
        query: "sidecar".to_owned(),
        limit: 10,
        offset: 0,
        filters,
        revision: revision.clone(),
    }
}

#[tokio::test]
async fn several_kinds_are_asked_for_at_once() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    let result = projection
        .search(&ask(
            SearchFilters {
                kinds: vec!["decision".to_owned(), "question".to_owned()],
                ..SearchFilters::default()
            },
            &revision,
        ))
        .await?;

    let mut found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    found.sort_unstable();
    assert_eq!(
        found,
        vec!["d-1", "q-1"],
        "the observation belongs to a kind that was not asked for"
    );
    assert_eq!(
        result.total, 2,
        "the total counts the narrowed set, not the corpus"
    );
    Ok(())
}

#[tokio::test]
async fn one_kind_and_a_set_are_the_union() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    let result = projection
        .search(&ask(
            SearchFilters {
                kind: Some("observation".to_owned()),
                kinds: vec!["decision".to_owned()],
                ..SearchFilters::default()
            },
            &revision,
        ))
        .await?;

    let mut found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    found.sort_unstable();
    assert_eq!(
        found,
        vec!["d-1", "o-1"],
        "naming both spellings asks for both, rather than one of them winning"
    );
    Ok(())
}

#[tokio::test]
async fn an_unexpressible_kind_narrows_exactly_as_a_predicate_would()
-> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    // A kind with a quote in it cannot be a SQL literal, so the whole set is
    // applied to the decoded hits. The expressible members must not be turned
    // into a predicate on their own: that would drop this one before anything
    // saw it, and the caller would get a short answer with no way to tell.
    let result = projection
        .search(&ask(
            SearchFilters {
                kinds: vec!["decision".to_owned(), "it's-a-kind".to_owned()],
                ..SearchFilters::default()
            },
            &revision,
        ))
        .await?;

    let found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(found, vec!["d-1"]);
    Ok(())
}

#[tokio::test]
async fn no_kinds_is_no_restriction() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    let result = projection
        .search(&ask(SearchFilters::default(), &revision))
        .await?;

    assert_eq!(result.hits.len(), 3);
    Ok(())
}
