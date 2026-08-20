# Storages

A project says once where its records go, and the declaration is committed
alongside the code, so everyone who clones the project reads the same answer:

```sh
memory-hub init --records refs             # Git objects under refs/memory/*
memory-hub init --records folder           # one JSON file per record
memory-hub declare-storage docs --kind repo-folder --path docs
```

There is no default. The engine does not decide for the product that embeds it
whether memory should travel inside Git or sit beside it.

```jsonc
// .memory/config.json
{"config_version": 1,
 "storages": {
   "main": {"kind": "refs", "holds": ["records", "content"]},
   "docs": {"kind": "repo_folder", "path": "docs", "holds": ["content"]}}}
```

Exactly one storage holds records. Others hold content, and a type points at
one by name.

## Moving a type to another storage

Where a type's records live is a field of its definition, and definitions get
edited. Letting the data follow an edited field would be data loss wearing the
clothes of a setting, so editing `storage` is **refused** while it would leave
records behind, and moving them is an operation of its own:

```json
{ "name": "memory_migrate_storage",
  "arguments": { "kind": "doc", "dry_run": true, "storage": "docs" } }
```

`"storage": null` brings the content back into the records. An absent field is
not the same thing and is refused: `null` says "here", absent says the caller
forgot to say.

`dry_run` returns the plan — which records move, in which direction, and what
has to be accepted — and writes nothing. Every warning code the plan lists must
be echoed in `acknowledge` before the migration runs. A boolean nobody reads is
not consent, and the two directions are not asking the same thing:

- **into the working tree** — `content_becomes_visible`: the content of every
  record is written where the whole team sees it in diffs and reviews. That is
  a change of visibility, not a technical detail.
- **back into refs** — `does_not_hide_published_history`: the plaintext blobs
  stay in Git history for good. This changes where new writes go; it is never
  retroactive privacy. And `files_are_left_in_place`: Memory does not delete
  files it did not put there on this run.

Content is written before the records that point at it, and the records move
before the definition that describes them. Interrupted anywhere, running it
again resumes: records that already live where the new storage says are not
records left behind.
