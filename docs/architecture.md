# Architecture

One workspace, one machine interface, and a storage contract with three
backends behind it. Every crate below has a README of its own.

## Envelope and policy contract

`memory-hub-core` owns the versioned generic record envelope and effective
policy resolution. It has no
store, MCP, index, or client-product dependency. Compatible future fields and
unknown client profile metadata survive JSON round trips; incompatible envelope
major versions fail during decode. See
[`crates/memory-hub-core/README.md`](../crates/memory-hub-core/README.md) for the
interface guarantees.

## Storage contract and its backends

`memory-hub-engine` owns the storage contract: what a store must do to hold
records (read one, read them all, apply a transaction, report a revision) and
what it may additionally offer — history, transport, snapshots — declared as
capabilities a caller can ask about instead of discovering through
a failure.

`memory-hub-store` keeps immutable snapshots under private Git refs without
touching HEAD, code branches, the index, or worktree. Atomic transactions use
libgit2 ref compare-and-swap, rebase concurrent different-record writes, and
return structured same-record conflicts. It also owns diff and deterministic
import/export. See
[`crates/memory-hub-store/README.md`](../crates/memory-hub-store/README.md).

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
update generic record freshness, and the cursor moves commit by commit so an
interrupted catch-up resumes where it stopped. Rebase/reset divergence is
reported and requires
an explicit full rebuild; hooks are never required for correctness. See
[`crates/memory-hub-reconcile/README.md`](../crates/memory-hub-reconcile/README.md).

## Local index

`memory-hub-index` maintains a disposable LanceDB read model under the Git
directory. MCP startup, successful Memory mutations, explicit reconciliation,
and `memory_reindex` synchronize it to the store's current revision, which is
what every read serves.
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
[`crates/memory-hub-contract/README.md`](../crates/memory-hub-contract/README.md) for
the process contract and reuse instructions.

## Build and verify

The workspace pins its Rust toolchain. From the repository root:

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```
