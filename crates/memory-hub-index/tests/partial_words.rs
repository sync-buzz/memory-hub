//! Part of a word finds the word.
//!
//! BM25 matches whole terms, so `arch` does not find `architecture` — which to
//! anybody typing into a search field reads as the search being broken. The
//! inverted index still answers first; this is the pass that widens a result it
//! came back thin on.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{Projection, SearchFilters, SearchRequest};
use memory_hub_store::{GitStore, Operation, Revision, Transaction};

fn record(
    key: &str,
    title: &str,
    content: &str,
) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    let mut envelope = Envelope::new(key, "doc", content)?;
    envelope.title = Some(title.to_owned());
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(envelope),
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
            Operation::put(record(
                "architecture",
                "Architecture",
                "How the shell, the engine and the window fit together.",
            )?),
            Operation::put(record(
                "community",
                "Community",
                "Nothing about buildings here.",
            )?),
        ],
    })?;
    projection
        .update(&store, empty.revision(), &written.revision)
        .await?;

    let revision = written.revision.clone();
    Ok((project, projection, revision))
}

fn ask(query: &str, revision: &Revision) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        limit: 10,
        offset: 0,
        filters: SearchFilters::default(),
        revision: revision.clone(),
    }
}

#[tokio::test]
async fn the_start_of_a_word_finds_it() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    let result = projection.search(&ask("arch", &revision)).await?;

    let found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert!(
        found.contains(&"architecture"),
        "`arch` has to find Architecture, got {found:?}"
    );
    Ok(())
}

#[tokio::test]
async fn the_middle_of_a_word_finds_it_too() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    // Somebody who remembers half a word remembers the middle of it as often as
    // the start, and a term index cannot answer either.
    let result = projection.search(&ask("chitect", &revision)).await?;

    let found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert!(found.contains(&"architecture"), "got {found:?}");
    Ok(())
}

#[tokio::test]
async fn every_term_has_to_appear() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    // Two fragments narrow. `arch` alone matches the record; `arch buildings`
    // must not, because the second word is somewhere else entirely.
    let result = projection.search(&ask("arch buildings", &revision)).await?;

    let found: Vec<&str> = result.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert!(
        !found.contains(&"architecture"),
        "a second term narrows rather than widens, got {found:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_whole_term_still_ranks_first() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded().await?;

    // The substring pass appends, so an index match keeps its place: this is a
    // widening of a thin answer, not a re-ranking of a good one.
    let result = projection.search(&ask("architecture", &revision)).await?;

    assert_eq!(
        result.hits.first().map(|hit| hit.id.as_str()),
        Some("architecture")
    );
    Ok(())
}
