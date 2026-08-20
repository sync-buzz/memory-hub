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
by both sides returns both versions as a conflict. Encrypted merge decrypts,
merges, and re-encrypts in one step (requires `memory_unlock` first). Records
deleted on the remote are kept locally — deletions do not replicate through
merge; publish an explicit delete instead.


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

One thing the fetch will insist on: verification is fail-closed, so
`memory-hub fetch` refuses history it cannot verify until you configure an
allowed signer or opt out explicitly — see
[Signing and verification](#signing-and-verification).
## Encryption is the storage's, not the type's

A type cannot ask to be encrypted, and a `storage` section that tries is
refused by name. `refs` encrypts the whole store at once or not at all, and a
folder of ordinary repository files cannot be encrypted and stay ordinary —
which is the entire reason it is a folder of ordinary repository files.

Nothing extra is needed for records whose content is outside. Their links,
tags, title and freshness live in the envelope, and the envelope is in `refs`:
encrypt `refs` and they are encrypted with it. The file holds the content and
nothing else, which is what makes attaching one invisible in the first place.

A locked project answers a search with `locked`, never with an empty result.
The two look the same and mean opposite things — one says look elsewhere, the
other says unlock and look again.

Turning encryption on protects future writes only. The plaintext blobs already
in Git history stay readable to anyone with the repository; `memory-hub doctor`
says so on every plaintext project rather than leaving it to be discovered.

## Signing and verification

GitHub rulesets do not protect `refs/memory/*`, so any collaborator with push
access can rewrite Memory refs. Signatures are the only real protection, and
Memory Hub treats the two directions differently:

- **Signing is opt-in.** Point `memory-hub.signing.key` at a private key, or
  let Memory Hub reuse Git's own SSH signing key (`gpg.format = ssh` plus a
  `user.signingkey` that names a file). Every Memory commit is then SSH-signed.
- **Verification is fail-closed.** `memory-hub fetch` refuses to import history
  it cannot verify. Configure the keys you trust, or opt out explicitly:

```sh
git config memory-hub.signing.key ~/.ssh/id_ed25519
git config --add memory-hub.signing.allowedSigner "ssh-ed25519 AAAA... alice"
git config memory-hub.signing.allowedSignersFile .memory-hub/allowed_signers
git config memory-hub.signing.verify off     # accept unsigned memory refs
```

For encrypted projects the manifest's SSH recipients are trusted automatically,
so a team that already shares keys needs no extra configuration.

A configured refspec may only move `refs/memory/*`, and a remote URL that Git
would read as an option (`--upload-pack=…`) or as a remote helper (`ext::…`) is
rejected — code refs stay untouched and a repository's config cannot turn a
fetch into command execution.
