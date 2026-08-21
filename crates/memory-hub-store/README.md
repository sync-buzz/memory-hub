# memory-hub-store

In-process Git object store behind the `GitStore` interface.

- `refs/memory/main` points to an append-only transaction commit chain; each
  commit owns the current immutable record tree and exact changed-record ids,
  and every past state is one of its parents.
- record filenames are SHA-256 digests, never semantic keys; transaction
  metadata lives in commit messages rather than growing the record tree;
- a put/delete batch builds new objects and then moves the ref with a CAS;
- concurrent different-record batches rebase, while same-record changes return
  a structured conflict;
- exports contain only sorted canonical records, so export/import/export is
  byte-for-byte stable.

## Which revision a read serves

`current()` — and therefore every read, search and export that does not name a
revision — resolves `refs/memory/main`. A record is readable the moment its
transaction lands. `expected_revision` is compared against that same revision,
so optimistic concurrency is a comparison between two values the caller has
actually seen.

A past revision is reached by naming it: `diff` compares two, and a snapshot
reopens one. Neither is the silent default of a read.

## Schema strictness

`GitStore` is strict by default: a record whose `kind` has no matching
`__type__` definition is rejected at write time. `with_schema_strict(false)`
accepts unknown kinds without validation, and exists for consumers that do not
publish a type corpus. Strictness is what makes `memory_schema_status` a gate
rather than a report.

The implementation uses `git2` for the object database, tree, commit, and ref
operations. It never invokes a shell command for a transaction.
