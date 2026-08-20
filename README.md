# Memory Hub

Memory Hub is a project memory engine: a store, a search index and a machine
interface for the records a project keeps about itself. The types are the
project's own — it declares them and Memory Hub validates records against them
— so what a decision or a specification is here is that project's answer.

A project declares where its records live: in Git objects under private refs,
in a folder of plain JSON files, or, with encryption on, in age-encrypted Git
objects. A directory that is not a repository works as completely as one that
is. A type can put its records' content in the repository's own files instead,
where the team already reads and reviews it.

The executable is named `memory-hub` and is run directly, from `PATH` or from a
bundle that ships it.

The repository contains the bootstrap CLI, the product-neutral envelope and
policy contract, the storage contract and its backends (Git objects, a folder
of files, age-encrypted Git objects), hookless code-history reconciliation, a
recoverable local LanceDB projection, the public MCP stdio interface, and a
reusable black-box behavioral contract harness.

## Declaring where memory lives

A project says once where its records go, and the declaration is committed
alongside the code:

```sh
memory-hub init --records refs             # Git objects under refs/memory/*
memory-hub init --records folder           # one JSON file per record
memory-hub declare-storage docs --kind repo-folder --path docs
```

There is no default. The engine does not decide for the product that embeds it
whether memory should travel inside Git or sit beside it.

```jsonc
// .memory/config.json
{"config_version": 1,
 "storages": {
   "main": {"kind": "refs", "holds": ["records", "content"]},
   "docs": {"kind": "repo_folder", "path": "docs", "holds": ["content"]}}}
```

Exactly one storage holds records. Others hold content, and a type points at
one by name.

## Build and verify

The workspace pins its Rust toolchain. From the repository root:

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

## Behavioral contract harness

`memory-hub-contract` runs one shared suite through the public MCP stdio
interface. It never links to private Memory Hub implementation crates. A
consumer can run the suite against a shipped binary:

```sh
cargo run -p memory-hub-contract -- \
  --release-binary /path/to/memory-hub
```

The repository also ships a deterministic process-level fake for client and
harness development:

```sh
cargo build -p memory-hub-contract --bins
cargo run -p memory-hub-contract -- \
  --fake-binary target/debug/memory-hub-contract-fake
```

Both targets execute the same scenarios: mixed put/delete atomic batches,
immutable snapshot reads concurrent with writes, two-process writers touching
different keys, same-key conflict, and recovery/idempotent retry after a
severed stdio session. Failures are asserted from structured `kind` and `data`,
never from stderr text. See
[`crates/memory-hub-contract/README.md`](crates/memory-hub-contract/README.md) for
the process contract and reuse instructions.

## Envelope and policy contract

`memory-hub-core` owns the versioned generic record envelope, the reserved
opaque encrypted representation, and effective policy resolution. It has no
store, MCP, index, or client-product dependency. Compatible future fields and
unknown client profile metadata survive JSON round trips; incompatible envelope
major versions fail during decode. See
[`crates/memory-hub-core/README.md`](crates/memory-hub-core/README.md) for the
interface guarantees.

## Storage contract and its backends

`memory-hub-engine` owns the storage contract: what a store must do to hold
records (read one, read them all, apply a transaction, report a revision) and
what it may additionally offer — history, transport, snapshots, encryption —
declared as capabilities a caller can ask about instead of discovering through
a failure.

`memory-hub-store` keeps immutable snapshots under private Git refs without
touching HEAD, code branches, the index, or worktree. Atomic transactions use
libgit2 ref compare-and-swap, rebase concurrent different-record writes, and
return structured same-record conflicts. It also owns checkpoints, history,
diff, and deterministic import/export, and — through `EncryptedStore` — the
age-encrypted variant of all of it. See
[`crates/memory-hub-store/README.md`](crates/memory-hub-store/README.md).

`memory-hub-folder` keeps one JSON file per record under a directory. A key is
a path, so a person opening the folder sees their records laid out the way they
named them. A revision is the blake3 digest of the corpus rather than a commit
id — there is no history to point at, and the guarantee callers actually depend
on is that a revision changes when the content does. A transaction touching
several files cannot be atomic on a filesystem, so it writes its intent first
and rolls forward on the next open; a crash mid-write leaves a project that
finishes the job rather than one holding half a transaction.

Rules about records — that a type must exist, that a folder holds one record
describing it — are not a backend's business. They live in
`memory-hub-service` as a `TransactionPolicy` the backend calls at the one
moment it owns: state read, nothing written yet. No backend knows what a
`__type__` record is.

## Code-history reconciliation

`memory-hub-reconcile` stores a worktree-local cursor and catches up every code
commit on MCP initialization, CLI use, and before Memory mutations. Path diffs
update generic record freshness and each processed commit receives a
code-linked Memory checkpoint. Rebase/reset divergence is reported and requires
an explicit full rebuild; hooks are never required for correctness. See
[`crates/memory-hub-reconcile/README.md`](crates/memory-hub-reconcile/README.md).

## Local index

`memory-hub-index` maintains a disposable LanceDB read model under the Git
directory. MCP startup, successful Memory mutations, explicit reconciliation,
and `memory_reindex` synchronize it to the store's current revision — the
staged one, which is what every read serves.
Interrupted or corrupt projections rebuild exclusively from an immutable Git
snapshot; readers refuse lagging generations.

Search is hybrid: BM25 full-text search on title/content/kind is the primary
channel. When BM25 finds fewer than 5 hits and an embedding model is attached,
a vector kNN rescue channel fires. Hits below a 0.35 cosine similarity floor are
discarded and the two channels are fused via Reciprocal Rank Fusion. The result
reports `mode: "hybrid"` when the vector channel contributed, `"fts"` otherwise,
and `degraded: true` only when no embedding model is available. The embedding
runtime (model registry, download, llama.cpp backend, fingerprint) lives in
`memory-hub-embed`; a model fingerprint ties vectors to a specific model file
and runtime, so a model swap forces a clean rebuild rather than silently mixing
incompatible vectors.

Type definitions are left out of both listing and search. A `__type__` record
is schema, not knowledge, and answering "what does this project know about
authentication" with a JSON schema answers a question nobody asked, while
obliging every client to learn about a kind it has no use for. They are still
reachable — ask for `kind: "__type__"`, or set `include_service` — because the
tools that maintain schema exist. Counts keep them apart the same way: `service`
is its own number and is in none of the others, so a count of documents is a
count of documents.

The MCP server resolves the active model on first use. Resolution only checks
that the GGUF is on disk — the model is loaded when the first search needs a
vector, so a session that never searches never pays for it, and start-up stays
in milliseconds. When the projection was built without vectors (or by another
model), the first hybrid search rebuilds it once. Without a model on disk,
search stays FTS-only and reports `degraded: true`.

## MCP interface

Start the only public machine interface with an explicit repository:

```sh
memory-hub mcp --project /absolute/path/to/repository
```

The server speaks MCP `2025-11-25` over stdio and Memory interface major `1`. Initialization publishes the
Memory interface, store, envelope, and index versions together with capability
availability, installation/project identifiers, encryption mode, and the
resolved Git directory. Clients may require a Memory interface major through
`_meta.memoryHub.memoryInterfaceVersion`; an incompatible major is rejected
before Memory Hub creates or moves a ref.

See [`crates/memory-hub-mcp/README.md`](crates/memory-hub-mcp/README.md) for the
resource and tool schemas, version handshake, errors, and revision subscription
contract.

The interface is an adapter, not the logic. Every use case lives in
`memory-hub-service` as typed Rust — arguments are values, results are domain
types, failures carry the same stable `kind` the wire promises — and
`memory-hub-mcp` parses JSON-RPC into it and renders the results back out. That
is what lets the use cases be tested without spawning a process
([`crates/memory-hub-service/README.md`](crates/memory-hub-service/README.md));
MCP remains the only public machine interface.

## Encryption

Memory Hub supports optional encrypted mode using [age](https://age-encryption.org).
When enabled, all record content and metadata are encrypted before reaching
the Git tree. The `memory-hub-crypto` crate is a thin wrapper around the
`age` crate (with SSH key support), and `memory-hub-store` provides an
`EncryptedStore` that transparently encrypts and decrypts records.

### How it works

Encryption uses **age** with **SSH keys** as recipient identities. Most
developers already have an SSH key on GitHub — Memory Hub reuses it:

- **Public key** (on GitHub) is used as an age recipient for encryption
- **Private key** (`~/.ssh/id_ed25519`) is used as an age identity for decryption
- Every record and the manifest are encrypted to all recipients in the list
- Only people whose keys are in the recipients list can decrypt

For users without SSH keys, Memory Hub generates an age-native X25519
keypair as a fallback. A backup X25519 keypair is always generated for
recovery.

### Access model

Two independent gates, both required for access:

- **Git access** — can clone/fetch the encrypted data from the repository
- **Crypto access** — SSH/X25519 key is in the recipients list, can decrypt

A collaborator with Git access but no crypto key sees encrypted blobs but
cannot read them. The project owner controls the recipients list through
`memory-hub encryption add/remove`.

### What is encrypted

In encrypted mode the Git tree contains no plaintext: record payloads,
semantic keys, titles, kinds, tags, and links are all inside the encrypted
manifest or encrypted record blobs. Only unavoidable Git metadata (refs,
object counts, timestamps, commit graph) remains visible.

### Ephemeral index

For encrypted projects, the LanceDB index contains plaintext derived from
decrypted records. To avoid persisting plaintext on disk, the index is
**ephemeral**: it is rebuilt from decrypted records on `memory_unlock` and
destroyed on `memory_lock`. If the MCP process crashes before `memory_lock`
runs, the next session start wipes the stale index directory before serving any
request — no plaintext survives a crash/restart cycle.

### Key operations (via MCP)

Encryption is managed through MCP tools, not CLI subcommands:

```
memory_init_encrypted   → initialize encrypted store with first recipient
memory_unlock           → decrypt with an identity, rebuild ephemeral index
memory_lock             → drop identity, destroy ephemeral index
memory_add_recipient    → add team member, re-encrypt all records
memory_remove_recipient → remove member, re-encrypt, rebuild index
memory_list_recipients  → show recipients in the manifest
memory_encryption_status → check current lock state
```

`memory_unlock` accepts either identity format: an OpenSSH private key
(`~/.ssh/id_ed25519`) for everyday use, or the age-native backup key
(`AGE-SECRET-KEY-1…`) returned by `memory_init_encrypted` — the recovery path
when the SSH key is lost. The format is detected from the file's content.

Every operation on an encrypted project goes through the encrypted store:
`memory_import` encrypts the bundle it is given, `memory_export` requires an
unlocked store and deliberately produces plaintext, and reads, searches and
backlinks return a `locked` error until `memory_unlock` runs.


## Bootstrap commands

```sh
memory-hub --version
memory-hub --help
memory-hub init --records refs --project /path/to/project
memory-hub init --records folder --project /path/to/project
memory-hub declare-storage docs --kind repo-folder --path docs --project /path/to/project
memory-hub doctor --project /path/to/repository
memory-hub doctor --project /path/to/repository --output json
memory-hub reconcile --project /path/to/repository --output json
memory-hub reconcile --project /path/to/repository --full-rebuild
memory-hub reconcile --project /path/to/repository --embed
```

`init` is the first command a project needs; everything that reads or writes
records answers `not_initialised` until it has run. `doctor` accepts an empty
Git repository; a commit is not required. JSON output
is versioned with `schema_version` and reports failures using stable `kind`
values. `doctor` is read-only: it reports how far Memory trails code history
but never creates checkpoints, advances the cursor, or marks records stale —
run `memory-hub reconcile` for that. `reconcile --embed` rebuilds the index with embedding vectors when a
model is downloaded; without `--embed` the index is FTS-only.

## Model management

```sh
memory-hub model list                     # show registry, on-disk status, active model
memory-hub model show bge-m3              # metadata, dimensions, backend
memory-hub model download bge-m3          # download GGUF with SHA-256 verification
memory-hub model use bge-m3               # set active model in config
memory-hub model benchmark bge-m3         # measure throughput, warn if below floor
```

Platform-aware defaults: Apple Silicon uses Metal + BGE-M3; Intel/Linux/Windows
uses CPU + nomic-embed-text-v1.5. `memory-hub doctor` reports missing or broken
models and suggests `model download`.

## Remote exchange

Memory has its own remote, separate from the code `origin`. Ordinary
`git clone`/`git push` never publish `refs/memory/*`; `memory-hub push` is an
explicit action that applies the effective push policy first.

```sh
memory-hub remote add <url>               # configure memory remote
memory-hub remote list                    # show the configured memory remote
memory-hub remote remove                  # forget it
memory-hub fetch --project /path/to/repo  # pull and merge memory refs
memory-hub push --project /path/to/repo   # publish memory refs (use --force to overwrite)
```

Merge is record-level: different keys merge automatically; the same key changed
by both sides returns both versions as a conflict. Encrypted merge decrypts,
merges, and re-encrypts in one step (requires `memory_unlock` first). Records
deleted on the remote are kept locally — deletions do not replicate through
merge; publish an explicit delete instead.


### Memory does not arrive with a clone

`git clone` copies branches and tags. It does not copy `refs/memory/*`, and no
option makes it — so a colleague who clones a project with years of memory in
it opens an empty one. Nothing is lost and nothing is broken; the memory is
simply still on the remote.

`memory-hub doctor` is what tells the two apart, because from inside the clone
they look identical:

```sh
memory-hub doctor                         # in the fresh clone
# [error] memory.presence: memory exists on the code remote <url> but not in
# this repository: `git clone` does not copy refs/memory/* — run
# `memory-hub remote add <url>`, then `memory-hub fetch`
```

The check asks the code `origin` when no memory remote is configured yet, so it
works in a clone nobody has set up. It only asks when the local memory is
empty — a repository that has its memory pays no network call. When the remote
carries no memory either, the check passes and says so: an empty project is a
normal state, not a failure.

The `kind` field is stable for scripts and for consumers rendering the report:
`memory_not_fetched` (it is on the remote), `no_memory_anywhere` (there is none
yet), `remote_unreachable` (the question could not be asked).

One thing the fetch will insist on: verification is fail-closed, so
`memory-hub fetch` refuses history it cannot verify until you configure an
allowed signer or opt out explicitly — see
[Signing and verification](#signing-and-verification).
### Encryption is the storage's, not the type's

A type cannot ask to be encrypted, and a `storage` section that tries is
refused by name. `refs` encrypts the whole store at once or not at all, and a
folder of ordinary repository files cannot be encrypted and stay ordinary —
which is the entire reason it is a folder of ordinary repository files.

Nothing extra is needed for records whose content is outside. Their links,
tags, title and freshness live in the envelope, and the envelope is in `refs`:
encrypt `refs` and they are encrypted with it. The file holds the content and
nothing else, which is what makes attaching one invisible in the first place.

A locked project answers a search with `locked`, never with an empty result.
The two look the same and mean opposite things — one says look elsewhere, the
other says unlock and look again.

Turning encryption on protects future writes only. The plaintext blobs already
in Git history stay readable to anyone with the repository; `memory-hub doctor`
says so on every plaintext project rather than leaving it to be discovered.

### Signing and verification

GitHub rulesets do not protect `refs/memory/*`, so any collaborator with push
access can rewrite Memory refs. Signatures are the only real protection, and
Memory Hub treats the two directions differently:

- **Signing is opt-in.** Point `memory-hub.signing.key` at a private key, or
  let Memory Hub reuse Git's own SSH signing key (`gpg.format = ssh` plus a
  `user.signingkey` that names a file). Every Memory commit is then SSH-signed.
- **Verification is fail-closed.** `memory-hub fetch` refuses to import history
  it cannot verify. Configure the keys you trust, or opt out explicitly:

```sh
git config memory-hub.signing.key ~/.ssh/id_ed25519
git config --add memory-hub.signing.allowedSigner "ssh-ed25519 AAAA... alice"
git config memory-hub.signing.allowedSignersFile .memory-hub/allowed_signers
git config memory-hub.signing.verify off     # accept unsigned memory refs
```

For encrypted projects the manifest's SSH recipients are trusted automatically,
so a team that already shares keys needs no extra configuration.

A configured refspec may only move `refs/memory/*`, and a remote URL that Git
would read as an option (`--upload-pack=…`) or as a remote helper (`ext::…`) is
rejected — code refs stay untouched and a repository's config cannot turn a
fetch into command execution.

## Attached repository folders

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
export, checkpoints — works from what Memory itself holds, so an unreachable
folder can never make one of them quietly return less.

A record whose file is gone stays `missing` indefinitely and is removed only by
an explicit deletion. Deleted, on another branch, and not pulled are
indistinguishable at the moment of looking, and two of the three are routine —
switching branches would otherwise destroy a feature branch's documentation
records every time.

### Folders

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

#### A folder with a title and a text of its own

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

#### Listing folders

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

#### Renaming a folder

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

### Branches

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

### What "invisible" does and does not mean

Invisible: the working tree is untouched, and `refs/memory/*` does not travel
with an ordinary clone or take part in branch merges. Visible: after
`memory-hub push` those refs are on the remote and show up in `git ls-remote`.
Staying unnoticed and syncing between your own machines are mutually exclusive,
and without a push the memory lives on one machine with no backup.

## Moving a type to another storage

Where a type's records live is a field of its definition, and definitions get
edited. Letting the data follow an edited field would be data loss wearing the
clothes of a setting, so editing `storage` is **refused** while it would leave
records behind, and moving them is an operation of its own:

```json
{ "name": "memory_migrate_storage",
  "arguments": { "kind": "doc", "dry_run": true, "storage": "docs" } }
```

`"storage": null` brings the content back into the records. An absent field is
not the same thing and is refused: `null` says "here", absent says the caller
forgot to say.

`dry_run` returns the plan — which records move, in which direction, and what
has to be accepted — and writes nothing. Every warning code the plan lists must
be echoed in `acknowledge` before the migration runs. A boolean nobody reads is
not consent, and the two directions are not asking the same thing:

- **into the working tree** — `content_becomes_visible`: the content of every
  record is written where the whole team sees it in diffs and reviews. That is
  a change of visibility, not a technical detail.
- **back into refs** — `does_not_hide_published_history`: the plaintext blobs
  stay in Git history for good. This changes where new writes go; it is never
  retroactive privacy. And `files_are_left_in_place`: Memory does not delete
  files it did not put there on this run.

Content is written before the records that point at it, and the records move
before the definition that describes them. Interrupted anywhere, running it
again resumes: records that already live where the new storage says are not
records left behind.

## Knowing what changed

`memory://revision/current` says that something changed. Subscribing to
`memory://records/changed` says **what**:

```json
{ "jsonrpc": "2.0",
  "method": "notifications/memoryHub/recordsChanged",
  "params": { "uri": "memory://records/changed",
    "records": [ { "key": "auth", "locator": "docs/guides/api/auth.md",
                   "change": "content_changed" } ] } }
```

An editor holding one record open can re-read that record instead of throwing
away everything it knows. The changes are `written`, `deleted`,
`content_changed`, `moved`, `content_absent`, `content_returned`,
`freshness_changed` and `needs_attention`.

It also closes a gap a revision cannot: a scan that finds only a file it cannot
match writes nothing, so the revision does not move — and the client still
needs to hear about it.

The two subscriptions are independent. A client that takes only
`memory://revision/current` hears that something changed and nothing more; one
that takes both hears which records it was.

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | Command completed successfully |
| 2 | Invalid command-line usage |
| 10 | One or more doctor checks failed |
| 70 | Memory Hub could not render or return its result |

## Supported platforms

The bootstrap is tested in CI on Linux, macOS, and Windows. Release archives are
published for macOS (arm64, x86_64), Linux (x86_64) as `.tar.gz`, and Windows
(x86_64) as `.zip`.

The macOS binaries are signed with a Developer ID certificate and a hardened
runtime when the signing secrets are configured for the repository
(`MACOS_CERTIFICATE`, `MACOS_CERTIFICATE_PASSWORD`, `MACOS_SIGNING_IDENTITY`).
They are deliberately not notarized here: a bare executable cannot be stapled,
so notarization belongs to whatever application bundles it.

## License

Memory Hub is licensed under FSL-1.1-MIT. See [LICENSE](LICENSE).
