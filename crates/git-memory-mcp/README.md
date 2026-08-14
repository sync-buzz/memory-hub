# git-memory-mcp

`git-memory-mcp` is Git Memory's sole public machine interface. It implements
MCP JSON-RPC over stdio; there is no second custom RPC protocol. A caller starts
it with `git memory mcp --project /absolute/repository/path`.

## Initialization and compatibility

The server negotiates MCP `2025-11-25`. The initialize result repeats the
Git Memory handshake in `capabilities.experimental.gitMemory` and
`_meta.gitMemory`:

```json
{
  "memoryInterfaceVersion": {"major": 1, "minor": 1},
  "storeVersion": {"major": 1, "minor": 1},
  "envelopeVersion": {"major": 1, "minor": 0},
  "indexVersion": {"major": 0, "minor": 0},
  "modelFingerprint": null,
  "encryptionMode": "plaintext",
  "installationId": "installation-<sha256>",
  "projectId": "project-<sha256>",
  "projectPath": "/absolute/repository/path",
  "gitDir": "/absolute/repository/path/.git/",
  "reconciliation": {"status": "ok", "report": {}}
}
```

A client can send its required version at
`params._meta.gitMemory.memoryInterfaceVersion`. A different major returns
`incompatible_memory_interface` before the store is opened, so no Memory ref is
created or moved. Newer minor versions remain compatible.

## Resources

All resource bodies are UTF-8 JSON with `mimeType: application/json`.

| URI | Body schema |
| --- | --- |
| `memory://project` | handshake fields above plus `gitDir` |
| `memory://revision/current` | `{schemaVersion: 1, revision: string}` |
| `memory://records/{key}` | `{schemaVersion: 1, revision: string, record: StoredRecord|null}` |
| `memory://index/status` | `{schemaVersion: 1, available: bool, capability, plannedSpec}` |
| `memory://model/status` | `{schemaVersion: 1, available: bool, capability, plannedSpec}` |
| `memory://policy/effective` | `{schemaVersion: 1, policies: EffectivePolicy[]}` |
| `memory://encryption/status` | `{schemaVersion: 1, mode, available, encryptedStoreAvailable, encryptedIndexAvailable}` |

Subscribe with MCP `resources/subscribe` to
`memory://revision/current`. After a successful transaction/import the server
sends `notifications/resources/updated`. The notification carries only the URI:
the client must reread the resource and treat its revision as authoritative.

## Tools

`tools/list` is the canonical JSON Schema catalogue. The implemented v1 store
surface is:

| Tool | Required input | Result |
| --- | --- | --- |
| `memory_apply_transaction` | `transaction_id`, `expected_revision`, non-empty `operations[]` | `{revision, changed_keys}` |
| `memory_get_record` | `key`, `revision` | `{revision, record}` |
| `memory_list_records` | optional `revision` | `{revision, records}` |
| `memory_checkpoint` | `message` | `Checkpoint` |
| `memory_history` | optional `limit` | `{checkpoints}` |
| `memory_diff` | `from_revision`, `to_revision` | `{fromRevision, toRevision, changes}` |
| `memory_export` | `revision` | `{revision, bundle}` |
| `memory_import` | `transaction_id`, `expected_revision`, `bundle` | `{revision, changed_keys}` |
| `memory_reconcile` | optional `divergence: report\|full_rebuild` | `ReconcileReport` |
| `memory_doctor` | none | repository/store health |
| `memory_encryption_status` | none | current plaintext/encryption availability |

A transaction operation is either `{"op":"put","record":StoredRecord}` or
`{"op":"delete","key":"..."}` (opaque callers may supply `id` instead of
`key`). The complete operation array is validated before one `GitStore::apply`
call; a bulk mutation is therefore one MCP call and one atomic store transaction.

Initialization and every mutating tool reconcile code history first. Divergence
returns `kind: diverged` until the client explicitly calls `memory_reconcile`
with `divergence: full_rebuild`.

The stable future-facing catalogue also declares search/backlinks, reindex,
remote transport, and model operations. Until their owning roadmap
specs land, calls fail explicitly with `kind: capability_unavailable`, the
planned spec, and an upgrade recovery action; the server never pretends that a
degraded implementation completed the operation.

## Errors

Domain failures use an MCP tool result with `isError: true` and
`structuredContent.error = {kind, message, data}`. Callers branch on `kind` and
`data`, not stderr text. Examples include `invalid_argument`, `invalid_record`,
`conflict`, `revision_not_found`, `transaction_reused`, and
`capability_unavailable`. Protocol lifecycle, unknown method/resource/tool, and
incompatible initialization failures use JSON-RPC errors with the same stable
machine-readable `data.kind` convention.
