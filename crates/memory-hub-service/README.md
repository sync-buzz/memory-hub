# memory-hub-service

The Memory Hub use cases, expressed in Rust types.

Everything a client can ask Memory to do lives here: records and transactions,
diff and snapshots, search and backlinks, reconciliation, and transport.
What is deliberately absent is any notion of a
wire format — no JSON-RPC, no request ids, no tool names.

- `MemoryService` owns one project: its storage declaration and its resolved
  embedding model.
- `ServiceError` carries the stable `kind` the public interface promises
  (`conflict`, `unsupported`, …), a human message, and
  structured data. A protocol adapter maps it onto its own error shape without
  re-deciding what went wrong.
- Arguments are Rust values and results are domain types. Rendering them into a
  wire shape belongs to the adapter, which is what keeps that shape changeable
  without touching the logic.

`memory-hub-mcp` is the only adapter today, and the only public machine
interface. This crate is not published: it exists so the use cases can be tested
without spawning a process — see `tests/use_cases.rs` — and so an in-process host
remains possible without duplicating the logic.
