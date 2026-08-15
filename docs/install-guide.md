# Git Memory — Installation Guide

Git Memory is a standalone, Git-backed project memory system. It persists
knowledge (decisions, constraints, specs, observations) in Git objects and
exposes them through the Model Context Protocol (MCP).

## Quick install

```sh
curl -fsSL https://git-memory.dev/install.sh | sh
```

This downloads the binary, verifies the checksum, installs it to
`~/.local/bin`, and downloads the platform-default embedding model.

## What gets installed

| Path | Contents |
|---|---|
| `~/.local/bin/git-memory` | The binary |
| `~/.config/git-memory/config.json` | Active model selection |
| `~/.config/git-memory/registry.json` | Installation registry (consumers, repositories) |
| `~/.cache/git-memory/models/` | Downloaded embedding models |

**Project data** (memory refs, records) lives inside each project's `.git/`
directory and is never touched by installation or uninstallation.

## Shared lifecycle

Git Memory is designed to be shared by multiple consumers (Sync, custom
clients, third-party tools). One consumer does not get to break or delete
the installation used by others.

### Consumer registration

Each consumer registers itself with a required major version:

```sh
git memory registry register-consumer sync 1
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
git memory uninstall --yes

# Remove everything (binary, config, models, registry)
git memory uninstall --purge --yes
```

Uninstalling one consumer (e.g. Sync) only unregisters it:

```sh
git memory registry unregister-consumer sync
```

This does **not** remove git-memory, memory refs, encryption keys, or
search indexes. Other consumers continue to work.

## Manual install

If you prefer not to use the install script:

1. Download the binary for your platform from [GitHub releases](https://github.com/evseevnn/git-memory/releases)
2. Verify the SHA-256 checksum against `checksums.txt`
3. Place the binary in your PATH (e.g. `~/.local/bin/`)
4. Run `git memory setup` to download an embedding model
5. Run `git memory doctor` to verify the installation

## First run

```sh
# Start the MCP server (works without a model in FTS-only mode)
git memory mcp

# Run the setup wizard to download a model
git memory setup

# Check installation health
git memory doctor
```

## MCP client configuration

Add git-memory to your MCP client (Claude Desktop, Sync, etc.):

```json
{
  "mcpServers": {
    "git-memory": {
      "command": "git-memory",
      "args": ["mcp"]
    }
  }
}
```

## Platform support

| Platform | Binary target | Default model |
|---|---|---|
| macOS (Apple Silicon) | `aarch64-apple-darwin` | `bge-m3` |
| macOS (Intel) | `x86_64-apple-darwin` | `nomic-embed-text-v1.5` |
| Linux (x86_64) | `x86_64-unknown-linux-gnu` | `nomic-embed-text-v1.5` |

## Data preservation

Git Memory never deletes your data without explicit confirmation:

- **Uninstall binary** → config, models, registry, and all project memory preserved
- **Uninstall consumer** → only removes from registry; binary and all data preserved
- **`--purge`** → removes config, models, and registry; project memory in `.git/` is still preserved

Project memory lives in `.git/refs/memory/` inside each repository and is
portable with the repository. It is never stored in a central location.
