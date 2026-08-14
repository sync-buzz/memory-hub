# Git Memory

Git Memory is a standalone, Git-backed project memory engine. The executable is
named `git-memory`, so Git exposes it as `git memory` whenever it is available on
`PATH`.

The repository currently contains the bootstrap CLI and a reusable black-box
behavioral contract harness. The canonical store, production MCP interface,
index, search, and encryption are intentionally outside the current scope.

## Build and verify

The workspace pins its Rust toolchain. From the repository root:

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
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

Both targets execute the same scenarios: atomic batch rejection, immutable
snapshot reads, stale writers touching different keys, same-key conflict, and
recovery/idempotent retry after a severed stdio session. Failures are asserted
from structured `kind` and `data`, never from stderr text. See
[`crates/git-memory-contract/README.md`](crates/git-memory-contract/README.md) for
the process contract and reuse instructions.

## Bootstrap commands

```sh
git memory --version
git memory --help
git memory doctor --project /path/to/repository
git memory doctor --project /path/to/repository --output json
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
