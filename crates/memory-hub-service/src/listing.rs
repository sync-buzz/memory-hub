use std::cmp::Ordering;
use std::collections::BTreeMap;

use memory_hub_core::Envelope;
use memory_hub_core::Presence;
use memory_hub_schema::TYPE_KIND;
use memory_hub_store::Revision;

use crate::freshness_str;

/// What a listing selects, in what order, and how much of each record it keeps.
///
/// Filters are combined with AND, and `tags` requires every listed tag. The defaults are the
/// ones a client gets by omitting the fields: the first 50 records by key,
/// ascending, with full bodies.
// A query is a flat set of independent switches, and each of these is one a
// caller sets on its own. Grouping them to satisfy a count would put a name
// between the caller and the thing it is asking for.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug)]
pub struct ListingQuery {
    pub limit: usize,
    pub offset: usize,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub archived: Option<bool>,
    pub freshness: Vec<String>,
    /// Restrict to a folder. `Some(String::new())` is the root — records filed
    /// nowhere — which is a different question from "no filter".
    pub folder: Option<String>,
    /// Whether the folder filter reaches below the folder it names.
    pub folder_subtree: bool,
    /// Whether records whose content is not here are included.
    pub presence: PresenceFilter,
    /// Whether records that are Memory's own machinery are included.
    ///
    /// A type definition is schema, not knowledge: returning it to somebody who
    /// asked what the project knows answers a question they did not ask, and
    /// obliges every client to learn about a kind it has no use for. It is
    /// still reachable — by asking for its kind, or by raising this — because
    /// the tools that maintain schema exist.
    pub include_service: bool,
    /// Whether the records that *are* folders are listed among the documents.
    ///
    /// They are not, by default. A record carrying `is_folder` is the folder
    /// its type's documents are filed in rather than one of them, so returning
    /// it beside them answers a question nobody asked and makes every client
    /// that draws a list filter it out again — once, correctly, in each of
    /// them. What a folder has to say is reached through `memory_list_folders`,
    /// which names it, and through search, which finds its text like any other.
    pub include_folders: bool,
    pub sort: ListingSort,
    pub descending: bool,
    pub metadata_only: bool,
}

/// Which records a listing keeps, by whether their content is here.
///
/// Memory does not branch and code does, so a record whose document lives on
/// another branch is a normal state rather than a broken one. Hiding it by
/// default is what keeps a switch to `main` from filling the list with every
/// feature branch's documentation; asking for it explicitly is always
/// possible, and it comes back saying why it was hidden.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PresenceFilter {
    /// The default: everything except records this branch simply does not have.
    ///
    /// Not "only records whose content is here". A document deleted on the
    /// branch that owns it is the one case a person is asked about, and asking
    /// somebody about a record they cannot see is not asking. What is hidden is
    /// the routine absence — another branch has it — which is the noise this
    /// filter exists to suppress.
    #[default]
    Present,
    /// Everything, present or not.
    Any,
    /// Only records whose content is not here, for either reason.
    Absent,
}

impl PresenceFilter {
    /// Parse the name a client sends. An unrecognised name reads as the
    /// default, which is what every client that has never heard of this sends
    /// by omitting it.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "any" => Self::Any,
            "absent" => Self::Absent,
            _ => Self::Present,
        }
    }

    #[must_use]
    const fn admits(self, presence: Presence) -> bool {
        match self {
            Self::Any => true,
            Self::Present => !matches!(presence, Presence::NotOnBranch),
            Self::Absent => presence.is_absent(),
        }
    }
}

/// The field a listing is ordered by. An unrecognised name is not an error to
/// the caller — it falls back to `Key`, which is what the interface has always
/// done.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ListingSort {
    #[default]
    Key,
    Kind,
    Title,
    Freshness,
    Archived,
}

impl ListingSort {
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        match name {
            "kind" => Self::Kind,
            "title" => Self::Title,
            "freshness" => Self::Freshness,
            "archived" => Self::Archived,
            _ => Self::Key,
        }
    }
}

impl Default for ListingQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            kind: None,
            tags: Vec::new(),
            archived: None,
            freshness: Vec::new(),
            folder: None,
            folder_subtree: false,
            presence: PresenceFilter::Present,
            include_service: false,
            include_folders: false,
            sort: ListingSort::Key,
            descending: false,
            metadata_only: false,
        }
    }
}

impl ListingQuery {
    /// Highest page size the interface serves, regardless of what was asked.
    pub const MAX_LIMIT: usize = 200;

    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.min(Self::MAX_LIMIT);
        self
    }

    fn matches(&self, envelope: &Envelope) -> bool {
        if let Some(ref kind) = self.kind
            && &envelope.kind != kind
        {
            return false;
        }
        if envelope.kind == TYPE_KIND
            && !self.include_service
            && self.kind.as_deref() != Some(TYPE_KIND)
        {
            return false;
        }
        if envelope.is_folder && !self.include_folders {
            return false;
        }
        if !self
            .tags
            .iter()
            .all(|tag| envelope.tags.iter().any(|candidate| candidate == tag))
        {
            return false;
        }
        if let Some(archived) = self.archived
            && envelope.archive.archived != archived
        {
            return false;
        }
        if !self.freshness.is_empty() {
            let state = freshness_str(envelope.freshness.state);
            if !self.freshness.iter().any(|filter| filter == state) {
                return false;
            }
        }
        if let Some(folder) = &self.folder
            && !folder_matches(folder, self.folder_subtree, envelope.folder.as_deref())
        {
            return false;
        }
        // A record that holds its own content is always here; only a record
        // that points somewhere else can fail to be.
        let presence = envelope
            .content_ref
            .as_ref()
            .map_or(Presence::Present, |reference| reference.presence);
        if !self.presence.admits(presence) {
            return false;
        }
        true
    }

    fn compare(&self, left: &(String, Envelope), right: &(String, Envelope)) -> Ordering {
        let ordering = match self.sort {
            ListingSort::Kind => left.1.kind.cmp(&right.1.kind),
            ListingSort::Title => left
                .1
                .title
                .as_deref()
                .unwrap_or("")
                .cmp(right.1.title.as_deref().unwrap_or("")),
            ListingSort::Freshness => {
                freshness_str(left.1.freshness.state).cmp(freshness_str(right.1.freshness.state))
            }
            ListingSort::Archived => left.1.archive.archived.cmp(&right.1.archive.archived),
            ListingSort::Key => left.0.cmp(&right.0),
        };
        if self.descending {
            ordering.reverse()
        } else {
            ordering
        }
    }

    /// Filter, count, sort and page a corpus in one pass.
    #[must_use]
    pub fn apply(&self, revision: Revision, envelopes: &[(String, Envelope)]) -> Listing {
        let mut matched: Vec<&(String, Envelope)> = envelopes
            .iter()
            .filter(|(_, envelope)| self.matches(envelope))
            .collect();
        let counts = ListingCounts::over(&matched);
        let total = matched.len();
        matched.sort_by(|left, right| self.compare(left, right));
        let records = matched
            .into_iter()
            .skip(self.offset)
            .take(self.limit)
            .cloned()
            .collect();
        Listing {
            revision,
            records,
            total,
            limit: self.limit,
            offset: self.offset,
            has_more: total.saturating_sub(self.offset) > self.limit,
            counts,
            metadata_only: self.metadata_only,
        }
    }
}

/// One page of a listing, plus the shape of everything the filters selected.
#[derive(Clone, Debug)]
pub struct Listing {
    pub revision: Revision,
    pub records: Vec<(String, Envelope)>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
    pub counts: ListingCounts,
    /// The caller asked for metadata only; rendering the page is the adapter's
    /// job, so this travels with the result rather than being applied here.
    pub metadata_only: bool,
}

/// Counts over the full matched set, so a paged response still describes the
/// whole selection.
///
/// The selection, not the corpus: every filter the query carries has already
/// been applied, including the presence filter, so a record hidden because this
/// branch does not have its document is not counted here. The corpus-wide
/// answer is [`ListingCounts::over_corpus`], which counts everything and is
/// what a records summary reports.
#[derive(Clone, Debug, Default)]
pub struct ListingCounts {
    pub total: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_freshness: BTreeMap<String, usize>,
    pub archived: usize,
    pub live: usize,
    /// Records that are Memory's own machinery — type definitions today.
    ///
    /// In a listing, how many of the selected records are machinery: zero
    /// unless they were asked for, since they are left out by default. In a
    /// records summary it is how many exist at all, and there it is in none of
    /// the other numbers — a count of documents that quietly includes schema is
    /// a count nobody asked for, and somebody who does want it should not have
    /// to subtract.
    pub service: usize,
}

impl ListingCounts {
    fn over(matched: &[&(String, Envelope)]) -> Self {
        let mut counts = Self {
            total: matched.len(),
            ..Self::default()
        };
        for (_, envelope) in matched {
            if envelope.kind == TYPE_KIND {
                counts.service += 1;
            }
            *counts.by_kind.entry(envelope.kind.clone()).or_default() += 1;
            *counts
                .by_freshness
                .entry(freshness_str(envelope.freshness.state).to_owned())
                .or_default() += 1;
            if envelope.archive.archived {
                counts.archived += 1;
            }
        }
        counts.live = counts.total - counts.archived;
        counts
    }

    /// Counts over an entire corpus, ignoring paging **and every filter** —
    /// what a records summary reports. A record whose document is on another
    /// branch is counted here and not in a listing, which is the difference
    /// between "what is in memory" and "what this branch is showing you".
    ///
    /// Type definitions are the exception: schema is not a document, so it is
    /// counted in `service` and in none of the other numbers.
    #[must_use]
    pub fn over_corpus(envelopes: &[(String, Envelope)]) -> Self {
        let documents: Vec<&(String, Envelope)> = envelopes
            .iter()
            .filter(|(_, envelope)| envelope.kind != TYPE_KIND)
            .collect();
        let mut counts = Self::over(&documents);
        counts.service = envelopes.len() - documents.len();
        counts
    }
}

/// One folder, and everything known about it from every source at once.
///
/// The two origins are separate booleans rather than one value because they
/// mean different things to whoever draws the tree: storage without records is
/// an empty directory somebody can file into, records without storage is a
/// folder whose documents this branch does not have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FolderEntry {
    /// Repository-relative, `""` for the root.
    pub path: String,
    /// At least one record is filed here.
    pub in_records: bool,
    /// The storage has this directory, whatever is in it.
    pub in_storage: bool,
    /// How many documents are filed directly in it — not counting what is in
    /// the folders below, and not counting type definitions, which are schema
    /// rather than documents.
    pub records: usize,
    /// The key of the record that stands for this folder, if one does.
    pub described: Option<String>,
}

impl FolderEntry {
    /// A folder nothing is known about yet, named.
    pub(crate) fn empty(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            in_records: false,
            in_storage: false,
            records: 0,
            described: None,
        }
    }
}

/// Whether a folder path satisfies a folder filter — the same predicate a
/// record's own folder is held to, so `memory_list_folders` and
/// `memory_list_records` select the same region of the tree.
#[must_use]
pub fn folder_in_scope(wanted: &str, subtree: bool, path: &str) -> bool {
    folder_matches(wanted, subtree, (!path.is_empty()).then_some(path))
}

/// Whether a record's folder satisfies a folder filter.
///
/// The root is `""` and holds records filed nowhere. Asking for the root and
/// everything below it is asking for the whole corpus, which is why that case
/// admits every record rather than only the unfiled ones.
fn folder_matches(wanted: &str, subtree: bool, actual: Option<&str>) -> bool {
    if wanted.is_empty() {
        return subtree || actual.is_none();
    }
    let Some(actual) = actual else {
        return false;
    };
    actual == wanted || (subtree && actual.starts_with(&format!("{wanted}/")))
}
