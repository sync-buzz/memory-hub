# memory-hub-store

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

## Which revision a read serves

`current()` — and therefore every read, search and export that does not name a
revision — resolves `refs/memory/staged`. A record is readable the moment its
transaction lands, without a checkpoint. `expected_revision` is compared against
that same staged revision, so optimistic concurrency is a comparison between two
values the caller has actually seen.

`refs/memory/main` is what checkpoints name and what history and diff walk. It
is reached explicitly, never as the silent default of a read.

## Schema strictness

`GitStore` is strict by default: a record whose `kind` has no matching
`__type__` definition is rejected at write time. `with_schema_strict(false)`
accepts unknown kinds without validation, and exists for consumers that do not
publish a type corpus. Strictness is what makes `memory_schema_status` a gate
rather than a report.

The implementation uses `git2` for the object database, tree, commit, and ref
operations. It never invokes a shell command for a transaction.
