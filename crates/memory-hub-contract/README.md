# Memory Hub behavioral contract

This crate is the reusable black-box acceptance suite for Memory Hub servers.
It is intentionally a separate workspace package and has no dependency on the
`memory-hub` package. Consumers call `run_contract` or execute the
`memory-hub-contract` runner; they do not copy scenario sources.

## Target boundary

`ReleaseBinaryTarget` launches:

```text
<binary> mcp --project <temporary-git-repository>
```

`FakeServerTarget` launches the deterministic fake directly. Both are fresh
stdio processes and receive newline-delimited JSON-RPC 2.0. The client performs
MCP `2025-11-25` initialization and validates the negotiated protocol and
required capabilities before each operation. No public in-process Rust server
adapter exists.

The fake is a harness executable, not a second Memory Hub runtime. Its state is
deterministic, project-scoped, and atomically replaced on disk so that a new
process can observe an interrupted session's outcome. For the recovery scenario
it emits a standard MCP progress notification at a deterministic pre-commit
failpoint; the harness terminates it only after that acknowledgement. A release
binary is terminated at the public process boundary, where either the complete
old or complete new state is valid, but a partial batch is never valid.

## Exercised public surface

- resource `memory://revision/current`;
- tool `memory_apply_transaction` with `transaction_id`,
  `expected_revision`, and a bulk `operations` array;
- tool `memory_get_record` with an explicit immutable `revision`;
- MCP tool errors using
  `isError: true` and `structuredContent.error.{kind,data}`.

Successful transaction results contain `revision` and `changed_keys`. A stale
transaction touching keys unchanged since its expected revision is reapplied to
the current snapshot. A stale transaction touching a changed key returns
`kind: conflict` with expected/current revisions, conflicting keys, and a
recovery action. Retrying the same `transaction_id` converges on its original
revision.

The atomic scenario applies a valid mixed put/delete batch. Snapshot consistency
is read from one process while another process writes, and both race scenarios
start two independent MCP processes behind the same barrier.

The fixture records use only generic `note` records and neutral opaque client
metadata. They contain no product-specific entity kinds or metadata.

## Reuse

From Rust, depend on this package and supply a process target:

```rust,no_run
use memory_hub_contract::{ReleaseBinaryTarget, run_contract};

let report = run_contract(&ReleaseBinaryTarget::new("/opt/bin/memory-hub"));
assert!(report.passed, "{report:#?}");
```

For language-independent CI, run the binary with `--output json`. The report has
a versioned shape and one result per shared scenario.
