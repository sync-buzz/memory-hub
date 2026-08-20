# The command line

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
