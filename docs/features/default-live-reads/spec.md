**Status**: proposal — design for review; not a build commitment.
**Owner**: @itsHabib
**Date**: 2026-06-19
**Related**: [vision.md](../../vision.md), [filter-expansion/spec.md](../filter-expansion/spec.md), [search/spec.md](../search/spec.md), the orientation sibling [project-overview/spec.md](../project-overview/spec.md) (PR #85), PROTOCOL.md. Implements **option 1** of the dossier task `deletes-not-just-cancel`.

# Default-live reads — drop terminal clutter from the default list/search surface

## Problem

Reported by multiple people using dossier: as a corpus matures, `task.list` / `phase.list` / `project.list` return a growing pile of **terminal** items — `done` / `cancelled` tasks, `done` / `skipped` phases, `done` / `abandoned` projects. The default "give me the tasks" read drowns the live work in finished history. dossier has no delete by design (`cancelled` is a terminal state; git keeps the record), so nothing prunes the default surface — it only grows. The same friction was re-reported re-homing the dossier-cloud work, and is captured as `deletes-not-just-cancel`.

This is the **enumeration** half of the same scaling problem `project.overview` (PR #85) solves for **orientation**. Measured: ~90% of tasks on mature projects (ship, dossier itself) are terminal. `project.overview` fixes the orient read by *aggregating* (counts, not rows); this fixes the list reads by *defaulting to live rows*.

## Diagnosis

The list verbs default to "all statuses" when no `status` filter is given. On a young project that's fine; on a mature one every `task.list { project }` re-surfaces dozens of done/cancelled rows nobody asked about. The fix isn't delete — that's the heavier, separate discussion (`deletes-not-just-cancel` options 2/3: soft-delete status, hard `*.delete` verbs). It's making the *default* surface the live work, with terminal one keystroke away. Zero data-semantics change, no new verbs, no on-disk change.

## Recommendation: default to live, opt in to terminal

When a list verb is called **without an explicit `status` filter**, default to **non-terminal** statuses instead of all. An explicit `status` is always honored exactly (ask for `done`, get `done`). A new `include_terminal: bool` restores the all-statuses default when `status` is omitted.

**Terminal sets** — a pure `is_terminal()` predicate per status enum, in `domain.rs` (1:1 with the state machine, which already treats `Done`/`Cancelled` as terminal task sources):

| Primitive | Terminal (hidden by default) | Live (shown by default) |
|---|---|---|
| Task | `done`, `cancelled` | `todo`, `claimed`, `in_progress`, `blocked` |
| Phase | `done`, `skipped` | `pending`, `active` |
| Project | `done`, `abandoned` | `planning`, `active`, `paused` |

`blocked` is **live** (stuck ≠ finished — you want to see what's blocked). A `paused` project is **live** (it's still yours; you'd want it in a default portfolio list).

**Resolution rule** (per list verb):

| `status` | `include_terminal` | result |
|---|---|---|
| explicit | (ignored) | exactly the requested statuses — **unchanged** |
| omitted | `false` (default) | live statuses only — **new default** |
| omitted | `true` | all statuses — today's behavior, opt-in |

### `search` keeps finding terminal work (D4)

`search` answers a *different* question — "where does X appear, anywhere?" — and finding **completed** work ("did I already build this?", "have I designed auth before?") is a core, deliberate use of it. Default-hiding terminal would make search worse at its primary job. So **search keeps returning terminal hits by default**, but gains the *same* `include_terminal` knob (default **true** for search) so a caller can scope to live-only when they want.

The control is uniform across the read surface; the **default differs by verb because the verbs answer different questions** — lists answer "what's the current work" (live-by-default), search answers "where does this appear" (everything-by-default). That asymmetry is the design, not an inconsistency.

## Why this shape

- **Flip the default, don't add an opt-in-hide.** The clutter *is* the default read; making people opt *in* to a clean view is backwards. The most-reported symptom is "the default is noisy," so the default is what changes. (This is exactly option 1's framing in `deletes-not-just-cancel`, the operator's recommended-first move.)
- **Not delete.** Hard removal is the heavier half of the friction (new `*.delete` verbs, terminal-only guardrails, block-if-non-empty policy, cloud parity). Default-live kills the *symptom* (clutter) with no new verbs and no data-semantics change. Ship it first; it likely resolves most of the friction, and the residual ("I still need real removal") is the evidence that graduates hard-delete — decided from use, not speculation.
- **Reuses the existing predicate surface.** The list verbs already take a multi-valued `status` (filter-expansion). "Default to live" is just "inject the live statuses when none were asked for" — a few lines over the existing matcher, no new filter machinery.

## Interaction with `project.overview` (PR #85)

Complementary, not overlapping — together they complete "reads work well at scale":

- **`project.overview` aggregates ALL** (counts include terminal — a *complete* picture of where finished vs open work sits). Unaffected by this change; counts stay exhaustive.
- **The list verbs enumerate LIVE by default** (rows default to non-terminal). Aggregate-everything for the count view; default-live for the row view — consistent.
- **Composes with `bodies:false`** (#85 Move 2): `task.list { project }` is now live-only by default *and* `bodies:false` strips bodies → the bounded, live drill-down the overview steers agents toward.
- **Likely retires phase-archival** (deferred from #85): if `done`/`skipped` phases drop from the default `phase.list`, you get archival's main benefit — done phases off the active surface — with **no new status, no migration, no format change**. The cheaper path to the same outcome; revisit a true `archived` lifecycle only if default-live proves insufficient.

## Backward compatibility

- **Changes the no-status default of the three list verbs** (all → live). Opt-out is one field: `include_terminal: true`, or an explicit `status` (incl. terminal values). dossier is pre-1.0 (PROTOCOL.md §Versioning); the corpus is unchanged on disk; this is the intended fix, not an accident.
- **Tool descriptions must state the new default loudly** — it's behavioral for LLM agents (they need to know `task.list` is live-by-default and how to see terminal). Same "description is behavioral for agents" point as #85's `project.get` steering.
- **`project.get` / `project.overview` unchanged** — `get` is the full hydrate; `overview` counts all statuses.
- **Dogfood tests change.** `read_dogfood_corpus` and any test asserting raw `list_*` counts must update (e.g. `task.list { project: "dossier" }` returns 6 live, not 58). That's the visible proof the change works; the assertions move with it.

## Layering & where the code lives

Respects `domain → store → server → bin`:

- **`domain.rs`** — `is_terminal()` on `TaskStatus` / `PhaseStatus` / `ProjectStatus`; a small `live_statuses()` helper per enum. Pure policy, 1:1 with the state machine.
- **`store.rs`** — in `list_tasks` / `list_phases` / `list_projects`, when the filter's `status` is `None` and `include_terminal` is false, restrict to the live set before the existing match/sort/limit chain; mirror in `search`.
- **`server.rs`** — add `include_terminal` to the four read verbs' arg structs (+ the filter structs) and update the tool descriptions.

No new dependency direction; no on-disk format change.

## Scope & rollout

Single PR, "amazing" band.

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/domain.rs` (predicates), `src/store.rs` (default injection in 3 lists + search), `src/server.rs` (`include_terminal` arg + descriptions) | ~100 | ~100 |
| Tests | `src/store.rs` test module + dogfood count updates | ~120 | ~60 |
| **Total** | | | **~160** |

Sequencing: touches the same list-verb arg/filter structs as #85's `bodies:false`. Land in either order; the second rebases. The two are the "reads at scale" pair — ship close together.

## Decisions to lock

- **D1 — flip the default to live; no opt-in-hide.** The clutter is the default; the default is what changes.
- **D2 — explicit `status` always wins.** `include_terminal` only governs the omitted-status default. (An explicit `status: ["done"]` returns done regardless of `include_terminal`.)
- **D3 — terminal sets per the table.** `blocked` and `paused` are live, not terminal.
- **D4 — search defaults to all (opt-in live-only); lists default to live (opt-in terminal).** Uniform `include_terminal` knob, verb-appropriate defaults.
- **D5 — no delete in this PR.** This is `deletes-not-just-cancel` option 1; soft-delete (option 2) and hard `*.delete` verbs (option 3) stay parked, graduated only on residual evidence after this lands.

## Non-goals

- No delete verbs, no soft-delete `deleted` status, no `deleted_at` (the separate `deletes-not-just-cancel` discussion).
- No bulk "prune all cancelled" (a bulk op; revisit only if the pain is volume, not individual items).
- No change to `project.get` or `project.overview`.
- No new on-disk format, no migration.
- No cross-corpus default change beyond the per-verb rule above.

## Test plan

- **Per-verb default-live**: `task.list`/`phase.list`/`project.list` with no `status` return only live rows; assert terminal rows are absent.
- **`include_terminal: true`** restores all statuses for each verb.
- **Explicit `status` honored**: `status: ["done"]` returns terminal rows even with `include_terminal` false (D2).
- **`is_terminal()` units**: every enum variant maps to the table; `blocked`/`paused` are live.
- **search**: terminal hits present by default; `include_terminal: false` scopes to live-only.
- **Dogfood**: update `read_dogfood_corpus` to the live-default counts; add an assertion that `include_terminal: true` recovers the full counts (proves opt-out works against the real corpus).
- **`make check` green** on the repo matrix.

## Acceptance

- `task.list { project: "ship" }` returns only live tasks (~12 todo, not 106); `task.list { project: "ship", include_terminal: true }` or `{ status: ["done"] }` returns the terminal rows.
- `phase.list { project: "ship" }` drops `done`/`skipped` phases by default.
- `search { query: "..." }` still surfaces hits in done/cancelled items (finding past work is unbroken); `include_terminal: false` scopes it to live.
- No on-disk migration; no change to `project.get` / `project.overview`.
