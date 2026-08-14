# git-memory-store

In-process Git object store behind the `GitStore` interface.

- `refs/memory/staged` points to an append-only transaction commit chain; each
  commit owns the current immutable record tree and exact changed-record ids.
- `refs/memory/main` points to a commit chain of explicit checkpoints; code
  reconciliation checkpoints also carry the full processed code revision.
- record filenames are SHA-256/opaque identifiers, never semantic encrypted
  keys; transaction metadata lives in commit messages rather than growing the
  record tree;
- a put/delete batch builds new objects and then moves `staged` with ref CAS;
- concurrent different-record batches rebase, while same-record changes return
  a structured conflict;
- exports contain only sorted canonical records, so export/import/export is
  byte-for-byte stable.

The implementation uses `git2` for the object database, tree, commit, and ref
operations. It never invokes a shell command for a transaction.
