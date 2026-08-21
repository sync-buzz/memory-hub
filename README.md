# Memory Hub

**One interface an agent can use to remember a project, over whichever storage
that project chose.**

An agent that works on your code needs somewhere to keep what it learns —
decisions, constraints, notes, the documentation it wrote — and somewhere to
look it up next time. Every tool solves that its own way: a database here, a
folder of Markdown there, a hosted service somewhere else. The record ends up
where the tool lives instead of where the project lives, and the next tool
starts from nothing.

Memory Hub is the layer in between. A project declares where its records are
kept; agents and applications talk to one interface and never learn which
storage answered.

```
    agent (MCP)  ─┐
                  ├─►  Memory Hub  ─►  Git objects · plain files
  your app (Rust) ─┘                    + a local search index
```

## Two ways to use it

**As an MCP server.** Memory Hub speaks the Model Context Protocol over stdio,
so any MCP client — Claude Code, Cursor, VS Code, your own — connects to it and
gets tools for reading, writing and searching a project's memory.

```json
{
  "mcpServers": {
    "memory": {
      "command": "memory-hub",
      "args": ["mcp", "--project", "/absolute/path/to/your/project"]
    }
  }
}
```

**As a Rust library.** The same engine links into your program: typed calls,
typed errors, no process to supervise and no JSON to parse. See
[docs/embedding.md](docs/embedding.md).

## Quick start

```sh
./build.sh && ./install.sh                 # or take a binary from a release
cd /path/to/your/project

memory-hub init --records git-metadata     # decide where records live
memory-hub model download bge-m3           # optional: search by meaning
memory-hub mcp --project "$PWD"            # what an MCP client runs
```

[docs/install-guide.md](docs/install-guide.md) has the longer version,
including the model and what to do without one.

## Where a project's records can live

| Storage | What it is | Good for |
| --- | --- | --- |
| `git_metadata` | Git objects under `refs/memory/*`, outside your branches and your working tree | memory that travels with the repository and never shows up in a diff |
| `directory` holding records | one JSON file per record in a directory | a project that is not a Git repository, or one that wants its memory as plain files |
| `directory` holding content | the *content* of records is your own files — `docs/*.md` and the rest | documentation the team already writes and reviews, with the records tracking it |

A project declares its storages once, in a file it commits, and a type points at
one by name. Nothing above that layer knows which is which:
[docs/storages.md](docs/storages.md).

## What it does besides storing

- **Search that answers in words and in meaning.** Full-text first, a vector
  channel when the words run out, one ranked answer.
- **Freshness that is measured, not declared.** Memory Hub reads your code
  history and marks a record whose files have moved on, so nobody trusts a
  claim about a function that changed last month.
- **Documents where they already are.** A type can keep its content in the
  repository's own files; Memory Hub writes nothing into them and follows them
  when they are renamed or moved.
- **Types the project owns.** What a decision or a specification is here is
  declared by the project and validated against its schema.
- **Memory that can be shared, or not.** Records stay on your machine until you
  push them to a memory remote of their own — an ordinary `git push` never
  carries them.

## Documentation

| | |
| --- | --- |
| [Install guide](docs/install-guide.md) | building, installing, models |
| [Storages](docs/storages.md) | declaring where records live, moving a type's content |
| [Documents in the repository](docs/documents.md) | attached folders, scanning, branches, folder trees |
| [The MCP interface](docs/mcp.md) | how a client connects, and how it hears about changes |
| [Embedding in Rust](docs/embedding.md) | using Memory Hub as a library |
| [The command line](docs/cli.md) | every command, model management, exit codes, platforms |
| [Sharing memory](docs/remote.md) | memory remotes, fetch and push, who can read what |
| [Architecture](docs/architecture.md) | the crates, the storage contract, the index, the contract harness |
| [Compatibility](docs/compatibility-matrix.md) | interface versions and what a client must negotiate |

## License

Memory Hub is licensed under FSL-1.1-MIT. See [LICENSE](LICENSE).
