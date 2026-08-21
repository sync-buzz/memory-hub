# Storages

Where a project's **records** go is the host's answer, given when it opens the
project — the engine does not decide for the product that embeds it whether
memory travels inside Git or sits beside it, and there is nothing on disk that
could disagree with the host that gave the answer.

```sh
memory-hub mcp --records git-metadata     # Git objects under refs/memory/*
memory-hub mcp --records directory        # one JSON file per record
```

Unsaid, this command line answers from the project: Git's metadata in a
repository, a directory anywhere else. Another host answers however it likes —
Sync always says `git_metadata`, because a project there is a repository.

Where a **type's documents** go is written in the type, as the directory
itself:

```jsonc
// the content of a `__type__` record
{"kind_name": "doc", "storage": "docs"}
```

Absent means the bodies sit in the records, which is what every type was before
storage became a choice. A path means a directory of the working tree: the
files are the team's, Git versions them, a pull request shows them in its diff,
and Memory writes nothing into them.

One folder per type. There is no file-name mask, so *every* file in the folder
is a document of the type that names it, and two types over one folder would
both claim every new file in it. `memory_delete_type` removes the type it is
asked about and the records that mirror the folder; the files are left exactly
where they are — see [Deleting one](documents.md#deleting-one) for why removing
a type and deleting its records are different operations.

## Moving a type to another storage

Where a type's records live is a field of its definition, and definitions get
edited. Letting the data follow an edited field would be data loss wearing the
clothes of a setting, so editing `storage` is **refused** while it would leave
records behind, and moving them is an operation of its own:

```json
{ "name": "memory_migrate_storage",
  "arguments": { "kind": "doc", "dry_run": true, "storage": "docs" } }
```

`storage` is the directory to move into. `"storage": null` brings the content
back into the records; an absent field is not the same thing and is refused:
`null` says "here", absent says the caller forgot to say.

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
again resumes: records that already live where the new folder says are not
records left behind.
