**Status**: accepted (implemented)
**Owner**: @itsHabib
**Date**: 2026-05-16
**Related**: [horizon.md](../../horizon.md), [vision.md](../../vision.md), [PROTOCOL.md](../../../PROTOCOL.md), [LAYOUT.md](../../../LAYOUT.md)

# Filter expansion — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/domain.rs`, `src/store.rs`, `src/server.rs` | ~320 | 320 |
| Tests | `src/store.rs` test module, `src/server.rs` test module | ~250 | 125 |
| **Total** | | | **~445** |

Band: **amazing** (<500 weighted). Single PR. If implementation lands in stretch, natural split: PR A is `task.list` predicates + cross-project; PR B mirrors the shape onto `phase.list` / `project.list`.

## Goal

Extend the three list verbs (`task.list`, `phase.list`, `project.list`) with rich predicates so an LLM agent can answer natural-language questions about the portfolio in a single MCP call.

Today the verbs require a parent (project slug for `task.list` and `phase.list`) and accept only `status`. To answer *"what tasks did I close last week mentioning auth?"* the agent has to walk the project tree and filter client-side. Filter expansion moves the filtering server-side so the LLM composes one predicate-shaped call per question.

## Behavior

### `task.list`

Extended argument shape:

```
task.list {
  project?: string | null,                   # omit OR null = cross-corpus
  phase?: string,                            # requires project; else validation error
  status?: TaskStatus[],                     # OR-of-statuses
  assignee?: string,                         # exact match against frontmatter `assignee`
  body_contains?: string,                    # case-insensitive literal substring on task body
  created_after?: DateTime, created_before?: DateTime,
  updated_after?: DateTime, updated_before?: DateTime,
  completed_after?: DateTime, completed_before?: DateTime,
  claimed_after?: DateTime, claimed_before?: DateTime,
  order_by?: "created_at" | "updated_at" | "completed_at" | "claimed_at",
  desc?: boolean,                            # default false (ASC)
  limit?: number,
}
```

Required-project semantics relax: `project` becomes optional. Omitting (or passing `null`) means "scan every project in the corpus." Predicates AND-together; empty predicate set returns all tasks subject to the default sort.

### `phase.list`

```
phase.list {
  project?: string | null,
  status?: PhaseStatus[],
  body_contains?: string,
  created_after?, created_before?,
  updated_after?, updated_before?,
  order_by?: "created_at" | "updated_at" | "order",
  desc?: boolean,
  limit?: number,
}
```

Cross-project listing mirrors `task.list`. `order` references the existing `order` frontmatter field (linear position within a project).

### `project.list`

```
project.list {
  status?: ProjectStatus[],
  body_contains?: string,
  created_after?, created_before?,
  updated_after?, updated_before?,
  order_by?: "created_at" | "updated_at",
  desc?: boolean,
  limit?: number,
}
```

`project.list` is already corpus-scoped — no parent to nullify. Just adds the new predicates.

## Decisions

- **`body_contains` is case-insensitive literal substring.** Not tokenized, not regex. Predictable for the LLM; cheap to implement; easy to upgrade later if evidence shows it's insufficient. A tokenizer introduces fuzzy behavior the LLM has to model; literal substring is what `grep -i` does, a primitive every LLM already grounds in.
- **`status` is multi-valued** (`Vec<TaskStatus>`). *"What's open?"* naturally maps to `status: [claimed, in_progress]`; a single-value field forces multiple calls.
- **Date params are RFC 3339 strings** matching existing `created_at` serialization. Malformed strings produce a typed validation error rather than silent coercion.
- **Cross-project: omit `project` OR pass `null`.** Identical semantics. Both treated the same internally.
- **`phase` requires `project`.** Validation error if `phase` is set and `project` is null. Phase slugs are unique within a project, not across the corpus.
- **Default sort** is `created_at` ASC when `order_by` is unset. Deterministic for future pagination semantics; matches how on-disk ULIDs naturally sort.
- **`order_by` on a nullable field** (e.g. `completed_at` when many rows haven't completed) implicitly filters out rows where that field is null. Sorting by a field you don't have is almost certainly not what the caller wants; document this.
- **Return shape unchanged.** Same `TaskSummary` / `PhaseSummary` / `ProjectSummary` records as today, just more rows / more filtering.

## Non-goals

- No new verbs. `search`, `activity`, conflict / hygiene verbs are separate (horizon.md phases 2+).
- No pagination cursors. Just `limit` for v1; revisit if "limit without stable cursor" becomes a real LLM-composition problem.
- No aggregations (count, group-by, distinct).
- No multi-field text search. `body_contains` matches body only — title / slug / frontmatter are not searched. Add later only if evidence demands.
- No fuzzy / semantic / vector search. Strictly literal.
- No tool-description rewrites — that's a separate small PR (horizon Phase 3).

## Acceptance

The six queries from [horizon.md](../../horizon.md) acceptance table become single MCP calls and return the expected rows against a fixture corpus:

| NL query | Call |
|---|---|
| *"What's open in roxiq right now?"* | `task.list { project: "roxiq", status: ["claimed", "in_progress"] }` |
| *"What did I close last week?"* | `task.list { assignee: "human:michael", status: ["done"], completed_after: <7d ago> }` |
| *"Designed anything around auth before?"* | `phase.list { body_contains: "auth" }` |
| *"Latest design doc I touched?"* | `phase.list { order_by: "updated_at", desc: true, limit: 1 }` |
| *"Which projects are paused?"* | `project.list { status: ["paused"] }` |
| *"What's in flight across the portfolio?"* | `task.list { status: ["claimed", "in_progress"] }` |

## Test plan

- **Per-predicate unit tests** on `FsStore::list_tasks` / `list_phases` / `list_projects`: one test each for `assignee`, `body_contains`, each date range field, `order_by` × `desc`, `limit`.
- **Combo tests**: assignee + status + date range; cross-project + body_contains; sort + limit interaction.
- **Validation tests**: malformed dates rejected; `phase` without `project` rejected; `order_by` accepts the documented values only.
- **Dogfood corpus round-trip**: extend `read_dogfood_corpus` (or add a sibling) that exercises all six acceptance queries against the in-repo `projects/dossier/` fixture and asserts expected row counts / IDs.

## Implementation sketch

- `domain.rs`: new types `TaskListFilter`, `PhaseListFilter`, `ProjectListFilter` carrying the predicate set. `OrderField` enums per primitive.
- `store.rs`: extend `list_tasks` / `list_phases` / `list_projects` to accept the filter; the existing single-status path becomes the `status = Some(vec![s])` case of the new shape.
- `server.rs`: update the `#[tool]` arg structs to mirror the filter; pass through to the store. Update `#[tool]` doc strings to surface the new predicates (since the doc is the LLM's onboarding).
- Filter application is a straight `Iterator::filter` chain over the parsed frontmatter. No new file format, no new on-disk artifacts.

## Open questions

- **`body_contains` case sensitivity** — **resolved: case-insensitive**. Implementation lowercases both sides; predictable for an LLM and matches the `grep -i` mental model.
- **`body_contains` and code blocks** — **resolved: yes, matches inside fenced code blocks**. Strict literal substring; the user can be more specific if they want to exclude code. Tokenization stays out of scope until evidence demands.
- **Filter combinator** — **resolved: AND only**. 90% of LLM-generated queries are conjunctive, and OR over `status` is already covered by the multi-value list. Revisit if evidence demands OR / NOT.
- **`order_by` on nullable fields** — **resolved: implicitly filter out rows where the sort key is null**. Sorting by a field you don't have is almost certainly not what the caller wants; documented in the tool description so the LLM is on the same page.
- **Cross-project performance** — open. At hundreds of projects × thousands of tasks, is a naive walk fast enough? Probably yes at v0 corpus sizes; revisit when measurement disagrees.
