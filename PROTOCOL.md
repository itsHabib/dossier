# Data model — v0

The primitives and verbs dossier exposes over MCP. This document is the
*data-model* contract — what a `Project` looks like, what a `Task` is,
what transitions the state machine allows. It is **not** a multi-
implementer wire spec: dossier is a tool we run for our own agents, not
a reference implementation a third party will conform to (see
[docs/vision.md](docs/vision.md)).

If you're curious about the on-disk format, that lives in [LAYOUT.md](LAYOUT.md).

## Status

v0 — minimal core. The smallest surface that lets a single implementer
agent (Claude / Cursor / ship) drive a project end-to-end while the
operator queries it. Semantic / vector / RAG search, multi-tenant auth,
cross-project relationships, conflict detection, and decision tracking
are explicitly out of scope; see the vision doc.

## Roles

The protocol does not enforce roles — any agent may call any verb — but
three patterns recur:

- **Orchestrator** — creates projects, adds phases, defines tasks. Often a
  human via a frontend, sometimes an agent kicking off work from a higher-
  level goal.
- **Implementer** — claims tasks, posts progress, links artifacts, marks
  tasks complete. Examples: ship, a code-writing agent, a human in a
  terminal.
- **Reader** — queries state to answer questions ("what's going on with
  project X", "what's blocking task Y"). Examples: a manager-facing chat
  agent, a CLI status command.

## Primitives

All primitives carry: `id` (server-assigned, stable), `created_at`,
`updated_at` (ISO-8601), and `actor` on every write (free-form string —
agent name or human handle; servers may validate, the protocol does not).

### Project

A unit of work with a goal. Holds phases, tasks, and artifacts.

| field         | type                                              | notes                       |
|---------------|---------------------------------------------------|-----------------------------|
| `id`          | string                                            | server-assigned             |
| `slug`        | string                                            | client-supplied, unique     |
| `title`       | string                                            |                             |
| `description` | string (markdown)                                 | the design doc body         |
| `status`      | `planning` \| `active` \| `paused` \| `done` \| `abandoned` |                |
| `created_at`  | timestamp                                         |                             |
| `updated_at`  | timestamp                                         |                             |

### Phase

An ordered subdivision of a project. Phases are linear, not a graph.

| field        | type                                        | notes                  |
|--------------|---------------------------------------------|------------------------|
| `id`         | string                                      | server-assigned        |
| `project`    | string                                      |                        |
| `title`      | string                                      |                        |
| `body`       | string (markdown)                           | phase doc / acceptance |
| `order`      | integer                                     | dense, server-managed  |
| `status`     | `pending` \| `active` \| `done` \| `skipped` |                        |

### Task

A discrete piece of work an implementer can claim and complete.

| field          | type                                                              | notes                         |
|----------------|-------------------------------------------------------------------|-------------------------------|
| `id`           | string                                                            |                               |
| `project`      | string                                                            | owning project's id (`prj_…`) |
| `project_slug` | string                                                            | slug of the owning project; derived from corpus path, not stored in frontmatter |
| `phase`        | string \| null                                                    | nullable for project-wide tasks |
| `title`        | string                                                            |                               |
| `body`         | string (markdown)                                                 | spec / acceptance criteria    |
| `status`       | `todo` \| `claimed` \| `in_progress` \| `blocked` \| `done` \| `cancelled` |                      |
| `assignee`     | string \| null                                                    | actor that holds the claim    |
| `claimed_at`   | timestamp \| null                                                 |                               |
| `completed_at` | timestamp \| null                                                 |                               |
| `notes`        | array of `{ actor, body, posted_at }`                             | append-only progress log      |

### Artifact

A pointer to something concrete the work produced or depends on.

| field         | type                                                          | notes                               |
|---------------|---------------------------------------------------------------|-------------------------------------|
| `id`          | string                                                        |                                     |
| `project`     | string                                                        |                                     |
| `task`        | string \| null                                                |                                     |
| `kind`        | `commit` \| `pr` \| `file` \| `url` \| `run` \| `doc`         | extensible                          |
| `ref`         | string                                                        | sha / url / path / run id           |
| `label`       | string                                                        | short human-readable                |
| `linked_at`   | timestamp                                                     |                                     |
| `actor`       | string                                                        | who linked the row                  |
| `meta`        | map string → string, optional                                 | flat denormalized summary; omitted when empty. Caps: ≤16 keys, key ≤64 bytes, value ≤512 bytes, ≤4 KiB total serialized. Unknown keys/values round-trip untouched. Immutable for an existing `(task, kind, ref)` — correction is via supersede (distinct `ref` + `meta.supersedes`), not mutation. |

## Slug scope

Phase and task slugs are unique **within their parent project**, not globally. Two projects can both have phases named
`write-side` (and several in the dossier portfolio do). Use the project slug +
phase slug as the addressing tuple at the MCP boundary; the ULID is the
corpus-global identifier. Tooling that takes a bare `phase:<slug>` argument
must disambiguate across projects (or require a project hint). The same
observation applies to task slugs.

## Verbs (MCP tools)

Names are dot-segmented. All return the affected resource on success. All
writes are idempotent on `(actor, request_id)` if `request_id` is supplied.

### Project

- `project.create` — `{ slug, title, description }` → Project
- `project.get` — `{ id | slug }` → Project (with phases, tasks, artifacts inlined)
- `project.list` — `{ status?, body_contains?, created_after?, created_before?, updated_after?, updated_before?, order_by?, desc?, limit? }` → list of Project (without children)
- `project.update` — `{ id, title?, description?, status? }` → Project

### Phase

- `phase.add` — `{ project, title, body, after_phase? }` → Phase
- `phase.update` — `{ id, title?, body?, status? }` → Phase
- `phase.list` — `{ project?, status?, body_contains?, created_after?, created_before?, updated_after?, updated_before?, order_by?, desc?, limit? }` → list of Phase. `project` is optional — omit (or pass `null`) for a cross-corpus listing. Default `order_by` is `order` (linear position within a project).

### Task

- `task.create` — `{ project, phase?, title, body }` → Task (status=`todo`)
- `task.claim` — `{ id, actor }` → Task (status=`claimed`, assignee=actor). Fails if already claimed by another actor.
- `task.update` — `{ id, body?, status?, note? }` → Task. `note` appends to log.
- `task.complete` — `{ id, actor, note? }` → Task (status=`done`, completed_at=now). From `in_progress`, completes directly. From `todo` or `claimed` (same actor), performs claim → in_progress → done as one compound transition (assignee=`actor`, `claimed_at` and `completed_at` stamped). Cross-actor `claimed` rejects.
- `task.get` — `{ id }` → Task. Walks every project; no project slug required. Malformed id → `invalid_params("invalid id format")`; well-formed but absent → `invalid_params("task not found: ")` followed by the queried ULID.
- `task.list` — `{ project?, phase?, status?, assignee?, body_contains?, created_after?, created_before?, updated_after?, updated_before?, completed_after?, completed_before?, claimed_after?, claimed_before?, order_by?, desc?, limit? }` → list of Task. `project` is optional (omit / `null` = cross-corpus); `phase` is a slug and requires `project`. `status` is a list (OR-of-statuses). `body_contains` is a case-insensitive literal substring. Date params are RFC 3339; `_after` is inclusive, `_before` is exclusive. `order_by` on a nullable field (`completed_at`, `claimed_at`) drops rows where that field is null.

### Search

- `search` — `{ query, kinds?, project?, limit? }` → ranked list of hits across project / phase / task titles + spec bodies. `query` is a non-empty case-insensitive literal substring. `kinds` is a subset of `project` | `phase` | `task` (default: all). `project` restricts to one project slug (omit for corpus-wide). `limit` defaults to 50, applied after sort by `score` descending then `updated_at` descending. Each hit: `kind`, `id`, `project`, `slug`, `title`, `snippet` (~80 chars centered on first match), `score` (overlapping literal match count in title+body). Task hits may include `phase` (slug). The appended `## Notes` section on tasks is excluded from the index — only the spec body is searched. Empty `query` is rejected; no matches returns an empty list.

### Artifact

- `artifact.link` — `{ project, task?, kind, ref, label, meta?, actor }` → Artifact. Optional `meta` is a flat string map; cap violations return `invalid_params` naming the failing key. Re-link with the same `(task, kind, ref)` is idempotent when `meta` is byte-identical; differing `meta` is rejected (`"meta is immutable for an existing (task, kind, ref); supersede instead"`).
- `artifact.list` — `{ project?, task?, kind?, ref? }` → list of Artifact (includes `meta` when present). `ref` is an exact-match filter on the canonical ref (no substring/prefix matching) and AND-composes with `task`/`kind`; absent `ref` = no ref filtering.

## Task state machine

```
todo ──claim──▶ claimed ──update(in_progress)──▶ in_progress
  │                │                                  │
  │                ▼                                  ▼
  │             cancelled                          blocked ──update(in_progress)──▶ in_progress
  │                                                   │
  └──────── task.complete (compound) ────────────────┴── task.complete ──▶ done
```

- `cancelled` is terminal.
- `done` is terminal.
- Any non-terminal state may transition to `cancelled` via `task.update`.
- `blocked` may carry a note explaining the block.
- `task.complete` is the sole entry into `done`. It may walk intermediate states: from `todo`, claim (by `actor`) → `in_progress` → `done`; from `claimed` (same actor), `in_progress` → `done`. The underlying transitions are unchanged — the verb executes them atomically in one write.

## Identity & provenance

Every write includes `actor` (string). Every resource exposes the `actor`
that created it and the actor of each subsequent mutation. The protocol
does not authenticate actors — that is the server's concern. Implementers
SHOULD use stable identifiers (`ship`, `claude-code:michael`, `human:mh`)
so readers can attribute work.

## Versioning

This document is `v0` — pre-1.0. Tool names and field names may change
between revisions. When dossier picks up an external consumer that
needs version stability, this section grows; today it doesn't.

## Not in v0

What the v0 surface leaves out, and why. These are where the work sits
today — not a list of things dossier refuses to build. The core earns
the next capability when a real workflow keeps hitting the wall it
solves; see [docs/vision.md](docs/vision.md) for the "samurai = mastery
+ sequencing" framing.

Some jobs compose better *outside* dossier than inside it:

- **Semantic query / vector / RAG** — literal substring `search` ships;
  the embedding index and semantic retrieval belong to whatever LLM
  queries the corpus, not the store.
- **Decisions** as a first-class primitive — today a task or an
  `artifact` of kind `doc` carries them.

The rest are sequencing calls — not yet needed at solo scale, added when
the evidence shows up:

- **Conflict detection** (multi-claim, slug similarity, stale claims) —
  a multi-writer concern; a solo corpus doesn't hit it yet.
- **Cross-project links / dependencies** — projects stay independent
  until a workflow genuinely spans them.
- **Permissions / multi-tenant auth** — solo today.
- **Notifications / subscriptions / streaming** — poll for now.
- **Rich attachments** — artifacts point at external storage instead.
- **Time tracking, estimates, sprints** — workflow conventions on top of
  the primitives, not primitives themselves.
