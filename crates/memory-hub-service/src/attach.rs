//! Reconciling a source of documents with the records that describe them.
//!
//! A repository folder is one such source: Git versions it, a pull request
//! shows it in the diff, and Memory writes nothing into it — not a marker, not
//! an id — so a colleague who has never heard of Memory sees a repository that
//! has not changed. A database or a wiki would be another source, answering
//! the same question in the same shape.
//!
//! The price of writing nothing into a document is that it carries no
//! identity, so reconciliation works from the pair `locator + digest`. Three
//! cases are unambiguous. The fourth is not, by construction, and is never
//! decided here.

use std::collections::{BTreeMap, BTreeSet};

use memory_hub_core::{ContentHash, Presence};
use serde::{Deserialize, Serialize};

use crate::ServiceError;

/// Where a storage's documents are, in the working tree.
///
/// A folder and nothing else — no file-name mask, deliberately: a person who
/// put a diagram in their documentation folder should see the diagram in
/// Memory. A mask decides which files are worth having,
/// and that is not a decision a corpus gets to make about somebody's project —
/// what we cannot render is a question for the viewer, not for the scan.
///
/// One storage holds one type, so nothing has to be told apart inside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    /// Directory, relative to the project root.
    pub folder: String,
    /// What to call a document this attachment creates, with `*` where the
    /// record's key goes. A default for writing, never a filter for reading.
    pub new_files: String,
}

/// Names the operating system leaves behind. Nobody put them there and nobody
/// will miss them; every other file is somebody's.
const LITTER: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

impl Attachment {
    #[must_use]
    pub const fn new(folder: String, new_files: String) -> Self {
        Self { folder, new_files }
    }

    /// Whether `path` — project-relative — is one of this attachment's
    /// documents.
    ///
    /// At any depth below the folder: a documentation directory always has
    /// nested directories, and flattening them would collapse `guides/` and
    /// `api/` into one list.
    #[must_use]
    pub fn covers(&self, path: &str) -> bool {
        let Some(below) = path.strip_prefix(&format!("{}/", self.folder)) else {
            return false;
        };
        let name = below.rsplit('/').next().unwrap_or(below);
        !name.is_empty() && !LITTER.contains(&name)
    }
}

/// The media type a locator implies, by its extension.
///
/// By name, never by content: deciding from the bytes would mean reading every
/// document on every scan, and the answer would change under somebody's hands
/// while they are mid-save. A name is what a person chose, and it is what
/// their editor and their browser go by too.
///
/// `None` for an extension this build does not know — better than a plausible
/// guess a viewer would act on.
#[must_use]
pub fn media_type_for(locator: &str) -> Option<&'static str> {
    let extension = locator.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "md" | "markdown" => "text/markdown",
        "txt" | "text" => "text/plain",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "csv" => "text/csv",
        "json" => "application/json",
        "yaml" | "yml" => "application/yaml",
        "toml" => "application/toml",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => return None,
    })
}

/// What a source can do, so a caller can ask instead of trying.
///
/// One field today, and a struct rather than a bool because the question grows:
/// an HTTP source will read and not write, a database may write some tables and
/// not others. A caller that asks about a capability this build has never heard
/// of should get a default, not a type that no longer deserialises.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceCapabilities {
    /// Whether documents can be written here at all.
    pub writable: bool,
}

/// One document a source currently holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceDocument {
    /// How the source addresses it. A repository-relative path, for a folder.
    pub locator: String,
    pub hash: ContentHash,
    /// Whether this particular document can be written.
    ///
    /// Carried by the listing rather than asked per document: the listing
    /// already walks the source and sees this on the way, and the question is
    /// asked when a list is drawn, not when a write is attempted. A viewer that
    /// had to ask separately would either make one request per row or show
    /// every row as editable and change its mind later.
    pub writable: bool,
}

/// Something that can say which documents it holds.
///
/// Listing costs a digest per document and never a full read, because a scan
/// runs whenever a project opens and content is fetched only when somebody
/// asks for a body.
pub trait DocumentSource {
    /// What this source can do.
    fn capabilities(&self) -> SourceCapabilities;

    /// Every document, with its digest. Ordered by locator.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] only when the source itself is unusable. A
    /// source that is simply not here — a folder this branch does not have —
    /// answers with nothing, because that is the honest answer and every
    /// record then reports its document is missing rather than being deleted.
    fn list(&self) -> Result<Vec<SourceDocument>, ServiceError>;

    /// One document's body, if it is there.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the source is unusable.
    fn read(&self, locator: &str) -> Result<Option<String>, ServiceError>;

    /// Whether the source's own history still has this document, even though
    /// it is not in the listing.
    ///
    /// For a repository folder: whether the checked-out commit has the path.
    /// This is what separates "another branch has it" from "somebody deleted
    /// it here", and the two deserve different treatment — the first is
    /// routine, the second is a decision.
    ///
    /// A source with no history answers `false`, which reads as "gone" and is
    /// the honest answer when there is nowhere else it could be.
    fn tracked(&self, locator: &str) -> bool;

    /// Write a document's bytes, creating it if it is not there.
    ///
    /// Bytes rather than text: a documentation folder holds diagrams and PDFs
    /// beside the Markdown, and a document that does not decode as UTF-8 is
    /// still a document.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] with kind `unsupported` when the source does
    /// not write, and `repository` when the write itself fails.
    fn write(&self, locator: &str, content: &[u8]) -> Result<(), ServiceError>;

    /// The folders the source has, whether or not anything Memory knows about
    /// is in them. Repository-relative and ordered, the attachment's own root
    /// among them: a record filed directly in `docs/` is in a folder like any
    /// other.
    ///
    /// Asked because aggregating the folders of known records cannot answer
    /// it. A directory on disk exists without our permission: it may be empty,
    /// hold nothing but files outside the mask, or hold only documents this
    /// branch hides. A person sees all three in their file tree and in a pull
    /// request, and a tree drawn from records alone would show none of them.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] only when the source itself is unusable.
    fn folders(&self) -> Result<Vec<String>, ServiceError>;
}

/// A directory of the repository, and the files in it that belong to one type.
pub struct FolderSource<'a> {
    project: &'a std::path::Path,
    /// Owned: an attachment is two strings, and a source is often built from
    /// one resolved on the spot rather than one somebody is holding.
    attachment: Attachment,
    /// Resolved on the first question about `HEAD`, and only then: a scan that
    /// finds everything where it left it never opens the repository at all.
    repository: std::cell::OnceCell<Option<git2::Repository>>,
}

impl<'a> FolderSource<'a> {
    #[must_use]
    pub const fn new(project: &'a std::path::Path, attachment: Attachment) -> Self {
        Self {
            project,
            attachment,
            repository: std::cell::OnceCell::new(),
        }
    }

    /// Walk the folder, at any depth.
    ///
    /// Depth is where the hierarchy comes from: a documentation folder always
    /// has nested directories, and flattening them would collapse `guides/`
    /// and `api/` into one list. The mask still names files, so nesting is the
    /// walk's business and never the mask's.
    ///
    /// Symbolic links are read as what they are and never followed. Following
    /// one that points outside the repository would pull somebody else's files
    /// into the corpus under a locator that looks local, and following one that
    /// points at an ancestor would not terminate. Neither is a state a person
    /// attaching `docs/` is asking for.
    fn collect(&self, directory: &std::path::Path, depth: usize, into: &mut Vec<SourceDocument>) {
        if depth > MAX_ATTACHMENT_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                self.collect(&path, depth + 1, into);
                continue;
            }
            let Ok(relative) = path.strip_prefix(self.project) else {
                continue;
            };
            let Some(locator) = relative.to_str() else {
                continue;
            };
            if !self.attachment.covers(locator) {
                continue;
            }
            // Hashed as bytes, not as text. A folder of documentation holds
            // diagrams and PDFs next to the Markdown, and a document that does
            // not decode as UTF-8 is still a document: it moves, it is edited,
            // it goes missing. What cannot be done with it is full-text search,
            // and that is the index's business rather than the scan's.
            let Ok(content) = std::fs::read(&path) else {
                continue;
            };
            // Seen on the way past, at no extra cost: the walk already has the
            // metadata, and asking the filesystem again per document would be
            // one syscall per row of somebody's list.
            let writable = entry
                .metadata()
                .is_ok_and(|metadata| !metadata.permissions().readonly());
            into.push(SourceDocument {
                locator: locator.to_owned(),
                hash: ContentHash::for_bytes(&content),
                writable,
            });
        }
    }
}

impl FolderSource<'_> {
    /// Collect directories under `directory`, on the same terms as the walk
    /// for documents: symbolic links are not followed, and depth is bounded.
    fn collect_folders(&self, directory: &std::path::Path, depth: usize, into: &mut Vec<String>) {
        if depth > MAX_ATTACHMENT_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(self.project) else {
                continue;
            };
            let Some(folder) = relative.to_str() else {
                continue;
            };
            into.push(folder.to_owned());
            self.collect_folders(&path, depth + 1, into);
        }
    }
}

/// How deep an attached folder is walked.
///
/// Far below any documentation tree and far above nothing: the bound exists so
/// a pathological directory structure cannot exhaust the stack, not to express
/// a limit anybody should meet.
const MAX_ATTACHMENT_DEPTH: usize = 64;

impl DocumentSource for FolderSource<'_> {
    fn capabilities(&self) -> SourceCapabilities {
        // A folder of the working tree is ours to write into: the files are
        // ordinary repository files, and creating one is what publishing a
        // record here means. Whether a *particular* file can be written is a
        // question about that file, and travels with it in the listing.
        SourceCapabilities { writable: true }
    }

    fn write(&self, locator: &str, content: &[u8]) -> Result<(), ServiceError> {
        if !self.attachment.covers(locator) {
            return Err(ServiceError::new(
                "invalid_argument",
                "that locator is not inside this storage",
                serde_json::json!({
                    "locator": locator,
                    "folder": self.attachment.folder,
                }),
            ));
        }
        let path = self.project.join(locator);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ServiceError::new(
                    "repository",
                    format!("could not create {}: {error}", parent.display()),
                    serde_json::json!({"locator": locator}),
                )
            })?;
        }
        std::fs::write(&path, content).map_err(|error| {
            ServiceError::new(
                "repository",
                format!("could not write {}: {error}", path.display()),
                serde_json::json!({"locator": locator}),
            )
        })
    }

    fn list(&self) -> Result<Vec<SourceDocument>, ServiceError> {
        let mut documents = Vec::new();
        self.collect(
            &self.project.join(&self.attachment.folder),
            0,
            &mut documents,
        );
        documents.sort_by(|left, right| left.locator.cmp(&right.locator));
        Ok(documents)
    }

    fn folders(&self) -> Result<Vec<String>, ServiceError> {
        let root = self.project.join(&self.attachment.folder);
        let mut folders = Vec::new();
        // The attachment root is one of its own folders. A record filed
        // directly in `docs/` is in a folder like any other, and leaving it out
        // would make the one folder that certainly exists the one a tree cannot
        // show.
        if root.is_dir() {
            folders.push(self.attachment.folder.clone());
        }
        self.collect_folders(&root, 0, &mut folders);
        folders.sort();
        folders.dedup();
        Ok(folders)
    }

    fn read(&self, locator: &str) -> Result<Option<String>, ServiceError> {
        Ok(std::fs::read_to_string(self.project.join(locator)).ok())
    }

    fn tracked(&self, locator: &str) -> bool {
        // One tree lookup by path, not a walk: the question is only whether
        // this commit carries the document. The repository is discovered once
        // per source rather than once per record — this is asked for every
        // record whose document is absent, and discovery walks the filesystem
        // upwards, which made a branch switch pay for it in a loop.
        let Some(repository) = self
            .repository
            .get_or_init(|| git2::Repository::discover(self.project).ok())
            .as_ref()
        else {
            return false;
        };
        let Ok(head) = repository.head() else {
            return false;
        };
        let Ok(tree) = head.peel_to_tree() else {
            return false;
        };
        tree.get_path(std::path::Path::new(locator)).is_ok()
    }
}

/// What a scan concluded about one document or one record.
#[derive(Clone, Debug, PartialEq)]
pub enum ScanChange {
    /// Same locator, different bytes: edited in place.
    Edited {
        key: String,
        locator: String,
        hash: ContentHash,
    },
    /// Same bytes at a different locator: moved or renamed.
    Moved {
        key: String,
        from: String,
        to: String,
    },
    /// Nothing at the recorded locator, and why.
    Missing {
        key: String,
        locator: String,
        presence: Presence,
    },
    /// A document is back where a record said it was.
    Returned { key: String, locator: String },
    /// A document belonging to no record, and the records it might be a rename
    /// of.
    ///
    /// New and renamed-with-edit are indistinguishable — nothing about the
    /// document says which — so this is carried out to a person with the
    /// candidates ranked, never guessed at.
    Unmatched {
        locator: String,
        hash: ContentHash,
        candidates: Vec<RenameCandidate>,
    },
    /// A document belonging to no record, with nothing plausible it could be a
    /// rename of.
    New {
        /// The key the record will be born with — named here so a client told
        /// about the change can re-read it.
        key: String,
        locator: String,
        hash: ContentHash,
    },
}

/// A record a stray document might be the new location of.
#[derive(Clone, Debug, PartialEq, PartialOrd)]
pub struct RenameCandidate {
    pub key: String,
    pub locator: String,
    /// Between 0 and 1. Ranking only — the decision belongs to a person.
    ///
    /// Scored on the locator, not the content: Memory keeps no copy of what
    /// the vanished document said, and a digest does not run backwards.
    pub similarity: f32,
}

/// How alike a candidate must be before it is offered at all.
///
/// Git's default for rename detection, and for the same reason: without a
/// floor, every unrelated record whose document is absent becomes a candidate,
/// so adding one file while another is missing on a branch you switched away
/// from would raise a question about two documents that have nothing to do
/// with each other.
pub const RENAME_THRESHOLD: f32 = 0.5;

/// One record the scan is reconciling against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KnownRecord {
    pub key: String,
    pub locator: String,
    pub hash: ContentHash,
    pub presence: Presence,
}

/// Reconcile what a source holds with what Memory recorded.
///
/// Locator match wins over digest match: an edit in place is a stronger
/// statement than two documents that happen to hash alike, and reading it as a
/// move would invent a rename nobody performed.
///
/// `tracked` answers whether the source's history still has a locator that is
/// not in the listing. It is a function rather than a second list because the
/// question is only asked about the handful of records whose document is
/// absent, and answering it for every document would cost a walk.
#[must_use]
pub fn classify(
    documents: &[SourceDocument],
    records: &[KnownRecord],
    tracked: &dyn Fn(&str) -> bool,
    key_for: &dyn Fn(&str) -> String,
) -> Vec<ScanChange> {
    let by_locator: BTreeMap<&str, &KnownRecord> = records
        .iter()
        .map(|record| (record.locator.as_str(), record))
        .collect();
    let present_locators: BTreeSet<&str> = documents
        .iter()
        .map(|document| document.locator.as_str())
        .collect();

    // Only a record whose own locator is gone can be the far end of a move.
    let movable: Vec<&KnownRecord> = records
        .iter()
        .filter(|record| !present_locators.contains(record.locator.as_str()))
        .collect();

    // Digest first, so a renamed directory costs one lookup per file instead
    // of a scan of every absent record.
    //
    // Only records that were here at the last scan take part. A record already
    // marked absent has settled: its document is on another branch or was
    // deleted, and a file appearing somewhere else later is not evidence that
    // it moved. Without that restriction any new placeholder — an empty file,
    // a copied template — could carry a settled record's key onto itself,
    // because bytes that are not distinctive are not an identity. Records that
    // have settled are still offered as rename candidates, where a person
    // decides.
    let mut by_hash: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, record) in movable.iter().enumerate() {
        if record.presence.is_present() {
            by_hash.entry(record.hash.as_str()).or_default().push(index);
        }
    }

    let mut changes = Vec::new();
    let mut accounted: BTreeSet<&str> = BTreeSet::new();
    let mut taken = vec![false; movable.len()];

    for document in documents {
        if let Some(record) = by_locator.get(document.locator.as_str()) {
            accounted.insert(&record.key);
            if record.hash == document.hash {
                if record.presence.is_absent() {
                    changes.push(ScanChange::Returned {
                        key: record.key.clone(),
                        locator: record.locator.clone(),
                    });
                }
            } else {
                changes.push(ScanChange::Edited {
                    key: record.key.clone(),
                    locator: record.locator.clone(),
                    hash: document.hash.clone(),
                });
            }
            continue;
        }

        // Several records can share a digest — boilerplate, stubs, empty
        // documents — and a renamed directory presents all of them at once.
        // Pairing by the longest common locator suffix is what keeps
        // `guides/api/index.md` with `handbook/api/index.md` instead of with
        // `handbook/cli/index.md`, which would carry a key onto the wrong
        // document and leave every link pointing at the wrong text.
        let candidates_by_hash = by_hash
            .get(document.hash.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let best = candidates_by_hash
            .iter()
            .copied()
            .filter(|index| !taken[*index])
            .max_by_key(|index| {
                common_suffix_segments(&movable[*index].locator, &document.locator)
            });
        if let Some(index) = best {
            taken[index] = true;
            let record = movable[index];
            accounted.insert(&record.key);
            changes.push(ScanChange::Moved {
                key: record.key.clone(),
                from: record.locator.clone(),
                to: document.locator.clone(),
            });
            continue;
        }

        let mut candidates: Vec<RenameCandidate> = movable
            .iter()
            .enumerate()
            .filter(|(index, _)| !taken[*index])
            .map(|(_, record)| RenameCandidate {
                key: record.key.clone(),
                locator: record.locator.clone(),
                similarity: similarity(&record.locator, &document.locator),
            })
            .filter(|candidate| candidate.similarity >= RENAME_THRESHOLD)
            .collect();
        candidates.sort_by(|left, right| {
            right
                .similarity
                .total_cmp(&left.similarity)
                .then_with(|| left.key.cmp(&right.key))
        });

        if candidates.is_empty() {
            changes.push(ScanChange::New {
                key: key_for(&document.locator),
                locator: document.locator.clone(),
                hash: document.hash.clone(),
            });
        } else {
            changes.push(ScanChange::Unmatched {
                locator: document.locator.clone(),
                hash: document.hash.clone(),
                candidates,
            });
        }
    }

    changes.extend(absences(records, &accounted, tracked));
    changes
}

/// Why each record the listing did not account for is absent.
///
/// `tracked` is asked only here, for the handful of records whose document is
/// not in the listing — answering it for every document would cost a walk.
fn absences(
    records: &[KnownRecord],
    accounted: &BTreeSet<&str>,
    tracked: &dyn Fn(&str) -> bool,
) -> Vec<ScanChange> {
    records
        .iter()
        .filter(|record| !accounted.contains(record.key.as_str()))
        .filter_map(|record| {
            let presence = if tracked(&record.locator) {
                Presence::Removed
            } else {
                Presence::NotOnBranch
            };
            (record.presence != presence).then(|| ScanChange::Missing {
                key: record.key.clone(),
                locator: record.locator.clone(),
                presence,
            })
        })
        .collect()
}

/// A readable key for a document that is new to Memory.
///
/// Derived from the locator once, at birth, and never again: a move carries the
/// locator and leaves the key alone, because links point at the key and a key
/// that followed the document would break every one of them.
///
/// The whole locator below the attachment root is used, not just the file
/// name: with nested folders `guides/api/auth.md` and `cli/auth.md` would
/// otherwise both want to be `auth`.
#[must_use]
pub fn key_for(attachment: &Attachment, locator: &str) -> String {
    let below = locator
        .strip_prefix(&format!("{}/", attachment.folder))
        .unwrap_or(locator);
    let stem = below.rsplit_once('.').map_or(below, |(stem, _)| stem);
    let slug: String = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        locator.replace('/', "-")
    } else {
        slug
    }
}

/// The first key derived from `base` that nothing else has.
///
/// A key is derived from a locator, and derivation is lossy: every character
/// that is not `[a-z0-9_-]` becomes `-`, so `getting started.md` and
/// `getting-started.md` arrive at the same answer. Two conclusions with one key
/// are a transaction that touches a record twice, which is refused — so an
/// ordinary pair of file names would stop the folder being scanned at all. And
/// a derived key that happens to match a key somebody wrote by hand would take
/// that record over.
///
/// The suffix is decided once, when the record is born, and never again: the
/// key does not follow the file afterwards.
#[must_use]
pub fn unique_key(base: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_owned();
    }
    // Bounded because an unbounded search here would be a loop nobody can
    // explain: the ordinal counts documents whose names slug alike, and a
    // folder with that many of them has a naming problem this cannot solve.
    (2_u32..=MAX_KEY_ORDINAL)
        .map(|ordinal| format!("{base}-{ordinal}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_owned())
}

/// How many documents may derive the same key before the scan gives up trying
/// to tell them apart.
const MAX_KEY_ORDINAL: u32 = 1_000;

/// The directory rename a set of scan changes agrees on, if they agree on one.
///
/// A directory rename reaches the scan as N document moves and nothing else —
/// there is no event that says a directory was renamed. What can be recovered
/// from those moves is the pair of prefixes every one of them shares, and only
/// while they all share it: a scan that also carries a move nobody would call
/// part of the rename is not evidence about a directory, it is several things
/// happening at once.
///
/// Recovered because a record filed in that folder by metadata alone — a
/// decision filed next to the documents, the record that is the folder — has
/// no file to follow and would be left pointing at a path that no longer
/// exists. When the moves do not agree, nothing is touched and the state is
/// left for `doctor`, which is the honest outcome of a guess declined.
#[must_use]
pub fn directory_rename(changes: &[ScanChange]) -> Option<(String, String)> {
    let mut agreed: Option<(String, String)> = None;
    for change in changes {
        let ScanChange::Moved { from, to, .. } = change else {
            continue;
        };
        let shared = common_suffix_segments(from, to);
        let head = |path: &str| {
            let segments: Vec<&str> = path.split('/').collect();
            segments
                .get(..segments.len().saturating_sub(shared))
                .map(|head| head.join("/"))
        };
        let (Some(old), Some(new)) = (head(from), head(to)) else {
            return None;
        };
        // A file renamed where it lies says nothing about its directory.
        if old == new || old.is_empty() || new.is_empty() {
            return None;
        }
        match &agreed {
            Some((seen_old, seen_new)) if *seen_old != old || *seen_new != new => return None,
            Some(_) => {}
            None => agreed = Some((old, new)),
        }
    }
    agreed
}

/// How many trailing path segments two locators share.
fn common_suffix_segments(left: &str, right: &str) -> usize {
    left.rsplit('/')
        .zip(right.rsplit('/'))
        .take_while(|(left, right)| left == right)
        .count()
}

/// How alike two locators are, between 0 and 1.
///
/// Shared trailing segments dominate — a renamed directory leaves the file
/// name and often several segments untouched — and the file names are compared
/// character by character to separate candidates that share none.
fn similarity(left: &str, right: &str) -> f32 {
    if left == right {
        return 1.0;
    }
    let shared = common_suffix_segments(left, right);
    if shared > 0 {
        let depth = left.split('/').count().max(right.split('/').count());
        #[expect(
            clippy::cast_precision_loss,
            reason = "paths are short; the ratio ranks candidates, it does not measure"
        )]
        let ratio = shared as f32 / depth as f32;
        // Sharing the file name alone already makes a rename plausible, which
        // is what a directory rename looks like from here.
        return 0.5 + ratio / 2.0;
    }
    name_similarity(left, right)
}

fn name_similarity(left: &str, right: &str) -> f32 {
    let left = left.rsplit_once('/').map_or(left, |(_, name)| name);
    let right = right.rsplit_once('/').map_or(right, |(_, name)| name);
    let longest = left.chars().count().max(right.chars().count());
    if longest == 0 {
        return 1.0;
    }
    let distance = edit_distance(left, right);
    #[expect(
        clippy::cast_precision_loss,
        reason = "file names are short; the ratio ranks candidates, it does not measure"
    )]
    let similarity = 1.0 - (distance as f32 / longest as f32);
    similarity
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != *right_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}
