# Storage Layout — dossier mesh v0

How dossier persists projects on disk. This is the mesh implementation's
storage contract, separate from the [PROTOCOL.md](PROTOCOL.md) wire spec.

Two design rules drive every choice here:

1. **Git is the source of truth.** The mesh is a projection — restartable,
   rebuildable, replaceable. If the mesh dies, the corpus survives.
2. **Humans can read and edit directly.** A person with a text editor can
   open a project file and understand it. The protocol is for agents; the
   files are for everyone.

## Corpus root

A *corpus* is any directory containing a `.dossier/` marker. The corpus is
expected to be a git repo (the mesh does not require it, but loses
provenance and conflict-resolution if it isn't).

```
<corpus-root>/
  .dossier/
    config.toml          # corpus-level config (mesh version, defaults)
  projects/
    <project-slug>/
      project.md
      phases/
        01-<phase-slug>.md
        02-<phase-slug>.md
      tasks/
        <task-id>-<task-slug>.md
      artifacts.jsonl
```

## IDs

ULID with type prefix: `prj_01HFG...`, `phs_01HFG...`, `tsk_01HFG...`,
`art_01HFG...`. Time-sortable, collision-free, URL-safe. Generated server-
side by default; clients may supply their own (the mesh accepts any
prefix-correct ULID).

Slugs are human-chosen and unique within their parent (project slug unique
in corpus, phase slug unique in project, task slug unique in project).

## project.md

YAML frontmatter, markdown body. The body **is** the project's design doc.

Frontmatter is YAML 1.2 core. The 1.1 boolean words (`yes`, `no`, `on`,
`off`) are plain strings and may appear unquoted — parse the corpus with a
1.2-core resolver, not a YAML 1.1 one.

```markdown
---
id: prj_01HFG3J7K8N9P0Q1R2S3T4V5W6
slug: dossier
title: dossier — agent project protocol and reference mesh
status: active        # planning | active | paused | done | abandoned
created_at: 2026-05-10T14:30:00Z
updated_at: 2026-05-10T14:30:00Z
created_by: human:mh
---

# dossier

Free-form markdown. Goal, motivation, non-goals, references, whatever
the project needs. The protocol does not interpret the body.
```

## phases/NN-<slug>.md

`NN` is a zero-padded two-digit order prefix that determines linear order
on disk. The mesh treats `order` in frontmatter as authoritative; the
filename prefix is for human readability and stable git diffs.

```markdown
---
id: phs_01HFG3J7K8N9P0Q1R2S3T4V5W7
project: prj_01HFG3J7K8N9P0Q1R2S3T4V5W6
slug: protocol-spec
title: Draft v0 protocol spec
order: 1
status: done          # pending | active | done | skipped
created_at: 2026-05-10T14:30:00Z
updated_at: 2026-05-10T15:10:00Z
created_by: human:mh
owner: human:mh
---

Acceptance: PROTOCOL.md exists and covers primitives, verbs, state
machine, identity, versioning. No storage details (those go in LAYOUT.md).
```

## tasks/<id>-<slug>.md

Each task is a file. Notes (the progress log) live as an append-only
markdown section inside the same file — keeps everything for one task in
one place a human can read.

```markdown
---
id: tsk_01HFG3J7K8N9P0Q1R2S3T4V5W8
project: prj_01HFG3J7K8N9P0Q1R2S3T4V5W6
phase: phs_01HFG3J7K8N9P0Q1R2S3T4V5W7
slug: write-protocol-md
title: Write PROTOCOL.md v0
status: done          # todo | claimed | in_progress | blocked | done | cancelled
assignee: claude-code:michael
claimed_at: 2026-05-10T14:35:00Z
completed_at: 2026-05-10T15:05:00Z
created_at: 2026-05-10T14:30:00Z
updated_at: 2026-05-10T15:05:00Z
depends_on: [tsk_01KSDVQST4YP73CYV9K7G7GZ8G]   # optional; empty / absent means no deps
---

## Spec

Cover: primitives, verbs, state machine, identity, versioning, scope.
Strict-not-permissive on unknown fields.

## Notes

- 2026-05-10T14:35 — claude-code:michael: claimed
- 2026-05-10T14:50 — claude-code:michael: primitives + verbs drafted, working state machine
- 2026-05-10T15:05 — claude-code:michael: complete; PROTOCOL.md written
```

The mesh appends to `## Notes` with a single line per write. If `## Notes`
is missing, it appends the heading then the line.

## artifacts.jsonl

One JSON object per line, append-only. Artifacts are dense small records
with no body — files would be wasteful and noisy in git diffs.

```jsonl
{"id":"art_01HFG...","project":"prj_01HFG...","task":"tsk_01HFG...","kind":"file","ref":"PROTOCOL.md","label":"v0 spec","linked_at":"2026-05-10T15:05:00Z","actor":"claude-code:michael"}
{"id":"art_01HFG...","project":"prj_01HFG...","task":"tsk_01HFG...","kind":"commit","ref":"abc123","label":"initial spec commit","linked_at":"2026-05-10T15:06:00Z","actor":"claude-code:michael"}
```

`kind` is one of `commit | pr | file | url | run | doc` (extensible — the
protocol allows future kinds, the mesh accepts any string and round-trips
unknown kinds untouched).

## Concurrency

The mesh assumes single-writer-per-corpus by default. Writes are:

1. Read current file
2. Mutate in memory
3. Write atomically (temp file + rename)

For `artifacts.jsonl`: open with `O_APPEND` and a file lock for the duration
of the append. Append-only avoids most concurrency pain.

Cross-process / multi-mesh coordination: out of scope for v0. If you have
two meshes pointed at the same corpus, you will eventually corrupt it.
Solve later with a lockfile or by leaning on git's merge.

## What lives in `.dossier/config.toml`

Reserved for v0; the file may be empty. Future contents:

- `mesh_version` — schema version of the on-disk layout
- `actor_default` — default actor when none supplied
- `id_generator` — `ulid` (default) | `uuid` | `nano`

## What does NOT live on disk

- **Indexes** — semantic embeddings, full-text indexes, link graphs. These
  are mesh-internal, rebuildable from the corpus, and live in
  `.dossier/cache/` (gitignored).
- **Locks** — runtime only.
- **Subscriptions / queries / sessions** — runtime only.

## Migration

The on-disk layout is versioned via `mesh_version` in
`.dossier/config.toml`. Breaking changes ship a migration tool. v0 is
pre-1.0; expect churn.
