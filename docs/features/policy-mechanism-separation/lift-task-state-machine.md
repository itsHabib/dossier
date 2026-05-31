**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-31
**Related**: dossier task `lift-task-state-machine` (id: `tsk_01KSZT0833NJTH8Q1HE8ZCHQVZ`), phase `policy-mechanism-separation`, [docs/follow-ups.md](../../follow-ups.md)

# Lift the task verbs into MeshService as CAS loops — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (claim/update/complete self-CAS + create project-CAS), `src/store.rs` (delete inherent task methods) | ~290 | 290 |
| Tests | `tests/proptest_state_machine.rs` (reseed via service) + concurrent claim-race + same-slug create-race tests | ~150 | 75 |
| **Total** | | | **~365** |

Band: **amazing**. Single PR. **Depends on `domain-extract-pure-core`** (must land first — the lifted verbs call the pure transition fns it extracts).

## Goal

The task write verbs (create / claim / update / complete) run as inherent sync methods on `FsStore` (`src/store.rs:1170-1367`), so `MeshService` must hold a concrete `fs: Arc<FsStore>` to call them. The state machine — the policy — lives in the mechanism layer. It needs to run at the service layer over `Arc<dyn Store>` so S3Store gets the identical state machine.

## Behavior / fix

Lift `create_task` / `claim_task` / `update_task` / `complete_task` into `MeshService` (`src/server.rs`). The three **mutate-existing** verbs (`claim` / `update` / `complete`) each become a:

```
get → run-transition (the pure domain fns from domain-extract-pure-core) → put_task(expected = version)
```

CAS loop, per cloud spec §7.1's three-way branch on `Conflict` (`docs/features/cloud-backend/spec.md`):

- **terminal** — the state machine now rejects the transition → surface the error, no retry.
- **idempotent** — desired state already reached (e.g. same-actor re-claim) → return Ok.
- **true-retry** — the version moved under us → re-read, re-apply with full-jitter backoff. Take the concrete budget from cloud spec §8, not a hand-rolled one: `base = 25ms`, `cap = 2s`, **max 5 attempts**, `sleep = uniform(0, min(cap, base·2ⁿ))`; on exhaustion surface a typed `conflict` error to the caller (never silent). §8 is the single retry budget for every lifted CAS loop — the older `PHASE_ADD_MAX_RETRIES = 8` is superseded, and unit 3's `add_phase` converges on the same numbers.

### `create_task` — project-scoped uniqueness, not self-CAS

`create` is **not** a mutate-existing transition, so it does **not** fit the loop above. A new task has no prior version, and `put_task(expected = None)` is keyed on the freshly-minted `tsk_…` id — so two concurrent creates with the same slug write *different* objects and **neither conflicts**, leaving the project with duplicate task slugs. (Today's `load_tasks_for` → `any(|t| t.slug == args.slug)` check inside `create_task`, `src/store.rs:1170-1367`, is only race-safe because FsStore's `write_lock` serializes it; that guarantee is gone once the verb runs over `Arc<dyn Store>` against S3.) The uniqueness invariant is **project-scoped**, so the CAS authority must be the *project*, not the task:

```
get project (versioned) + list tasks → assert slug free → put_project(expected = project_version)  // claim the write slot, bump the project version
                                                         → put_task(expected = None)                // create-only
on Conflict(project) → re-read, re-check slug:
    slug now taken  → terminal: `task slug already exists in project`   (no retry)
    slug still free → true-retry (§8 budget)
```

This mirrors `add_phase`'s `project.md` CAS gate (the same pattern unit 3 lifts) and cloud spec **D2** (`project.md` is the CAS point for project-scoped invariants). **Decision to confirm at implementation:** the project-CAS gate is the recommended mechanism (consistent with `add_phase`); the alternatives Codex named — a per-project slug→id index object written create-only with `If-None-Match`, or a deterministic slug-keyed task object — are acceptable if they preserve the same invariant. Pick one and record it; do not ship the list-then-write race.

Delete the inherent `FsStore` task methods once the service owns them. Rewrite the task test-seed helpers (`src/server.rs` ~1248-1265 + `tests/proptest_state_machine.rs`) to seed via the service rather than `fs.create_task`. Add a service-layer concurrent-claim CAS test: two writers race a claim on one task → exactly one `Ok`, the loser re-reads to a terminal "already claimed" (no third outcome), looped.

## Acceptance

- `MeshService` task verbs run over `self.store` (the trait), not `self.fs`.
- The inherent `FsStore` task methods are gone.
- The state-machine proptest passes through the service path.
- The new concurrent-claim CAS test passes (one winner, looped).
- A concurrent **same-slug create** race yields exactly one `Ok`; the loser terminal-rejects with `task slug already exists in project`, and the project never holds two tasks with the same slug.

## Test plan

- `make check` green.
- State-machine invariants 1–5 hold via the service.
- New claim-race test green (one winner; the loser re-reads to a terminal already-claimed; no third outcome).
- New same-slug create-race test green: two writers race `create` with the same project + slug → one `Ok`, one terminal `slug already exists`; the project holds exactly one task with that slug (looped).
- `grep -n "fn claim_task\|fn complete_task\|fn create_task\|fn update_task" src/store.rs` returns nothing (methods lifted out).

## Non-goals

- The phase / project / artifact verbs (unit 3).
- The `fs: Arc<FsStore>` field stays for now — `lift-phase-project-artifact-drop-fs` drops it once the remaining verbs are lifted.
- The `write_lock` stays (recommended KEEP per the phase body): the process-local mutex is FsStore's intra-process belt-and-suspenders over the get→put CAS authority — don't change two things at once.
