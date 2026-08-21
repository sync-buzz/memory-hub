# Documents in the repository

A type can keep its records' content in the repository's own files, so the
things a team already writes and reviews become records without changing how
they are written or reviewed.

A type can declare that its records' content lives in a folder of the
repository, by naming a storage the project has declared:

```json
{ "kind_name": "doc", "storage": "docs" }
```

Those files are ordinary repository files. Git versions them, a pull request
shows them in its diff, and review happens where review already happens. Memory
Hub writes **nothing** into them — not a marker, not an id in frontmatter —
so a colleague who has never heard of Memory sees a repository that has not
changed.

Identity therefore lives in the record, not in the file: the record holds the
key, the locator, the digest, the title, the tags, the links and the freshness,
and the file holds only its text.

**Every file in the folder belongs to the type**, whatever it is called: a
diagram sitting beside the Markdown is one of that type's documents too,
because a person opening the folder sees it there and expects Memory to know
about it. One content storage holds one type, so two types wanting folders want
two folders — a file belongs to one type, and a name pattern deciding which
would answer that question differently for every file somebody adds.

`memory-hub` reconciles a folder when the project opens and whenever
`memory_scan` is called. A client that can see window focus or watch the
filesystem should ask again on those — before every read is too expensive, and
only at open is too rare for somebody editing files in the next window.

Four outcomes are unambiguous and applied without asking:

| On disk | Conclusion |
| --- | --- |
| same path, different bytes | edited in place — the digest is updated and freshness drops to `unverified`, because the claim was checked against a text that has changed |
| same bytes, different path | moved — the locator follows, the key does not, so every link pointing at it still resolves. Only a record whose document was there at the last scan can be the far end of a move: bytes that are not distinctive (an empty file, a copied template) must not carry a settled record's key onto a document nobody moved |
| nothing at the recorded path | `missing` — the record and its links stay |
| a file back where a record said it was | returned, with all its metadata |

The fifth is not, and is never guessed. A file matching no record may be new or
may be a rename with an edit; nothing about the file says which. It is reported
with the records it could be, ranked by name similarity, and waits for a
person. `memory_doctor` lists everything in that state.

A document that is not text — a diagram, a PDF, a video — is still a document.
It is scanned, moved and tracked like the rest, because those questions are
about bytes and not about words. What it cannot be is searched by its contents,
so a hit reports `content_kind: "binary"` rather than looking like an empty
document.

Each record carries a `media_type`, decided from the file name and never from
the bytes: reading every document on every scan would be expensive, and the
answer would change under somebody who is mid-save. A client picks an editor, a
viewer or a player from it before deciding to fetch anything.

Reading a document's body is `memory_read_content` and writing one is
`memory_write_content`. A read says what it returned — `encoding` is `utf-8`,
`base64` or `none` — so a diagram arrives as bytes rather than as a failure,
and a client that only understands text sees an encoding it does not recognise
rather than a string of replacement characters. Reading is the one operation that goes outside, and the
one that can answer `missing: true`; every other operation — listing, search,
export, diff — works from what Memory itself holds, so an unreachable
folder can never make one of them quietly return less.

A record whose file is gone stays `missing` indefinitely and is removed only by
an explicit deletion. Deleted, on another branch, and not pulled are
indistinguishable at the moment of looking, and two of the three are routine —
switching branches would otherwise destroy a feature branch's documentation
records every time.

## Deleting one

Deleting is one operation whatever the storage; what differs is how much of the
record there is to take. A record that keeps its body in itself is gone when its
envelope is. A record whose body is a document **owns that document**, so the
delete takes the file too — the file first, then the record, so an interruption
leaves a record reporting its document as gone, which `doctor` raises and a
person settles.

The caller says `delete` either way. Where the content lives is the project's
own declaration and never a parameter of the operation:

```json
{ "op": "delete", "key": "guide" }
```

Leaving the file would not be a smaller deletion, it would be a deletion that
undoes itself: the next scan finds a document belonging to no record and hands
back a record for it, with a key derived from the path and none of the links the
old one had.

**Removing a type is the other operation, and the difference is the working
tree.** `memory_delete_type` takes a type's definition, every record of it, and
— for a type whose content lived in a declared directory — that declaration.
Every file stays where it is: those documents were in the repository before
Memory was asked about them, and the type was only what Memory knew. Deleting
the records one at a time is not the same thing, and it is not how a type is
removed.

## Folders

A record can carry a `folder` — a path of segments — and listing and search can
select one:

```json
{ "folder": "docs/guides", "folder_scope": "subtree" }
```

`folder_scope` is `exact` by default; `subtree` reaches below. `"folder": ""`
is the root, meaning records filed nowhere. Selection is a predicate the index
evaluates, so paging a subtree returns full pages.

Hierarchy is a **name, never a location**. In `refs` the tree stays flat and
hashed, so none of the problems of a physical hierarchy — case-insensitive
filesystems, path length, unicode normalization, reserved names — arrive with
it. Hierarchy is physical only where it already was without Memory Hub.

Folders are implicit: one exists while a record is in it. Nothing is orphaned
by a delete, the same way Git treats directories.

For a record in `refs` the folder is metadata a person sets. For a record whose
content is a repository file it is the directory that file is in, and the two
may not disagree: one fact, one place. Moving such a record means moving its
file, and a directory rename is simply every document in it moving at once —
the locators follow, the keys do not, so no link breaks.

That the two share one namespace is deliberate and worth knowing: nothing stops
a decision from being filed under `docs/guides` next to the documents. It will
sit there quite happily, and no file will appear for it.

### A folder with a title and a text of its own

A folder is a name until somebody gives it something to say. That is one field:

```json
{ "key": "api-guides",
  "kind": "guide",
  "title": "API guides",
  "content": "How authentication, limits and versioning work here.",
  "folder": "docs/guides/api",
  "is_folder": true }
```

**A record with `is_folder` is the folder it is filed in.** The folder it
stands for is its own `folder` and never a path of its own — a path named by a
second field follows nothing when the directory is renamed, while `folder`
already moves correctly, because every other record moves by it.

There is no folder type and no folder kind. Any type will do: a project that
wants folders with fields of their own — an `adr-index`, a `guide-section` —
declares an ordinary type and its schema is checked like any other. And because
the record is a document, it is listed, searched, counted and linked to as one.
Nothing has to learn about it to keep working.

Three consequences worth stating plainly:

- **One folder, one such record.** A second is refused at the write, naming the
  one that is already there. Two of them is not a conflict to resolve later, it
  is a question — which of the two is the folder — asked of every client that
  draws a tree.
- **A folder with nothing else in it is a real folder**, because the record that
  is it is in it. No special rule: the folder exists for the ordinary reason.
- **Deleting it deletes a description, not a folder.** The folder remains while
  anything else is filed there, and stops existing when nothing is, by the same
  rule as any other folder.

In an attached documentation folder, the record that is the folder is usually a
file that is already there: `docs/guides/api/README.md`. Marking it costs
nothing and it moves with its directory like any other document. A folder
description that must not depend on the branch goes in `refs` instead.

### Listing folders

Aggregating the folders of known records answers the question for `refs`, where
a folder cannot exist unnamed. It does not answer it for an attached directory,
which exists on disk with no permission from us: `docs/api/` may be empty, hold
nothing but files outside the mask, or hold only documents the current branch
hides. A person sees all three in their file tree and in a pull request.

`memory_list_folders` answers from both sources at once:

```json
{ "path": "docs/api",
  "in_records": false,
  "in_storage": true,
  "records": 0,
  "described_by": null }
```

The two origins are separate answers because they mean different things:
storage without records is an empty directory somebody can file into; records
without storage is a folder whose documents this branch does not have.
`described_by` is the key of the record that is the folder — a client that
draws a tree needs it, or it shows that record twice, once as the folder and
once as its own child.

**The directories are read live and never stored.** Git keeps no empty
directories, so an empty `docs/api/` is a fact about one working tree and is
simply absent from a fresh clone. A remembered list would raise, on one machine,
a folder that does not exist on another.

### Renaming a folder

In an attached folder, rename the directory. Git records it, the scan reads it
as every document in it moving at once, and the record that is the folder moves
with them. Records filed there by metadata alone — a decision next to the
documents — are carried along too, so long as the moves agree on one pair of
paths; when they do not, nothing is touched and `memory_doctor` reports it,
because a guess about somebody's directory is worse than a question.

In `refs` there is no directory to rename, so there is an operation:

```json
{ "from": "decisions/storage", "to": "decisions/persistence",
  "transaction_id": "rename-1" }
```

`memory_rename_folder` rewrites `folder` on every record under `from` in one
transaction. One, because N of them leave the folder half-renamed the moment
one fails. It is refused for a directory of an attached folder: renaming there
means renaming the directory, which a person does the ordinary way.

## Branches

Memory does not branch. `refs/memory/*` takes no part in branch merges, which
is deliberate — a record is knowledge about the project, not about a branch,
and branching it would mean merging the corpus every time you merge code.

Code does branch. So the corpus holds the union of every branch's documents,
and the checked-out branch decides which of them are real right now.

A scan therefore separates two kinds of absence, using the checked-out commit:

| Working tree | `HEAD` tree | `presence` | What happens |
| --- | --- | --- | --- |
| no | no | `not_on_branch` | hidden, and that is all — another branch has it |
| no | yes | `removed` | somebody deleted it here; `doctor` asks whether the record should go too |
| yes | — | `present` | nothing |

Hidden means hidden, never deleted. Deleting on absence would destroy a feature
branch's documentation every time somebody switched to `main`. Listing and
search omit `not_on_branch` records by default — and only those: a `removed`
document is the one case a person is asked about, and asking somebody about a
record they cannot see is not asking. `presence: "any"` or `"absent"` asks for
everything, and every record comes back carrying its `presence`, so a client can
say which of the two it is looking at.

`presence` is measured against the working tree in front of you. It rides along
in the record because a scan is where it can be written, not because it means
anything on another machine — so a fetch does not adopt somebody else's reading
of your checkout, and the scan at project open settles it. Links are unaffected: a
backlink from a record whose document is on another branch is still returned,
marked with its `source_presence`, because a link is a statement about the
project rather than about a branch.

Switching branches is visible to Memory Hub in one ref read, so no filesystem
watcher is needed for it. Opening a project scans regardless — a document can
have been edited while nothing was watching, and `HEAD` says nothing about
that.

## What "invisible" does and does not mean

Invisible: the working tree is untouched, and `refs/memory/*` does not travel
with an ordinary clone or take part in branch merges. Visible: after
`memory-hub push` those refs are on the remote and show up in `git ls-remote`.
Staying unnoticed and syncing between your own machines are mutually exclusive,
and without a push the memory lives on one machine with no backup.
