# Memory Hub Compatibility Matrix

This document defines the version compatibility rules between Memory Hub
releases and the clients that consume its interface.

## Version components

Memory Hub uses three independent version dimensions:

| Dimension | Source | Example |
|---|---|---|
| **MCP protocol** | `MCP_PROTOCOL_VERSION` in `memory-hub-mcp` | `2025-11-25` |
| **Memory interface** | `MEMORY_INTERFACE_MAJOR` / `MEMORY_INTERFACE_MINOR` in `memory-hub-mcp` | `1.0` |
| **Crate version** | `version` in workspace `Cargo.toml` | `0.1.0` |

The crate version is the release tag (`v0.1.0`). The memory interface major
is the compatibility boundary consumers negotiate during `initialize`.

## Consumer handshake

Every consumer must send `_meta.memoryHub.memoryInterfaceVersion` in the
MCP `initialize` request:

```json
{
  "_meta": {
    "memoryHub": {
      "memoryInterfaceVersion": {"major": 1, "minor": 0}
    }
  }
}
```

The server rejects `initialize` when the consumer's major does not match
`MEMORY_INTERFACE_MAJOR`. A newer minor from the consumer is accepted
(rolling minor upgrade); an older minor is accepted with a deprecation note
in the response.

## Compatibility rules

| Consumer major | Server major | Outcome |
|---|---|---|
| Same | Same | Full compatibility |
| Newer minor | Same major | Accepted (rolling upgrade) |
| Older minor | Same major | Accepted (backward compatible) |
| Different major | Any | **Rejected** before any mutation |

A rejected `initialize` returns error `incompatible_memory_interface` with
`recovery_action: install_compatible_memory_hub`. No memory refs are created
and no store state is touched.

## Store / envelope / index versions

Internal on-disk formats are tracked separately and are not part of the
consumer handshake:

| Format | Version source | Current |
|---|---|---|
| **Store** | `storeVersion` in handshake | `1.1` |
| **Envelope** | `envelopeVersion` in handshake, `CURRENT_ENVELOPE_VERSION` in `memory-hub-core` | `1.2` |
| **Index** | `indexVersion` in handshake | `1.0` |

These are informational: the server manages format migrations internally.
Consumers must not assume a specific on-disk layout.

## Model fingerprint

The handshake includes `modelFingerprint` — a digest of the active embedding
model. Consumers use this to decide whether search results are comparable
across sessions:

- Same fingerprint → vector search results are directly comparable.
- Different fingerprint → vector distances may differ; consumers should
  re-rank or fall back to FTS-only results.

When no model is configured, `modelFingerprint` is `null` and the server
operates in FTS-only mode.

## Release artifacts

Each release publishes:

| Artifact | Description |
|---|---|
| `memory-hub-{target}.tar.gz` | Prebuilt binary per platform |
| `checksums.txt` | SHA-256 checksums of all archives |
| `install.sh` | One-liner installer |
| `sbom-advisories.json` | Security advisories report |
| `sbom-licenses.json` | License compliance report |
| `dependency-tree.txt` | Full dependency tree |
| `LICENSE` | Project license |

## Platform matrix

| `uname -sm` | Binary target | Default model |
|---|---|---|
| `Darwin arm64` | `aarch64-apple-darwin` | `bge-m3` |
| `Darwin x86_64` | `x86_64-apple-darwin` | `nomic-embed-text-v1.5` |
| `Linux x86_64` | `x86_64-unknown-linux-gnu` | `nomic-embed-text-v1.5` |

## Consumer integration checklist

Consumers (including Sync) must:

1. Send `memoryInterfaceVersion` in `initialize` `_meta`.
2. Pin a specific release version and checksum in their installation.
3. Use only the public MCP JSON-RPC interface — no private crate dependencies.
4. Run `memory-hub-contract` against the pinned binary in CI.
5. Handle `incompatible_memory_interface` errors gracefully.
6. Not assume on-disk format stability beyond the published store/envelope/index versions.
