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
  "indexVersion": {"major": 1, "minor": 0},
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
| `memory://records/{key}` | `{revision, record: StoredRecord\|null}` |
| `memory://records/summary` | `{revision, total, by_kind, by_freshness, archived, live}` |
| `memory://index/status` | `{schemaVersion, available, state, canonicalRevision, targetRevision, fingerprint}` |
| `memory://model/status` | `{schemaVersion, available, model_id, dimensions, runtime_state, vector_search}` |
| `memory://policy/effective` | `{schemaVersion: 1, policies: EffectivePolicy[]}` |
| `memory://encryption/status` | `{mode, available, encryptedStoreAvailable, encryptedIndexAvailable, ephemeralIndex}` |

`memory://records/summary` returns aggregate counts (total, by_kind, by_freshness,
archived/live) over the full live corpus without pagination — for dashboard use.

Subscribe with MCP `resources/subscribe` to
`memory://revision/current`. After a successful transaction/import the server
sends `notifications/resources/updated`. The notification carries only the URI:
the client must reread the resource and treat its revision as authoritative.

## Tools

`tools/list` is the canonical JSON Schema catalogue. The implemented surface
spans store, read, search, transport, model, and encryption operations:

### Store and history

| Tool | Required input | Result |
| --- | --- | --- |
| `memory_apply_transaction` | `transaction_id`, `expected_revision`, non-empty `operations[]` | `{revision, changed_keys}` |
| `memory_checkpoint` | `message` | `Checkpoint` |
| `memory_history` | optional `limit` | `{checkpoints}` |
| `memory_diff` | `from_revision`, `to_revision` | `{fromRevision, toRevision, changes}` |
| `memory_export` | `revision` | `{revision, bundle}` |
| `memory_import` | `transaction_id`, `expected_revision`, `bundle` | `{revision, changed_keys}` |
| `memory_doctor` | none | repository/store health |
| `memory_reindex` | none | current durable projection status |

### Read and search

| Tool | Required input | Result |
| --- | --- | --- |
| `memory_get_record` | `key`, `revision` | `{revision, record}` |
| `memory_list_records` | none (all params optional) | `{revision, records, total, limit, offset, has_more, counts}` |
| `memory_search` | `query` | `{hits, total, limit, offset, has_more, mode, degraded, revision}` |
| `memory_backlinks` | `key` (optional `revision`) | `{entries}` |

`memory_list_records` accepts `limit` (max 200), `offset`, `kind`, `tags` (AND),
`archived`, `freshness`, `sort` (`key`/`kind`/`title`/`freshness`/`archived`),
`sort_order` (`asc`/`desc`), and `metadata_only`. The response always includes
`counts` (total, by_kind, by_freshness, archived/live) over the full filtered
corpus — even on page 2 — so a UI can render facet tabs without a second call.

`memory_search` runs BM25 full-text search across title/content/kind with the
same filters as `memory_list_records`. When an embedding model is attached and
BM25 returns fewer than 5 hits, a vector kNN rescue channel fires: hits below a
0.35 cosine floor are discarded and the two channels are fused via Reciprocal
Rank Fusion (`combined_rank = 1/(60+bm25_rank) + 1/(60+vec_rank)`). The result
reports `mode` (`fts` or `hybrid`) and `degraded` (`true` only when no embedding
model is available). Each hit carries `fts_score`, `vector_score`, and
`combined_rank`; sort is by `combined_rank` descending.

### Reconcile and transport

| Tool | Required input | Result |
| --- | --- | --- |
| `memory_reconcile` | optional `divergence: report\|full_rebuild` | `ReconcileReport` |
| `memory_transport_status` | none | remote config and sync status |
| `memory_fetch` | none | fetch result and merged keys |
| `memory_push` | optional `force` | push result |

### Model and encryption

| Tool | Required input | Result |
| --- | --- | --- |
| `memory_model_status` | none | embedding model status |
| `memory_encryption_status` | none | current plaintext/encryption availability |
| `memory_unlock` | `identity_path` | `{unlocked, revision, indexRebuilt}` |
| `memory_lock` | none | `{locked, indexDestroyed}` |
| `memory_init_encrypted` | `identity_path`, `recipient_public_key` | `{backup_identity}` |
| `memory_list_recipients` | none | `{recipients}` |
| `memory_add_recipient` | `public_key` | re-encryption result |
| `memory_remove_recipient` | `public_key` | re-encryption + index rebuild result |

A transaction operation is either `{"op":"put","record":StoredRecord}` or
`{"op":"delete","key":"..."}` (opaque callers may supply `id` instead of
`key`). The complete operation array is validated before one `GitStore::apply`
call; a bulk mutation is therefore one MCP call and one atomic store transaction.

Initialization and every mutating tool reconcile code history first. Divergence
returns `kind: diverged` until the client explicitly calls `memory_reconcile`
with `divergence: full_rebuild`.

## Errors

Domain failures use an MCP tool result with `isError: true` and
`structuredContent.error = {kind, message, data}`. Callers branch on `kind` and
`data`, not stderr text. Examples include `invalid_argument`, `invalid_record`,
`conflict`, `revision_not_found`, `transaction_reused`, `locked`,
`identity_load_failed`, `push_blocked`, and `capability_unavailable`. Protocol
lifecycle, unknown method/resource/tool, and incompatible initialization failures
use JSON-RPC errors with the same stable machine-readable `data.kind` convention.
