# Sharing memory between machines

Memory has its own remote, separate from the code `origin`. Ordinary
`git clone`/`git push` never publish `refs/memory/*`; `memory-hub push` is an
explicit action that applies the effective push policy first.

```sh
memory-hub remote add <url>               # configure memory remote
memory-hub remote list                    # show the configured memory remote
memory-hub remote remove                  # forget it
memory-hub fetch --project /path/to/repo  # pull and merge memory refs
memory-hub push --project /path/to/repo   # publish memory refs (use --force to overwrite)
```

Merge is record-level: different keys merge automatically; the same key changed
by both sides returns both versions as a conflict. Records deleted on the
remote are kept locally — deletions do not replicate through merge; publish an
explicit delete instead.


## Memory does not arrive with a clone

`git clone` copies branches and tags. It does not copy `refs/memory/*`, and no
option makes it — so a colleague who clones a project with years of memory in
it opens an empty one. Nothing is lost and nothing is broken; the memory is
simply still on the remote.

`memory-hub doctor` is what tells the two apart, because from inside the clone
they look identical:

```sh
memory-hub doctor                         # in the fresh clone
# [error] memory.presence: memory exists on the code remote <url> but not in
# this repository: `git clone` does not copy refs/memory/* — run
# `memory-hub remote add <url>`, then `memory-hub fetch`
```

The check asks the code `origin` when no memory remote is configured yet, so it
works in a clone nobody has set up. It only asks when the local memory is
empty — a repository that has its memory pays no network call. When the remote
carries no memory either, the check passes and says so: an empty project is a
normal state, not a failure.

The `kind` field is stable for scripts and for consumers rendering the report:
`memory_not_fetched` (it is on the remote), `no_memory_anywhere` (there is none
yet), `remote_unreachable` (the question could not be asked).
## Who can read what a remote holds

Memory refs are ordinary Git objects, so whoever can read the repository they
were pushed to can read the memory in it. That is the whole access model, and
it is deliberate: a private repository for the memory is what keeps it private,
not a key Memory Hub would have to distribute, rotate and lose.

Nothing is published by accident. `refs/memory/*` are not part of a normal
`git push`, so memory stays on the machine that wrote it until somebody
configures a remote for it — and that remote does not have to be the code one.
A public repository for the code and a private one for its memory is one
`memory-hub remote add` apart.

A configured refspec may only move `refs/memory/*`, and a remote URL that Git
would read as an option (`--upload-pack=…`) or as a remote helper (`ext::…`) is
rejected — code refs stay untouched and a repository's config cannot turn a
fetch into command execution.
