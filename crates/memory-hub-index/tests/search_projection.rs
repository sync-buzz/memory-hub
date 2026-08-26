#![allow(clippy::expect_used)]

//! A hit says which record to read, and how many there are to choose from.
//!
//! Two things a search answer has to get right before anything else about it
//! matters: it must not send the corpus back to whoever asked a question about
//! it, and the number it reports as `total` must be the number of matches
//! rather than the size of the page it happened to read.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{Projection, SearchFilters, SearchRequest};
use memory_hub_store::{GitStore, Operation, Revision, Transaction};

/// A body long enough that returning it would be the whole point of not
/// returning it, with the word being looked for near the end.
fn long_body(needle: &str) -> String {
    let filler = "The window, the engine and the shell each keep their own half \
                  of the story, and none of it is what this test is looking for. "
        .repeat(60);
    format!("{filler}The answer is that {needle} lives here. {filler}")
}

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

async fn seeded(
    operations: Vec<Operation>,
) -> Result<(tempfile::TempDir, Projection, Revision), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let projection = Projection::open(project.path().join("index")).await?;

    let empty = store.current()?;
    projection.rebuild(&empty).await?;
    let written = store.apply(&Transaction {
        id: "seed".to_owned(),
        expected_revision: empty.revision().clone(),
        operations,
    })?;
    projection
        .update(&store, empty.revision(), &written.revision)
        .await?;

    let revision = written.revision.clone();
    Ok((project, projection, revision))
}

fn ask(query: &str, limit: usize, revision: &Revision) -> SearchRequest {
    SearchRequest {
        query: query.to_owned(),
        limit,
        offset: 0,
        filters: SearchFilters::default(),
        revision: revision.clone(),
    }
}

#[tokio::test]
async fn a_hit_carries_the_window_around_the_match() -> Result<(), Box<dyn std::error::Error>> {
    let body = long_body("kingfisher");
    let (_project, projection, revision) =
        seeded(vec![Operation::put(record("manual", "Manual", &body)?)]).await?;

    let result = projection.search(&ask("kingfisher", 10, &revision)).await?;

    let hit = result.hits.first().expect("the record has the word in it");
    let excerpt = hit.excerpt.as_deref().expect("a text body has a window");
    assert!(
        excerpt.contains("kingfisher"),
        "the window is cut around the match, got {excerpt:?}"
    );
    assert!(
        excerpt.chars().count() < body.chars().count() / 4,
        "a window is not the body: {} of {} characters",
        excerpt.chars().count(),
        body.chars().count()
    );
    assert_eq!(
        hit.content_chars,
        body.chars().count(),
        "the whole body's length is stated even though the body is not sent"
    );
    let at = hit.excerpt_at.expect("a window says where it starts");
    let found = body.find("kingfisher").expect("the fixture holds the word");
    let found_chars = body[..found].chars().count();
    assert!(
        at <= found_chars && found_chars < at + 240,
        "the window starts before the match and holds it: window at {at}, match at {found_chars}"
    );
    Ok(())
}

#[tokio::test]
async fn total_counts_the_matches_not_the_page() -> Result<(), Box<dyn std::error::Error>> {
    let operations = (0..12)
        .map(|index| {
            record(
                &format!("note-{index}"),
                &format!("Note {index}"),
                "Every one of these mentions kingfisher exactly once.",
            )
            .map(Operation::put)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (_project, projection, revision) = seeded(operations).await?;

    let result = projection.search(&ask("kingfisher", 3, &revision)).await?;

    assert_eq!(
        result.hits.len(),
        3,
        "the page is the page that was asked for"
    );
    assert_eq!(
        result.total, 12,
        "`total` is how many records match, not how many were read"
    );
    assert!(result.has_more, "nine more of them are waiting");
    assert!(
        !result.total_capped,
        "twelve is well under the counting cap"
    );
    Ok(())
}

#[tokio::test]
async fn a_page_that_did_not_fill_is_the_whole_answer() -> Result<(), Box<dyn std::error::Error>> {
    let (_project, projection, revision) = seeded(vec![
        Operation::put(record("one", "One", "kingfisher")?),
        Operation::put(record("two", "Two", "nothing to see")?),
    ])
    .await?;

    let result = projection.search(&ask("kingfisher", 10, &revision)).await?;

    assert_eq!(result.total, 1);
    assert!(!result.has_more);
    Ok(())
}
