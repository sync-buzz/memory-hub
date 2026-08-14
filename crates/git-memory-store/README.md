# git-memory-store

In-process Git object store behind the `GitStore` interface.

- `refs/memory/staged` points directly to the current immutable tree.
- `refs/memory/main` points to a commit chain of explicit checkpoints.
- record and transaction filenames are SHA-256/opaque identifiers, never
  semantic encrypted keys;
- a put/delete batch builds new objects and then moves `staged` with ref CAS;
- concurrent different-record batches rebase, while same-record changes return
  a structured conflict;
- exports contain only sorted canonical records, so export/import/export is
  byte-for-byte stable.

The implementation uses `git2` for the object database, tree, commit, and ref
operations. It never invokes a shell command for a transaction.
