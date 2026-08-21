# The MCP interface

Memory Hub's machine interface is MCP over stdio, and it is the only one.
Tool and resource schemas are in
[`crates/memory-hub-mcp/README.md`](../crates/memory-hub-mcp/README.md).

Start the only public machine interface with an explicit repository:

```sh
memory-hub mcp --project /absolute/path/to/repository
```

The server speaks MCP `2025-11-25` over stdio and Memory interface major `1`. Initialization publishes the
Memory interface, store, envelope, and index versions together with capability
availability, installation/project identifiers, and the resolved Git
directory. Clients may require a Memory interface major through
`_meta.memoryHub.memoryInterfaceVersion`; an incompatible major is rejected
before Memory Hub creates or moves a ref.

See [`crates/memory-hub-mcp/README.md`](../crates/memory-hub-mcp/README.md) for the
resource and tool schemas, version handshake, errors, and revision subscription
contract.

The interface is an adapter, not the logic. Every use case lives in
`memory-hub-service` as typed Rust — arguments are values, results are domain
types, failures carry the same stable `kind` the wire promises — and
`memory-hub-mcp` parses JSON-RPC into it and renders the results back out. That
is what lets the use cases be tested without spawning a process
([`crates/memory-hub-service/README.md`](../crates/memory-hub-service/README.md));
MCP remains the only public machine interface.

## Knowing what changed

`memory://revision/current` says that something changed. Subscribing to
`memory://records/changed` says **what**:

```json
{ "jsonrpc": "2.0",
  "method": "notifications/memoryHub/recordsChanged",
  "params": { "uri": "memory://records/changed",
    "records": [ { "key": "auth", "locator": "docs/guides/api/auth.md",
                   "change": "content_changed" } ] } }
```

An editor holding one record open can re-read that record instead of throwing
away everything it knows. The changes are `written`, `deleted`,
`content_changed`, `moved`, `content_absent`, `content_returned`,
`freshness_changed` and `needs_attention`.

It also closes a gap a revision cannot: a scan that finds only a file it cannot
match writes nothing, so the revision does not move — and the client still
needs to hear about it.

The two subscriptions are independent. A client that takes only
`memory://revision/current` hears that something changed and nothing more; one
that takes both hears which records it was.
