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

/// Where a type's documents are, in the working tree.
///
/// A folder and nothing else — no file-name mask, deliberately: a person who
/// put a diagram in their documentation folder should see the diagram in
/// Memory. A mask decides which files are worth having,
/// and that is not a decision a corpus gets to make about somebody's project —
/// what we cannot render is a question for the viewer, not for the scan.
///
/// One folder holds one type, so nothing has to be told apart inside it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Attachment {
    /// Directory, relative to the project root.
    pub folder: String,
}

/// Names the operating system leaves behind. Nobody put them there and nobody
/// will miss them; every other file is somebody's.
const LITTER: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

impl Attachment {
    #[must_use]
    pub const fn new(folder: String) -> Self {
        Self { folder }
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

    /// Take a document out of the source.
    ///
    /// The other half of [`Self::write`], and the reason it exists: a record
    /// whose body is a document owns that document, so deleting the record has
    /// to take it. A source that kept the file would leave something the next
    /// scan reads as a document belonging to no record — and gives back a
    /// record for, with a new key and none of the links the old one had.
    ///
    /// A locator that is already not here is the state this was asked for, and
    /// answers `Ok`. The three ways that happens are all ordinary: the file is
    /// on another branch, somebody deleted it by hand, or this ran once before
    /// and was interrupted after the file and before the record.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] with kind `unsupported` when the source does
    /// not write, and `repository` when the removal itself fails.
    fn remove(&self, locator: &str) -> Result<(), ServiceError>;

    /// Move a document from one locator to another, keeping its bytes.
    ///
    /// Its own operation rather than a read, a write and a removal, and for
    /// two reasons. A document is whatever the folder holds — a diagram, a PDF
    /// — and the read that answers text would return nothing for most of them;
    /// rewriting the bytes to move a file is also work proportional to the
    /// document when the filesystem can do it in one step.
    ///
    /// **Nothing is overwritten.** A destination that is already occupied is
    /// refused, because the two documents are two records and the move would
    /// silently take one of them with it.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] with kind `unsupported` when the source does
    /// not write, `invalid_argument` when either locator is outside this
    /// storage or the destination is taken, and `repository` when the move
    /// itself fails.
    fn relocate(&self, from: &str, to: &str) -> Result<(), ServiceError>;

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
    /// A locator for a path inside the project, always `/`-separated.
    ///
    /// A locator is a repository path, not a filesystem one, and three separate
    /// things go on to read it that way: the attachment's own prefix is matched
    /// against it, `tracked` hands it to Git, and it is stored in a record that
    /// travels to machines with other separators. Windows hands out `\` here,
    /// and each of those three then quietly fails to match — a scan that walks
    /// the whole folder, discards every file, and reports nothing found without
    /// raising anything. `memory_hub_folder::paths::record_id` reads a path the
    /// same way and for the same reason.
    fn locator_for(&self, path: &std::path::Path) -> Option<String> {
        let relative = path.strip_prefix(self.project).ok()?;
        let mut parts = Vec::new();
        for component in relative.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_str()?),
                _ => return None,
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

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
            let Some(locator) = self.locator_for(&path) else {
                continue;
            };
            if !self.attachment.covers(&locator) {
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
                locator,
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
            let Some(folder) = self.locator_for(&path) else {
                continue;
            };
            into.push(folder);
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

    fn remove(&self, locator: &str) -> Result<(), ServiceError> {
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
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            // Already gone is what this was asked for.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ServiceError::new(
                "repository",
                format!("could not remove {}: {error}", path.display()),
                serde_json::json!({"locator": locator}),
            )),
        }
        // The directory it was in is left alone, empty or not. Git keeps no
        // empty directories, so an empty one is a fact about this working tree
        // and nothing anybody clones; removing it would also, for the last
        // document in an attachment, delete the folder somebody attached.
    }

    fn relocate(&self, from: &str, to: &str) -> Result<(), ServiceError> {
        for locator in [from, to] {
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
        }

        let source = self.project.join(from);
        let destination = self.project.join(to);

        // Asked before the move rather than relied on afterwards: `rename`
        // replaces an existing file on Unix without a word, and the file it
        // would replace is another record's document.
        if destination.exists() {
            return Err(ServiceError::new(
                "invalid_argument",
                "a document is already at that locator",
                serde_json::json!({"locator": to}),
            ));
        }

        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ServiceError::new(
                    "repository",
                    format!("could not create {}: {error}", parent.display()),
                    serde_json::json!({"locator": to}),
                )
            })?;
        }

        std::fs::rename(&source, &destination).map_err(|error| {
            ServiceError::new(
                "repository",
                format!(
                    "could not move {} to {}: {error}",
                    source.display(),
                    destination.display()
                ),
                serde_json::json!({"from": from, "to": to}),
            )
        })
        // The directory it came from is left alone for the same reason
        // `remove` leaves it: Git keeps no empty directories, and the last
        // document out of an attachment root must not take the attachment
        // with it.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Attachment, DocumentSource as _, FolderSource};

    /// A locator is a repository path, and a repository path is `/`-separated
    /// on every platform.
    ///
    /// This is a Windows test that runs everywhere. There, a locator was built
    /// straight from the filesystem path, so it arrived as `docs\guide.md`; the
    /// attachment's own prefix is matched with `docs/`, so it matched nothing,
    /// and the walk discarded every file it had just found. The failure had no
    /// error in it at all — a scan that reported an empty folder while looking
    /// at a full one. Asserting the separator here is what makes that a test
    /// failure on the platform it happens on rather than an empty answer.
    #[test]
    fn a_locator_is_separated_the_way_a_repository_is() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join("docs/nested")).unwrap();
        std::fs::write(project.path().join("docs/guide.md"), b"a body").unwrap();
        std::fs::write(project.path().join("docs/nested/deeper.md"), b"another").unwrap();

        let source = FolderSource::new(project.path(), Attachment::new("docs".to_owned()));
        let locators: Vec<String> = source
            .list()
            .unwrap()
            .into_iter()
            .map(|document| document.locator)
            .collect();

        assert_eq!(
            locators,
            vec![
                "docs/guide.md".to_owned(),
                "docs/nested/deeper.md".to_owned()
            ],
            "both the file and the one a directory deep are named with `/`"
        );
    }
}
