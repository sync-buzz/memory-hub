# Memory Hub — Installation Guide

Memory Hub is a project memory engine. It keeps a project's records in the
storage that project declares — Git objects, a folder of plain files, or
age-encrypted Git objects — and serves them through the Model Context Protocol
(MCP).

## Build & install

Memory Hub is built from source. You need Rust (install via
[rustup](https://rustup.rs)).

```sh
# From the repository root:

./build.sh           # cargo build --release

./install.sh         # installs to ~/.local/bin (add it to PATH)
# or:
./install.sh --install-dir /usr/local/bin
./install.sh --skip-model   # binary only, download model later
```

`install.sh` will run `build.sh` automatically if no built binary is found.

This builds the binary, installs it to `~/.local/bin` by default, and
downloads the platform-default embedding model. Use `--skip-model` to
install the binary only.

## What gets installed

| Path | Contents |
|---|---|
| `~/.local/bin/memory-hub` | The binary |
| `~/.config/memory-hub/config.json` | Active model selection |
| `~/.config/memory-hub/registry.json` | Installation registry (consumers, repositories) |
| `~/.cache/memory-hub/models/` | Downloaded embedding models |

**Project data** (memory refs, records) lives inside each project's `.git/`
directory and is never touched by installation or uninstallation.

## Shared lifecycle

Memory Hub is designed to be shared by multiple consumers (Sync, custom
clients, third-party tools). One consumer does not get to break or delete
the installation used by others.

### Consumer registration

Each consumer registers itself with a required major version:

```sh
memory-hub registry register-consumer sync 1
```

The registry tracks:
- **Installation** — binary path, version, checksum
- **Consumers** — name, required major version, registration timestamp
- **Repositories** — known project paths (for uninstall warnings)

The registry stores **no** project content, keys, or credentials.

### Compatibility

Consumers are compatible when their required major version matches the
installation's major version. See [compatibility matrix](compatibility-matrix.md)
for full rules.

- Same major → full compatibility
- Different major → rejected before any mutation

### Uninstall

```sh
# Remove the binary only — data preserved
memory-hub uninstall --yes

# Remove everything (binary, config, models, registry)
memory-hub uninstall --purge --yes
```

Uninstalling one consumer (e.g. Sync) only unregisters it:

```sh
memory-hub registry unregister-consumer sync
```

This does **not** remove memory-hub, memory refs, encryption keys, or
search indexes. Other consumers continue to work.

## Manual build

If you prefer to run cargo directly:

1. `cargo build --release` (or `cargo build` for a debug build)
2. Copy `target/release/memory-hub` to your PATH (e.g. `~/.local/bin/`)
3. Run `memory-hub setup` to download an embedding model
4. Run `memory-hub doctor` to verify the installation

## First run

```sh
# Start the MCP server (works without a model in FTS-only mode)
memory-hub mcp

# Run the setup wizard to download a model
memory-hub setup

# Check installation health
memory-hub doctor
```

## MCP client configuration

Add memory-hub to your MCP client (Claude Desktop, Sync, etc.):

```json
{
  "mcpServers": {
    "memory-hub": {
      "command": "memory-hub",
      "args": ["mcp"]
    }
  }
}
```

## Platform support

| Platform | Binary target | Release archive | Default model |
|---|---|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` | `.tar.gz` | `bge-m3` |
| macOS (Intel) | `x86_64-apple-darwin` | `.tar.gz` | `nomic-embed-text-v1.5` |
| Linux (x86_64) | `x86_64-unknown-linux-gnu` | `.tar.gz` | `nomic-embed-text-v1.5` |
| Windows (x86_64) | `x86_64-pc-windows-msvc` | `.zip` | `nomic-embed-text-v1.5` |

`install.sh` covers the Unix targets. On Windows, unpack the archive and place
`memory-hub.exe` on `PATH` yourself.

## Data preservation

Memory Hub never deletes your data without explicit confirmation:

- **Uninstall binary** → config, models, registry, and all project memory preserved
- **Uninstall consumer** → only removes from registry; binary and all data preserved
- **`--purge`** → removes config, models, and registry; project memory in `.git/` is still preserved

Project memory lives in `.git/refs/memory/` inside each repository and is
portable with the repository. It is never stored in a central location.
