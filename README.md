# Git Memory

Git Memory is a standalone, Git-backed project memory engine. The executable is
named `git-memory`, so Git exposes it as `git memory` whenever it is available on
`PATH`.

The repository contains the bootstrap CLI, the product-neutral envelope and
policy contract, the atomic Git object store, hookless code-history
reconciliation, a recoverable local LanceDB projection, the public MCP stdio
interface, optional age-based encryption, and a reusable black-box behavioral
contract harness.

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

`git-memory-contract` runs one shared suite through the public MCP stdio
interface. It never links to private Git Memory implementation crates. A
consumer can run the suite against a shipped binary:

```sh
cargo run -p git-memory-contract -- \
  --release-binary /path/to/git-memory
```

The repository also ships a deterministic process-level fake for client and
harness development:

```sh
cargo build -p git-memory-contract --bins
cargo run -p git-memory-contract -- \
  --fake-binary target/debug/git-memory-contract-fake
```

Both targets execute the same scenarios: mixed put/delete atomic batches,
immutable snapshot reads concurrent with writes, two-process writers touching
different keys, same-key conflict, and recovery/idempotent retry after a
severed stdio session. Failures are asserted from structured `kind` and `data`,
never from stderr text. See
[`crates/git-memory-contract/README.md`](crates/git-memory-contract/README.md) for
the process contract and reuse instructions.

## Envelope and policy contract

`git-memory-core` owns the versioned generic record envelope, the reserved
opaque encrypted representation, and effective policy resolution. It has no
store, MCP, index, or client-product dependency. Compatible future fields and
unknown client profile metadata survive JSON round trips; incompatible envelope
major versions fail during decode. See
[`crates/git-memory-core/README.md`](crates/git-memory-core/README.md) for the
interface guarantees.

## Git object store

`git-memory-store` keeps immutable snapshots under private Git refs without
touching HEAD, code branches, the index, or worktree. Atomic transactions use
libgit2 ref compare-and-swap, rebase concurrent different-record writes, and
return structured same-record conflicts. It also owns checkpoints, history,
diff, and deterministic import/export. See
[`crates/git-memory-store/README.md`](crates/git-memory-store/README.md).

## Code-history reconciliation

`git-memory-reconcile` stores a worktree-local cursor and catches up every code
commit on MCP initialization, CLI use, and before Memory mutations. Path diffs
update generic record freshness and each processed commit receives a
code-linked Memory checkpoint. Rebase/reset divergence is reported and requires
an explicit full rebuild; hooks are never required for correctness. See
[`crates/git-memory-reconcile/README.md`](crates/git-memory-reconcile/README.md).

## Local index

`git-memory-index` maintains a disposable LanceDB read model under the Git
directory. MCP startup, successful Memory mutations, explicit reconciliation,
and `memory_reindex` synchronize it to the canonical Git Memory revision.
Interrupted or corrupt projections rebuild exclusively from an immutable Git
snapshot; readers refuse lagging generations.

Search is hybrid: BM25 full-text search on title/content/kind is the primary
channel. When BM25 finds fewer than 5 hits and an embedding model is attached,
a vector kNN rescue channel fires. Hits below a 0.35 cosine similarity floor are
discarded and the two channels are fused via Reciprocal Rank Fusion. The result
reports `mode: "hybrid"` when the vector channel contributed, `"fts"` otherwise,
and `degraded: true` only when no embedding model is available. The embedding
runtime (model registry, download, llama.cpp backend, fingerprint) lives in
`git-memory-embed`; a model fingerprint ties vectors to a specific model file
and runtime, so a model swap forces a clean rebuild rather than silently mixing
incompatible vectors.

## MCP interface

Start the only public machine interface with an explicit repository:

```sh
git memory mcp --project /absolute/path/to/repository
```

The server speaks MCP `2025-11-25` over stdio. Initialization publishes the
Memory interface, store, envelope, and index versions together with capability
availability, installation/project identifiers, encryption mode, and the
resolved Git directory. Clients may require a Memory interface major through
`_meta.gitMemory.memoryInterfaceVersion`; an incompatible major is rejected
before Git Memory creates or moves a ref.

See [`crates/git-memory-mcp/README.md`](crates/git-memory-mcp/README.md) for the
resource and tool schemas, version handshake, errors, and revision subscription
contract.

## Encryption

Git Memory supports optional encrypted mode using [age](https://age-encryption.org).
When enabled, all record content and metadata are encrypted before reaching
the Git tree. The `git-memory-crypto` crate is a thin wrapper around the
`age` crate (with SSH key support), and `git-memory-store` provides an
`EncryptedStore` that transparently encrypts and decrypts records.

### How it works

Encryption uses **age** with **SSH keys** as recipient identities. Most
developers already have an SSH key on GitHub — Git Memory reuses it:

- **Public key** (on GitHub) is used as an age recipient for encryption
- **Private key** (`~/.ssh/id_ed25519`) is used as an age identity for decryption
- Every record and the manifest are encrypted to all recipients in the list
- Only people whose keys are in the recipients list can decrypt

For users without SSH keys, Git Memory generates an age-native X25519
keypair as a fallback. A backup X25519 keypair is always generated for
recovery.

### Access model

Two independent gates, both required for access:

- **Git access** — can clone/fetch the encrypted data from the repository
- **Crypto access** — SSH/X25519 key is in the recipients list, can decrypt

A collaborator with Git access but no crypto key sees encrypted blobs but
cannot read them. The project owner controls the recipients list through
`git memory encryption add/remove`.

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
memory_unlock           → decrypt with SSH identity, rebuild ephemeral index
memory_lock             → drop identity, destroy ephemeral index
memory_add_recipient    → add team member, re-encrypt all records
memory_remove_recipient → remove member, re-encrypt, rebuild index
memory_list_recipients  → show recipients in the manifest
memory_encryption_status → check current lock state
```

See the [encryption architecture document](.sync/docs/encryption-architecture.md)
for the full threat model, manifest structure, and recovery workflows.

## Bootstrap commands

```sh
git memory --version
git memory --help
git memory doctor --project /path/to/repository
git memory doctor --project /path/to/repository --output json
git memory reconcile --project /path/to/repository --output json
git memory reconcile --project /path/to/repository --full-rebuild
git memory reconcile --project /path/to/repository --embed
```

`doctor` accepts an empty Git repository; a commit is not required. JSON output
is versioned with `schema_version` and reports failures using stable `kind`
values. `reconcile --embed` rebuilds the index with embedding vectors when a
model is downloaded; without `--embed` the index is FTS-only.

## Model management

```sh
git memory model list                     # show registry, on-disk status, active model
git memory model show bge-m3              # metadata, dimensions, backend
git memory model download bge-m3          # download GGUF with SHA-256 verification
git memory model use bge-m3               # set active model in config
git memory model benchmark bge-m3         # measure throughput, warn if below floor
```

Platform-aware defaults: Apple Silicon uses Metal + BGE-M3; Intel/Linux/Windows
uses CPU + nomic-embed-text-v1.5. `git memory doctor` reports missing or broken
models and suggests `model download`.

## Remote exchange

Memory has its own remote, separate from the code `origin`. Ordinary
`git clone`/`git push` never publish `refs/memory/*`; `git memory push` is an
explicit action that applies the effective push policy first.

```sh
git memory remote add origin <url>        # configure memory remote
git memory remote list                    # show configured remotes
git memory fetch --project /path/to/repo  # pull and merge memory refs
git memory push --project /path/to/repo   # publish memory refs (use --force to overwrite)
git memory remote status --project /path  # check sync state
```

Merge is record-level: different keys merge automatically; the same key changed
by both sides returns both versions as a conflict. Encrypted merge decrypts,
merges, and re-encrypts in one step (requires `memory_unlock` first).

## Exit codes

| Code | Meaning |
| ---: | --- |
| 0 | Command completed successfully |
| 2 | Invalid command-line usage |
| 10 | One or more doctor checks failed |
| 70 | Git Memory could not render or return its result |

## Supported platforms

The bootstrap is tested in CI on Linux, macOS, and Windows.

## License

Git Memory is licensed under FSL-1.1-MIT. See [LICENSE](LICENSE).
