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
| `kind`        | `commit` \| `pr` \| `file` \| `url` \| `run` \| `doc` \| `verdict` \| `receipt` | extensible |
| `ref`         | string                                                        | sha / url / path / run id           |
| `label`       | string                                                        | short human-readable                |
| `linked_at`   | timestamp                                                     |                                     |
| `actor`       | string                                                        | who linked the row                  |
| `meta`        | map string → string, optional                                 | flat denormalized summary; omitted when empty. Caps: ≤16 keys, key ≤64 bytes, value ≤512 bytes, ≤4 KiB total serialized. Unknown keys/values round-trip untouched. Immutable for an existing `(task, kind, ref)` — correction is via supersede (distinct `ref` + `meta.supersedes`), not mutation. |

`kind: verdict` and `kind: receipt` are well-known kinds for the State
substrate: a **pointer + denormalized summary** for a gate authorization
decision (`verdict`) or a merge / close-out event (`receipt`). The
authoritative record (gate's hash-chained decision, the full PR, ship's
run) stays in its home store, reachable via `ref` — dossier stores the
summary that makes retrospective reads ("why did this PR merge?") answerable
from `artifact.list` alone, not a re-implementation of gate's or GitHub's
data. Like every other kind, these are **conventions, not schema**: dossier
does not validate `outcome` vocabularies or any other meta value, and an
unrecognized kind or meta key still round-trips untouched (FR3).

### Canonical `ref` form per kind

The `ref` filter on `artifact.list` (when present) is exact-match, so the
form is pinned per well-known kind rather than left to spell itself three
ways:

| kind      | canonical `ref` form                                                | notes |
|-----------|-----------------------------------------------------------------------|-------|
| `receipt` | the canonical GitHub PR URL: `https://github.com/<owner>/<repo>/pull/<n>` | no trailing slash, no `.git`, lowercase host |
| `verdict` | the gate audit ref, e.g. `gate://<repo>/pr/<n>/<gate_run_id>`         | gate's opaque per-evaluation id — a `run_…` id today (not `dec_…`); `<repo>` is the short repo name gate emits (`dossier`), stable per decision |

**Starting from just a PR number.** You will usually hold only "PR #N", not
the full canonical URL, the `owner/repo` slug, or the task id. The exact-`ref`
lookup needs the full URL and the task-anchor join needs the task id, so
neither is the entry point. The format-independent entry is:
`artifact.list { project, kind: "receipt" }`, apply the supersede reader rule
**first** — drop any row named by a later row's `meta.supersedes`, across the
whole set, since a supersede may correct `meta.pr` itself and filtering by `pr`
first would never see the replacement — **then** keep the surviving row whose
`meta.pr == "N"`. From that receipt,
`meta.verdict` (an `art_` id) names the authorizing verdict. There is no by-id
fetch (`artifact.list` filters only `project`/`task`/`kind`/`ref`), so resolve
that pointer by listing verdicts for the same anchor —
`artifact.list { project, task, kind: "verdict" }`, or project-wide when the row
has no task — and matching the **exact `art_` id**. Match the id, *not* `meta.pr`:
a PR with several gate evaluations yields several verdicts sharing one `meta.pr`
(e.g. an earlier `blocked` and a later `pass`), and only the receipt's
`meta.verdict` names the one that authorized the merge. If `meta.verdict` is
missing or dangles (the FK is unenforced), fall back to the task anchor +
`meta.pr` under the supersede reader rule; when that still leaves several live
`pass` verdicts for the head, the authorizer is **unresolvable from the
substrate alone** — recover it from the authoritative gate record via the
verdict `ref`.

### `meta` key conventions per kind

Conventions, not schema — unknown keys pass through untouched, and dossier
never validates an `outcome` (or any other) vocabulary; that belongs to the
emitter (gate's `pass` / `blocked` / `parked` / `refused`, a driver
judgment, …). `actor` on the artifact records *who linked the row* (the
close-out caller); `meta.source` records *who decided* — the two are kept
distinct so a skill-driven close-out and the gate that produced the verdict
are both attributable.

| kind      | `meta` keys                                                                 |
|-----------|-------------------------------------------------------------------------------|
| `verdict` | `source` (`gate` \| `review-coordinator` \| …), `outcome` (emitter's vocabulary, e.g. gate's `pass` \| `blocked` \| `parked` \| `refused`), `pr`, `head_sha`, `grant` (`grt_` id, when one applied), `tier` (emitter's vocabulary — gate emits `T0`–`T3`, *not* a bare `0`–`3`) |
| `receipt` | `event` (`merge` \| `close-out` \| …), `pr`, `merge_sha`, `verdict` (the `art_` id of the authorizing verdict), `supersedes` (`art_` id, when this row corrects an earlier immutable one) |
| `run`     | `engine`, `run` (ship run id), `judgment` — existing kind, meta convention enriched here |

Example rows (append-only `artifacts.jsonl`, one line each):

```jsonl
{"id":"art_01K…","project":"prj_01KRSZ…","task":"tsk_01K…","kind":"verdict","ref":"gate://dossier/pr/93/run_9ce4b19af24974c5","label":"gate pass PR #93","linked_at":"2026-07-23T18:00:00Z","actor":"claude-code:michael","meta":{"source":"gate","outcome":"pass","pr":"93","head_sha":"872b472","grant":"grt_01K…","tier":"T1"}}
{"id":"art_01K…","project":"prj_01KRSZ…","task":"tsk_01K…","kind":"receipt","ref":"https://github.com/itsHabib/dossier/pull/93","label":"merged PR #93","linked_at":"2026-07-23T18:05:00Z","actor":"claude-code:michael","meta":{"event":"merge","pr":"93","merge_sha":"a1b2c3d","verdict":"art_01K…"}}
```

### Supersede convention

`artifacts.jsonl` is append-only with no update path, and `artifact.link`
rejects a re-link that changes `meta` for an existing `(task, kind, ref)`
(`"meta is immutable for an existing (task, kind, ref); supersede instead"`).
Verdicts and receipts are immutable facts — a wrong `meta` (e.g. a
`meta.verdict` pointing at the wrong `art_` id) is corrected by
**superseding**, never by mutation:

1. Append a *new* artifact with a **distinct `ref`** (a fresh gate audit ref
   for a corrected verdict, or the PR URL with a `#v2` fragment for a
   re-recorded receipt) and `meta.supersedes: <art_id of the row being
   corrected>`.
2. **Reader rule:** among artifacts of a given `(kind, logical target)`,
   ignore any row named by a later row's `meta.supersedes`; the row that is
   never named as superseded is the current fact.

This gives a deterministic "current" fact without an update path and
without a fragile "latest `linked_at`" heuristic.

**Lookup under supersession.** The exact-match `ref` lookup on the
canonical (unfragmented) PR URL / gate audit ref returns the *original*
row — which, once a correction lands, is the **superseded** one, since the
replacement carries a distinct `ref`. That exact-ref lookup is the fast
path for the common, un-superseded case; it cannot by itself surface a
later supersede. To get the **current** fact when a supersede may have
occurred, don't rely on it — use the format-independent task-anchor join
(the §7.2 fallback): `artifact.list { project, task, kind: "receipt" }`
(or `verdict`), then apply the reader rule above — drop any row named by a
later row's `meta.supersedes` and take the survivor, disambiguating by
`meta.pr` when needed. This is exactly why `meta.supersedes` and the
task-anchor join exist: exact-`ref` is the fast path, the task + `meta.pr`
join is the correct path under supersession.

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
