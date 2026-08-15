# Git Memory

Git Memory is a standalone, Git-backed project memory engine. The executable is
named `git-memory`, so Git exposes it as `git memory` whenever it is available on
`PATH`.

The repository contains the bootstrap CLI, the product-neutral envelope and
policy contract, the atomic Git object store, hookless code-history
reconciliation, a recoverable local LanceDB projection, the public MCP stdio
interface, and a reusable black-box behavioral contract harness. Search,
remote exchange, and encryption implementations remain capability-gated for
later releases.

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

## Bootstrap commands

```sh
git memory --version
git memory --help
git memory doctor --project /path/to/repository
git memory doctor --project /path/to/repository --output json
git memory reconcile --project /path/to/repository --output json
git memory reconcile --project /path/to/repository --full-rebuild
```

`doctor` accepts an empty Git repository; a commit is not required. JSON output
is versioned with `schema_version` and reports failures using stable `kind`
values.

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
