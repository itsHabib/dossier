**Status**: proposal — design for review; NOT a build commitment. The artifact we decide *from*.
**Owner**: @itsHabib
**Date**: 2026-06-19
**Related**: [vision.md](../../vision.md), [filter-expansion/spec.md](../filter-expansion/spec.md), [search/spec.md](../search/spec.md), [cloud-backend/spec.md](../cloud-backend/spec.md) (§6 Store seam, D5 warm cache, D9 search-not-in-trait), [PROTOCOL.md](../../../PROTOCOL.md), [LAYOUT.md](../../../LAYOUT.md). Dogfood friction: dossier task `large-project-read-scaling`.

# Project overview — a projected, bounded "state of this project" read

## Problem

`project.get` is the natural call an agent makes to orient in a project — the documented first step of *"what's the state of `<project>`?"* It has stopped working at scale.

In a real ship-on-ship driver session (2026-06-19), `project.get { slug: "ship" }` (the MCP client emits it as `project_get`) returned ~395,675 characters — 393,675 corpus bytes plus the JSON envelope (see Evidence; the doc reports bytes throughout) — overran the MCP tool-result size cap, and could not be consumed inline at all — the harness dumped it to a file and the agent had to `jq` it just to read the phase slugs. The single most natural orientation read is now unusable on a project that has been used for months.

Root cause: `project.get` always hydrates the **entire subtree in one unbounded blob** — project meta + every phase (full body) + every task (full body + notes) + every artifact. Fine when a project is young; the cost grows monotonically as a dogfooded project accumulates phases and tasks. `done` and `cancelled` work never leaves the read. This is the natural consequence of dossier *succeeding*: the more a project is used, the heavier its overview read becomes.

### Evidence

Measured against the live corpus (`~/pers/dossier-state/`):

| Project | Phases | Tasks | Artifacts | `project.get` bytes | Task-body share | Terminal tasks |
|---|---|---|---|---|---|---|
| **ship** (friction case) | 25 | 106 | 140 | **393,675** | 312,314 (**79.3%**) | 94/106 (**89%**) |
| **dossier** (itself) | 23 | 58 | 104 | **172,485** | 119,419 (**69%**) | 52/58 (**90%**) |

Where ship's 393,675 bytes live: `project.md` 636 B (0.2%), phases 38,160 B (9.7%), **tasks 312,314 B (79.3%)**, artifacts 42,565 B (10.8%).

Three facts fall out of the data and drive the whole design:

1. **Tasks dominate.** ~80% of the payload on ship, ~70% on dossier. Phase bodies are a distant second. Any fix that doesn't address task volume doesn't move the needle.
2. **The payload is mostly history.** ~90% of tasks on *both* projects are terminal (`done`/`cancelled`). The read is dominated by work that is finished, not work you're orienting around.
3. **An index is a fraction of the body weight.** Task frontmatter alone is **13.5%** of full task bytes; phase frontmatter is **20.9%**. Replacing bodies with *counts* (not even rows) collapses the read to a few KB regardless of how much terminal work has accumulated.

## Diagnosis: an output-volume problem, not a lookup problem

Confirmed. The walk that assembles `project.get` reads ~131 small files (ship: 25 + 106) from local disk — single-digit-to-low-tens of milliseconds. Nothing about *finding* the data is slow. What blows the token cap is **serializing every body into one result**. The bottleneck is the size of what we return, not the cost of producing it — on the **local `FsStore` backend** (the live one).

One nuance that matters later: "bounded output" is not the same as "bounded work." A counts read returns a few KB no matter the task count, but it still *loads* every task to count it. On `FsStore` that load is free (local reads, low-ms — measured). On the remote `S3Store` backend it is not — see §Why the service layer and §Deferred. We design for the live backend now and name the remote cost honestly.

## What a good orientation read actually needs

When an agent asks *"what's the state of ship?"* it needs:

- Project identity + status (is this active? paused?).
- The **shape**: which phases exist, in what order, what status each is in, who owns it, when it was last touched.
- A sense of **where the work is**: how many tasks per phase, broken down by status — so it can see "phase 18 has 6 todo, everything else is done" and know exactly where to drill down. (The counts point at the *phase*; the actual task rows come from a scoped `task.list { phase, status, bodies:false }` — overview carries counts, not task ids/titles.)
- **Not** every phase's design-doc body. Not 94 terminal task bodies. You read *one* body once you've decided which one matters — and the drill-down reads (`phase.list`, `task.get`, `task.list { status }`) serve that, *provided they're bounded too* (they aren't today — see Move 2).

So the orientation read is an **aggregation** (counts), not an **enumeration** (rows-with-bodies). That single observation selects the design below.

## Options weighed

The friction task listed five. Verdicts, with the evidence above:

| # | Option | Verdict | Why |
|---|---|---|---|
| 1 | **Projection flags on the reads** (`bodies:false` etc.) | **Adopt as a companion, not as the orientation fix** | A `bodies:false` `project.get` (~50 KB on ship) genuinely *would* clear the failing constraint today, and the same field-strip is reusable on `phase.list` / `task.list` — three verbs, one mechanism. Real strengths, conceded. But for *orientation* it stays **O(all tasks)** — it enumerates 106 rows (89% terminal) and silently re-bloats as the project succeeds. So we adopt `bodies:false` for the drill-down reads (Move 2) and use a dedicated aggregation for orientation (Move 1). |
| 2 | **Default `project.get` to omit bodies, opt-in to hydrate** | **Rejected** | Same O(all-tasks) enumeration ceiling, plus it changes the shape of an existing read for every caller including programmatic full-hydrate ones. The additive Move 1 + the opt-in `bodies:false` of Option 1 cover the space without mutating `project.get`'s default. |
| 3 | **Dedicated `project.overview` verb** (meta + phase index + counts) | **RECOMMENDED (load-bearing)** | Aggregates instead of enumerates → bounded by *phase* count, not *task* count → a few KB on ship and on a project 10× its size. Additive; idiomatic to dossier's small-sharp-verb surface; mirrors the established `search` service-layer pattern. Detail below. |
| 4 | **Pagination on the arrays** | **Deferred** | Once orientation is aggregated (counts, not rows), it's already bounded — pagination buys nothing for it. It's a tool for the *hydrating* lists at much larger scale; `limit` already exists, cursors are the unbuilt part. Same evidence-gate filter-expansion already set for cursors. |
| 5 | **Phase lifecycle / `archived` status** | **Deferred (the more durable fix, but heavier)** | Honest steelman: archival is the *only* option that shrinks the corpus at the **source**, so it would thin every read — `project.get`, `phase.list`, `task.list`, and `search` ranking — not just orientation. The 90%-terminal data is the best argument *for* it. It still loses **now** because it touches the on-disk format, needs a migration, and changes default-filter semantics across every read verb — a real cost the additive moves avoid. Frame: durable-but-heavy vs reversible-and-cheap. Overview unblocks the friction today and leaves archival as a later, evidence-gated corpus-hygiene play. |

## Recommendation

A coherent orientation surface, not a single patch. Three moves; the first is load-bearing, the second and third make it *land*.

### Move 1 — `project.overview` (load-bearing)

A dedicated read returning project meta + a (bounded) description + an ordered phase index, where each phase carries **task-status counts instead of task bodies**, plus project-level rollups. No phase bodies, no task bodies, no notes — ever.

```
project.overview { slug } →
{
  project: {
    id, slug, title, status,
    created_at, updated_at, created_by,
    description,                   // first 600 chars of project.md body; see D2
    description_truncated: bool     // true if the body was clipped
  },
  phases: [                        // ordered by `order` ASC, ties by id ASC (= phase.list single-project default)
    {
      id, slug, title, order, status, owner, updated_at,
      task_counts: { todo, claimed, in_progress, blocked, done, cancelled, total }
    }
  ],
  unphased: {                      // tasks whose `phase` is empty OR points at no existing phase (D6)
    task_counts: { todo, claimed, in_progress, blocked, done, cancelled, total }
  },
  totals: {
    phases_by_status: { pending, active, done, skipped },
    tasks_by_status:  { todo, claimed, in_progress, blocked, done, cancelled, total },
    artifact_count                 // project-level only — see D3
  }
}
```

On ship this is ~25 phase rows + a handful of count maps — **a few KB, down from 394 KB** — and it stays a few KB no matter how many terminal tasks pile up.

**Counts contract** (the LLM acts on these, so they're a contract, not an implementation detail):

- **Every status key is always present**, zero when none — no missing-key-vs-zero ambiguity. Task counts use `todo|claimed|in_progress|blocked|done|cancelled`; phase counts use `pending|active|done|skipped`. The two enum sets are named in the tool description so an agent doesn't conflate them.
- **`total` is always present and authoritative** (= sum of the status buckets). An agent trusts it; it never re-derives.
- **Full per-phase status granularity is deliberate, not noise.** Carrying `claimed`/`in_progress`/`blocked` per phase (not just open/done) lets an agent see *which* phase has claimable work and decide where to drill down — the counts route it to the right phase, then a scoped `task.list { phase, status, bodies:false }` returns the actual task rows (overview carries counts, not task ids/titles). The cost is integers; the payload stays a few KB.
- **Partition is exhaustive and reconciles.** Invariant: `Σ phases[].task_counts.total + unphased.task_counts.total == totals.tasks_by_status.total`. A task counts toward a phase row iff its `phase` id equals that phase's id; toward `unphased` iff `phase` is empty *or* dangling (D6). A test injects an orphaned-phase-id task to prove the partition leaves nothing uncounted.

#### Why a new verb, not a `bodies:false` flag on `project.get`

The flag has genuine merit (it'd unblock the friction at ~50 KB, and it's reusable — which is why we *do* adopt it for the drill-down reads in Move 2). The orientation read still wants its own verb, for one durable reason and one secondary one:

- **Durable (decisive):** the overview is **bounded by phase count forever** — a few KB regardless of how much terminal work accumulates. *Every* flag variant stays **O(all tasks)**: ~50 KB on ship today, growing without limit as the project succeeds (94/106 tasks already terminal). The read that orientation depends on must not re-bloat on the very thing that makes a project worth orienting around. Counts don't; stripped rows do.
- **Precedent:** aggregating a corpus query in the service layer, off the `Store` trait, is the established pattern — `search` already does exactly this (TDD **D9**: "search is app-query, not storage"). `project.overview` is to `project.get` what `search` is to the list verbs.
- **Secondary (contract cleanliness):** counts are a different *return shape* from `project.get`'s flat `{ phases:[Phase], tasks:[Task], ... }` arrays, so they can't be a subset of it anyway — a verb gives them a clean schema and a targeted description instead of overloading `project.get` with a second response mode.

#### Why the service layer, not a new `Store` trait method

`project.overview` is computed in `MeshService` by calling the existing store reads (`list_phases`, `list_tasks`, `list_artifacts`) and **counting** instead of returning bodies. No new method lands on the `Store` trait, so no new obligation falls on `FsStore` or `S3Store`. This is the right call **for the live `FsStore` backend**: the walk is unchanged (it was never the problem on local disk), only the output shrinks.

Scope the cost honestly across backends:

- **On `FsStore`:** loading full task structs to count them is trivial — local reads, low-ms, measured.
- **On `S3Store` (which already exists in-tree, behind `DOSSIER_BACKEND=s3`):** it is **not** trivial. `list_tasks` there is an O(tasks) object-GET fan-out — it downloads every task body (in fact twice today: once to load, once to re-read for the ETag version it discards), purely to keep the status field. Overview-over-`list_tasks` on ship would pull ~200 object bodies over the network to throw away every byte. That is the same "download everything to discard it" pattern this feature exists to kill, relocated from output tokens to S3 GET fan-out + egress.

This is acceptable now because `S3Store` is not the live backend — but it is the **named precondition** for the manifest in §Deferred, which must ship in lockstep with `S3Store` going live, not "someday." The `search`/D9 analogy holds in *where* the operation lives (service layer); it does not yet inherit D9's *cost model*, which assumes the warm cache (D5) makes the scan cheap. Overview at v0 scans live; the cache/manifest that closes that gap is the deferred item, gated on S3.

### Move 2 — make the drill-down reads bounded too (`bodies:false` on `phase.list` / `task.list`)

Move 1's tool description (and the steering in Move 3) routes agents *off* `project.get` and *toward* `phase.list` / `task.get` / `task.list` to read specific parts. Those reads must not be foot-guns themselves — and today `phase.list` returns every phase **body** (38 KB on ship), `task.list` returns every task **body + notes**. Routing an agent into an unbounded read is just relocating the friction.

Add an opt-in `bodies: false` (default `true` = unchanged) to `phase.list` and `task.list` that drops the `body` (and, for tasks, `notes`) field at serialization time. This is a clean field-strip — a true subset of the existing shape, no aggregation, no union, no schema change beyond the field already being `skip_serializing_if`-eligible. It is the one mechanism that makes *every* enumeration bounded, and it closes the secondary hole the friction task named directly. `task.get` stays full-body (it's already a single, intentionally-hydrated row).

This makes the recommended orient → drill-down path bounded end to end — *when an agent follows the steering and passes `bodies:false`*; the default stays full-body so existing callers are unaffected (the bound is opt-in, not automatic — which is why Move 3's steering matters). It also subsumes the friction's option 1 cleanly: overview for *aggregation/orientation*, `bodies:false` for *bounded enumeration*. Complementary, not redundant.

### Move 3 — steer agents to the new surface (the activation)

The friction is a *habit*: an agent reaches for `project.get` because that's what every onboarding surface tells it to. The fix only lands if the surfaces change. Three of them:

- **The `project.overview` tool description** (verbatim, §Tool descriptions) — the single most load-bearing artifact, since it's what an agent reads at call time. It leads with the trigger phrase agents match on.
- **The dossier MCP server `instructions` / `get_info`** — the orientation recipe surfaced to *every consuming Claude Code / Desktop session* (the vision's actual public-adoption surface). Today the server carries no orientation guidance; add an overview-first recipe.
- **`project.get`'s own description + the dossier-repo `CLAUDE.md` recipe** — both currently name `project.get` as step one of "what's the state of `<project>`?". Demote it to "when you truly need every body"; name `project.overview` as the orient call. The `CLAUDE.md` / instructions change is an **acceptance criterion**, not an aside — without it the trained habit persists.

Description steering is best-effort, not a hard guarantee. That is the honest argument *for* a deterministic backstop on `project.get` — see the deferred guardrail (D4) for why it's deferred rather than shipped in PR 1.

### Tool descriptions (verbatim)

`project.overview`:
> Orient in a project — the bounded "what's the state of `<project>`?" read, and the one to call FIRST. Returns project meta + a (truncated) description + an ordered phase index where each phase carries task-status COUNTS (todo|claimed|in_progress|blocked|done|cancelled, plus total) instead of bodies, an `unphased` bucket for tasks not anchored to a live phase (empty *or* dangling phase id), and project-level rollups (phase + task counts, artifact count). Stays a few KB no matter how much work has accumulated. To read a specific design doc or task body, follow up with phase.list / task.get / task.list. project.get is the full unbounded hydrate — heavy on mature projects.

`project.get` (revised):
> Full hydrate: one project with every phase, task, and artifact body inline. Heavy on mature projects — can exceed the result size cap. To orient, call project.overview; to read a specific part, use phase.list / task.get / task.list.

`project.list` (append): `… (corpus-wide; for one project's state, use project.overview).`
`phase.list` (append): `… Pass bodies:false to omit phase bodies (slug/title/status/order only).`
`task.list` (append): `… Pass bodies:false to omit task bodies + notes (frontmatter only) — use it when drilling down from project.overview.`

## Backward compatibility

dossier is public and tagged, and pre-1.0 (PROTOCOL.md §Versioning: tool/field names MAY change between revisions; no external consumer needs version stability today). So compat is about not silently breaking *our own* callers, not a semver contract:

- **`project.overview` is purely additive** at the protocol level. The only real costs: (a) a slightly larger `tools/list` schema bundle every client downloads at handshake — acceptable; the verb earns its description budget by routing the common orient call; and (b) the standard rmcp registration requirements (return `Json<ProjectOverview>`, derive `JsonSchema` on the new types, keep schemars aligned per `CLAUDE.md`). Covered by `make check`.
- **`bodies:false` on `phase.list` / `task.list` is opt-in**, default `true` = today's behavior. No existing caller changes.
- **`project.get` is unchanged in shape and default behavior.** Only its description changes — non-breaking for programmatic callers, but note a description change *is* behavioral for LLM agents (they read it to pick a verb): steering them off `project.get` is the deliberate intent of Move 3, not a side effect. The deterministic over-size guardrail is *deferred* (D4) precisely because, as originally specced, it would have been a behavior change for callers that aren't failing — see D4.
- **On-disk format is untouched.** No new frontmatter, no new files, no migration. Everything is derived at read time. (Contrast option 5, which *would* touch the format.)
- **Counts are a best-effort snapshot, not a transactional read.** Overview issues separate `list_phases` / `list_tasks` calls under no lock (as `project.get` does today). At single-writer v0 this is exact. Under the future multi-writer / bounded-stale cache backend (TDD D7) counts are exact-per-object but not point-in-time consistent — fine for orientation; stated so the "exact aggregation" claim isn't read as transactional.

## Layering & where the code lives

Respects `domain → store → server → bin`:

- **`domain.rs`** — new serializable types: `ProjectOverview`, `PhaseOverview`, `TaskStatusCounts`, `PhaseStatusCounts`. Plain data, no I/O.
- **`server.rs`** — the `project.overview` handler (aggregation policy over existing store reads); the `bodies` arg on the `phase.list` / `task.list` handlers; the description / instructions updates.
- **`store.rs`** — no `Store` trait change. The `bodies:false` strip happens at the server/serialization boundary (the body fields are already `skip_serializing_if`-eligible); at most a small private aggregation helper if it reads better than inlining.

No downward import; no new dependency direction.

## Phased rollout

**PR 1 — the orientation surface (the killer).** Single PR, ~"ideal" band.

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/domain.rs` (overview types), `src/server.rs` (verb + `bodies` arg + descriptions/instructions) | ~260 | ~260 |
| Tests | `src/server.rs` test module + dogfood assertions | ~180 | ~90 |
| **Total** | | | **~350** |

Deliverables: `project.overview` + the aggregation; `bodies:false` on `phase.list` / `task.list`; tool-description + server-`instructions` + `CLAUDE.md` recipe updates; tests (below). If review prefers, the `bodies:false` list-verb projection (Move 2) is cleanly separable into a fast-follow PR — but it's recommended in PR 1 so the steered drill-down path is bounded from day one.

**Parked follow-ups** — each its own evidence gate, not bundled:

- **`project.get` over-size guardrail** (D4) — gated on dogfood showing steering alone doesn't stop agents face-planting on `project.get`. If built, it must be designed properly (D4): not the v0-naïve "assemble-then-measure with a configurable budget."
- **Phase lifecycle (`archived`)** — the durable corpus-hygiene play; revive if the active surface stays noisy across reads after PR 1, or if `search` ranking degrades under terminal-task volume. Needs its own spec (status-vs-field, migration, default-filter semantics).
- **Pagination / cursors** on hydrating lists — gate on a real "limit without stable cursor" composition problem.
- **Overview-backing manifest at the Store seam** — see §Deferred; gated on `S3Store` going live.

## Deferred: the cache / denormalized manifest (and its real trigger)

The friction task floated a cached per-project manifest backing the overview read. We measured the walk on the live backend: ~131 local files, low-ms. A manifest would add a write-path invalidation burden (every `phase.*` / `task.*` / `artifact.*` write must keep it current) to optimize a latency `FsStore` doesn't have. The cost is real and immediate; the benefit is hypothetical *on local disk*. **Defer at v0** — not a permanent "no," just the wrong thing to build first.

The trigger that flips the decision is specific and already named: when **`S3Store` is the live backend**, overview-over-`list_tasks` becomes the O(tasks) GET fan-out described above — the second service-layer read (after `search`) to pay the warm-cache full-hydrate cost, and worse, the first body-load pass in `load_tasks_for` is sequential (N round-trips). At that point the manifest is the right shape.

Be honest about the upgrade path, because a naïve version doesn't work: a `bodies:false` read at the `Store` seam does **not** save S3 cost — S3 GET has no projection, so you pay full egress and discard the body (the existing `with_body:false` project read already proves this — it downloads the whole object and drops the body after). The real S3-era shape is a **separate per-project manifest object** (e.g. `.dossier/cache/<slug>/overview.json`, consistent with LAYOUT.md's `.dossier/cache/` convention for rebuildable indexes) maintained on write and read by a **narrow `Store` primitive** — which *is* a trait addition. That's allowed; it's just a later change gated on S3, not something to pre-build now and not something the v0 service-layer design forecloses. Lock which lane (manifest object + narrow read) before `S3Store` ships; build it under that pressure, with that evidence.

## Decisions to lock

- **D1 — verb vs flag for orientation.** Dedicated `project.overview` verb (bounded by phase count forever; service-layer aggregation per the `search`/D9 precedent), *plus* `bodies:false` on the list verbs for bounded drill-down. The two are complementary.
- **D2 — `project.description` in overview, truncated to 600 chars.** Include it, but **bounded** — the first **600 characters** of the project.md body (a fixed, non-configurable value — roughly one tight paragraph, enough for an agent to grasp the project without the full design doc) with a `description_truncated` flag, matching `search`'s snippet precedent. A project.md *is* the full design doc and is **not** length-bounded; including it whole would re-blow the cap on the `description` field for a project with a long doc. The 600-char bound keeps "a few KB regardless" literally true.
- **D3 — per-phase artifact counts: no, project-level only.** Artifacts link to a project (and optionally a task), never to a phase — `Artifact` has no `phase` field, and a project-only artifact (no `task`) has *no* phase attribution at all. A per-phase breakdown is therefore not just awkward but undefined-for-totality. Project-level rollup is the only well-defined aggregation.
- **D4 — `project.get` over-size guardrail: deferred, and redesigned if revived.** The original "typed error when the assembled payload exceeds a byte budget" had three problems the review surfaced: (a) it's a *regression* for callers that aren't failing — dossier-self (172 KB) sits inside the candidate budget yet is currently consumable, and programmatic full-hydrate callers (`dossier export`, cortex) have no token cap and legitimately want the whole blob; (b) a "lockable byte budget" is an un-opinionated config knob; (c) "assemble-then-measure" does the full O(all-tasks) hydrate (pathological on S3) just to refuse it. If revived after evidence: a *fixed* threshold set **at/above the real result cap** (so it only fires where the call already failed), an explicit `full: true` escape for deliberate full-hydrate callers, a named error code (`invalid_request` with a machine-readable `{ reason: "too_large", suggest: "project.overview" }`), and a *cheap* pre-check from counts — never assemble-then-discard. Until then, Move 3's steering carries it.
- **D5 — overview includes all phases, all statuses.** Including `done` and `skipped` phases (with their counts) — the point is seeing where finished vs open work sits; hiding done phases here would pre-empt the option-5 question. (Phase-level archival is the separate, deferred lever.)
- **D6 — partition of tasks across phases.** `phase` is a phase *id*. A task joins a phase row iff `task.phase == phase.id`; it falls into `unphased` iff `phase` is empty **or** dangling (a non-empty id matching no existing phase — possible via hand-edits per LAYOUT.md, or a future phase-delete). The reconcile invariant (Move 1) and an orphan-injection test keep the partition exhaustive. The field stays named `unphased` (not `unanchored` / `orphaned`) — the common case is genuinely unphased, and the rare dangling case is folded in with the contract spelled out in the tool description ("empty or referring to a deleted phase") so the name isn't misread.
- **D7 — phase recency.** Include `updated_at` on each `PhaseOverview` row (free; phase frontmatter already carries it; stays bounded) so "what changed recently" is answerable in the one orient call instead of a follow-up `phase.list { order_by: updated_at }`.
- **D8 — corrupt-corpus behavior: fail, don't skip.** A corrupt / unparseable task or phase file fails the whole `project.overview`, matching `project.get`'s current behavior. Corrupted state must never silently become "fewer tasks" — a skip-and-count policy would mask data loss behind a plausible number. Surface it loudly.

## Non-goals

- No RAG / vectors / semantic retrieval. Counts are exact aggregations over frontmatter.
- No new on-disk format, no migration (overview is derived at read time).
- No change to `project.get`'s default shape or behavior (only its description); the guardrail is deferred (D4).
- No phase-archival lifecycle in this PR (parked).
- No cross-project overview — `project.list` serves the portfolio-level read; overview is single-project by design. (An agent orienting across the portfolio stays on `project.list`, not a per-project overview loop — relevant once S3 makes per-call fan-out costly.)
- No per-phase or per-task artifact breakdown (D3).
- **No new `dossier` CLI subcommand.** The binary exposes a curated write/list subset; project reads are MCP-only today (`project.get` isn't a subcommand either), and overview follows that precedent.

## Test plan

- **Aggregation correctness** (synthetic temp corpus): phases at mixed statuses; tasks spread across all statuses; an unphased task; **an orphaned-phase-id task** → assert per-phase `task_counts`, the `unphased` bucket, `totals`, and the reconcile invariant `Σ phase totals + unphased == grand total` all hold; assert **no body/notes field** appears anywhere in the output.
- **Counts contract**: every status key present (zero when none); `total` equals the bucket sum on every map.
- **Ordering**: phases in `order` ASC, ties by id ASC — same as `phase.list` single-project default (duplicate `order` values resolve deterministically; frontmatter `order` is authoritative, the filename prefix is cosmetic).
- **Description truncation** (D2): a project with a long project.md → `description` clipped to the bound, `description_truncated == true`; a short one → full body, flag `false`.
- **`bodies:false`** (Move 2): `phase.list { project, bodies:false }` and `task.list { project, bodies:false }` omit the body (and notes) fields; default still returns them.
- **Empty project**: no phases/tasks → empty `phases`, zeroed counts, not an error.
- **Not found**: unknown slug → typed `invalid_params`, same shape as `project.get` today.
- **Corrupt corpus** (D8): a corrupt task/phase file fails the whole overview (not skip-and-undercount), matching `project.get` today — a test pins fail-on-corrupt so it's defined, not incidental.
- **Dogfood**: `project.overview { slug: "dossier" }` against the in-repo fixture returns a bounded result (assert a byte ceiling) and the right phase count; pins the on-disk pipeline like `read_dogfood_corpus` does.
- **`make check` green** on the repo matrix (fmt + clippy `-D warnings` + test).

## Acceptance

- `project.overview { slug: "ship" }` returns a payload that fits inline — a few KB, roughly two orders of magnitude under today's 394 KB — and lets an agent read every phase slug + status + per-phase open-task counts + recency in one call. (The dogfood byte-ceiling is set from the *measured* output once fields are final, not a fixed percentage: at 25 phases the schema floor alone is ~6 KB, above a naïve "< 1%" target — so the test asserts an absolute ceiling, e.g. ≤ 25 KB, not a ratio.)
- The recommended orient → drill-down path is bounded end to end: `phase.list { bodies:false }` / `task.list { bodies:false }` return slug/title/status without bodies.
- The onboarding surfaces are updated so agents actually reach for `project.overview` first: the verb's tool description, the dossier MCP server `instructions`, and the `CLAUDE.md` orientation recipe (this is an acceptance criterion, not an aside).
- No existing verb changes default shape; no on-disk migration.
