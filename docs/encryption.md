# Encryption

Memory Hub supports optional encrypted mode using [age](https://age-encryption.org).
When enabled, all record content and metadata are encrypted before reaching
the Git tree. The `memory-hub-crypto` crate is a thin wrapper around the
`age` crate (with SSH key support), and `memory-hub-store` provides an
`EncryptedStore` that transparently encrypts and decrypts records.

## How it works

Encryption uses **age** with **SSH keys** as recipient identities. Most
developers already have an SSH key on GitHub — Memory Hub reuses it:

- **Public key** (on GitHub) is used as an age recipient for encryption
- **Private key** (`~/.ssh/id_ed25519`) is used as an age identity for decryption
- Every record and the manifest are encrypted to all recipients in the list
- Only people whose keys are in the recipients list can decrypt

For users without SSH keys, Memory Hub generates an age-native X25519
keypair as a fallback. A backup X25519 keypair is always generated for
recovery.

## Access model

Two independent gates, both required for access:

- **Git access** — can clone/fetch the encrypted data from the repository
- **Crypto access** — SSH/X25519 key is in the recipients list, can decrypt

A collaborator with Git access but no crypto key sees encrypted blobs but
cannot read them. The project owner controls the recipients list through
`memory-hub encryption add/remove`.

## What is encrypted

In encrypted mode the Git tree contains no plaintext: record payloads,
semantic keys, titles, kinds, tags, and links are all inside the encrypted
manifest or encrypted record blobs. Only unavoidable Git metadata (refs,
object counts, timestamps, commit graph) remains visible.

## Ephemeral index

For encrypted projects, the LanceDB index contains plaintext derived from
decrypted records. To avoid persisting plaintext on disk, the index is
**ephemeral**: it is rebuilt from decrypted records on `memory_unlock` and
destroyed on `memory_lock`. If the MCP process crashes before `memory_lock`
runs, the next session start wipes the stale index directory before serving any
request — no plaintext survives a crash/restart cycle.

## Key operations (via MCP)

Encryption is managed through MCP tools, not CLI subcommands:

```
memory_init_encrypted   → initialize encrypted store with first recipient
memory_unlock           → decrypt with an identity, rebuild ephemeral index
memory_lock             → drop identity, destroy ephemeral index
memory_add_recipient    → add team member, re-encrypt all records
memory_remove_recipient → remove member, re-encrypt, rebuild index
memory_list_recipients  → show recipients in the manifest
memory_encryption_status → check current lock state
```

`memory_unlock` accepts either identity format: an OpenSSH private key
(`~/.ssh/id_ed25519`) for everyday use, or the age-native backup key
(`AGE-SECRET-KEY-1…`) returned by `memory_init_encrypted` — the recovery path
when the SSH key is lost. The format is detected from the file's content.

Every operation on an encrypted project goes through the encrypted store:
`memory_import` encrypts the bundle it is given, `memory_export` requires an
unlocked store and deliberately produces plaintext, and reads, searches and
backlinks return a `locked` error until `memory_unlock` runs.
