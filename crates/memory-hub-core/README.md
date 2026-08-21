# memory-hub-core

Product-neutral durable contract used by the Memory Hub store, index, MCP
server, and independent clients.

The module owns three things:

- the versioned plaintext envelope, including all data needed for rebuild and
  code-history reconciliation;
- policy resolution with explicit `default`, `project`, and `client` sources.

Envelope major versions are compatibility boundaries. Newer minor versions are
accepted, and unknown envelope fields plus unknown `profile.metadata` values are
retained through a JSON decode/encode round trip. Client profile versions are
independent from the envelope version.

Call `Envelope::validate` (or `StoredRecord::validate`) immediately before a
durable write. Deserialization performs the same validation, so an incompatible
major version or stale content hash cannot enter the store through decoded wire
data.
