# Git Memory

Git Memory is a standalone, Git-backed project memory engine. The executable is
named `git-memory`, so Git exposes it as `git memory` whenever it is available on
`PATH`.

This repository currently contains the bootstrap CLI only. The canonical store,
MCP interface, index, search, and encryption are intentionally outside this
initial scope.

## Build and verify

The workspace pins its Rust toolchain. From the repository root:

```sh
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

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
