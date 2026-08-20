# Embedding Memory Hub in a Rust program

Memory Hub is two things in one workspace: a server an agent talks to over MCP,
and a library a Rust program links. They are the same engine — `memory-hub-mcp`
is a JSON-RPC adapter over `memory-hub-service`, and there is no behaviour in
the adapter that the library cannot reach.

## Which one to use

**Spawn the binary** when the host is not Rust, when the same project is open in
more than one program at a time, or when a crash inside the index or the
embedding runtime must not take the host process with it.

**Link the library** when the host is Rust and wants typed results and typed
errors instead of parsed JSON, or when it already supervises its own processes
and does not want to supervise one more.

Both can be true of one product: a desktop application can link the library for
its own window and still hand agents the binary.

## Depending on it

The crates are not published to crates.io. Depend on the repository and pin a
tag, so a build is reproducible and an engine update is a deliberate change:

```toml
[dependencies]
memory-hub-service = { git = "https://github.com/sync-buzz/memory-hub", tag = "v0.1.0" }
memory-hub-index   = { git = "https://github.com/sync-buzz/memory-hub", tag = "v0.1.0" }
memory-hub-store   = { git = "https://github.com/sync-buzz/memory-hub", tag = "v0.1.0" }
```

## Opening a project

`open` performs no I/O. The store, the schema registry, the index and the
embedding model are resolved on first use, so a host that only ever reads one
record never pays for an embedding runtime.

```rust
use memory_hub_service::MemoryService;

let service = MemoryService::open(project_dir);
let revision = service.current_revision()?;
```

## Reading

```rust
use memory_hub_index::{SearchFilters, SearchRequest};

let found = service.search(&SearchRequest {
    query: "how is authentication configured".to_owned(),
    limit: 10,
    offset: 0,
    filters: SearchFilters::default(),
    revision,
})?;

let record = service.get_record("decisions/storage", None)?;
```

A read takes the revision it is answering at, so a listing, a search and a
record fetched beside them describe one state of the project rather than three
moments of it.

## Writing

```rust
use memory_hub_store::Operation;

let result = service.apply_transaction("import-42", expected_revision, operations)?;
```

A transaction states the revision it was written against. Concurrent writes to
different records are rebased and applied; two writes to the same record come
back as a structured conflict rather than a silent overwrite, and the caller
decides what to do with both versions. Retrying is safe: the same transaction
id applied twice is applied once.

## What the host owns

- **The runtime.** The service is synchronous and does not require an async
  context. A host that has one must not call the service on a thread it needs
  to keep free — index work and embedding are the expensive parts.
- **The failures.** `ServiceError` carries the same stable `kind` the wire
  promises (`not_initialised`, `locked`, `conflict`, `unsupported`, …), so a
  host that switches between linking and spawning handles one set of names.
- **The interface version.** Linking pins it at compile time: the crate a host
  builds against is the interface it gets, and there is no handshake to fail.

Every use case the service exposes is listed in
[`crates/memory-hub-service/README.md`](../crates/memory-hub-service/README.md),
and the tests in that crate call them directly — they are the worked examples.
