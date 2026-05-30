**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-29
**Related**: dossier task `phase-ordering-concurrency` (id: `tsk_01KSV24HKSZRA3YCQ76YWKGV62`), cloud-backend TDD [spec.md](../spec.md) §8 (PR #59), [docs/follow-ups.md](../../../follow-ups.md)
**Depends on**: `extract-store-trait` (implements under that task's `Store` CAS model)

# Resolve `phase.add` order lost-update under concurrency — design spec

Phase 0 of the cloud-backend rollout. Lands **after** `extract-store-trait`.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/server.rs` (`phase_add` handler, `src/server.rs:560` — make the read-compute-write safe), `src/store.rs` (phase-order write path, `src/store.rs:730` — CAS the affected writes) | ~70 | ~70 |
| Tests | concurrency test firing two `phase.add` at one project | ~60 | ~30 |
| **Total** | | | **~100** |

Band: **amazing** (< 500 weighted).

## Goal

Two concurrent `phase.add` calls against one project both read `project.md`, both compute a new `order`, both write — the second silently stomps the first (classic lost update), and the order-shift logic (`src/store.rs:730`, "shift existing phases at or above new_order") compounds it. Harmless under today's single-writer assumption, but a real correctness gap the moment there are concurrent writers — which is the entire point of the cloud backend. Flagged in round-2 review; the TDD (§8) calls for resolving it **in Phase 0**, not deferring.

## Behavior / fix

> The `MeshService.write_lock` (`Arc<Mutex<()>>`) only serializes writes **within a single process** — it does *not* prevent the lost update across independent writers (multiple `Store` handles / processes), which is exactly the multi-writer case the cloud backend introduces. The fix is the **write-time CAS**, not the in-process lock. (This is also why the test must drive the CAS path directly rather than spawn two threads against one shared service — see Test plan.)

Pick **one** of the two approaches from TDD §8 (either satisfies the acceptance criteria — choose by whichever comes out simpler against the current code):

- **(a) CAS the `project.md` / phase-order writes** *(recommended — reuses the `extract-store-trait` model)*. Wrap the read-compute-order-write in `phase_add` in a compare-and-swap: read `project.md` (+ affected phase files) with their versions, compute the new `order`, `put_*` with `expected = <version read>`. On `Conflict`, re-read and recompute the `order`, then retry. A second concurrent `phase.add` that raced in gets a `Conflict`, re-reads the now-updated state, and recomputes a fresh distinct `order` — no lost update. This is the same CAS primitive `extract-store-trait` adds; this task wires it into the `phase.add` path.
- **(b) `order` immutable-by-insertion**. Stop recomputing/shifting `order` on insert; derive ordering from creation (e.g. sort by creation timestamp / ULID), never explicitly reorder. Removes the read-modify-write race at its root by making the field append-only. Larger change to the existing `NN-<slug>.md` ordering scheme, so only take this path if it lands cleaner than (a).

Either way, the observable contract is the same and the bounded CAS retry must be finite (no unbounded spin) — on the rare sustained-contention case, surface a typed conflict rather than looping forever. The full 3-way re-read branch + jittered retry (TDD §7.1/§8) is **not** required here — that arrives with `S3Store` in Phase 1; Phase 0 needs only enough to make two racing `phase.add`s both land safely.

## Acceptance

- Two concurrent `phase.add` calls against one project both land, with **distinct, stable `order` values** and both phases present — **no lost update**.
- Existing phase tests (`phase_add`, `phase_list`, ordering) pass unchanged.
- `make check` green.

## Test plan

> Two threads against a single shared `MeshService` would just queue behind the in-process `write_lock` and **never reproduce the race** — they serialize, so the second read already sees the first write. The lost update this task fixes is the *multi-writer* one (independent `Store` handles / processes) the lock doesn't cover, so the test must drive the CAS path directly, not rely on real thread contention through the service lock.

- **Store/CAS-layer race test (the gate):** simulate two independent writers over one temp corpus. Both read the phase-collection state (`project.md` + phases) at version `v0`; writer A computes its `order` and `put_*(expected = v0)` → succeeds → `v1`; writer B `put_*(expected = v0)` → **`Conflict`**; B re-reads (now `v1`), recomputes a **fresh distinct** `order`, `put_*(expected = v1)` → succeeds. Assert both phases present with **distinct, stable** `order` and no lost update. Fails against the current read-compute-write (B would stomp A); passes after the fix.
- **Retry-loop unit test (approach a):** a `phase.add` whose underlying `project.md` version changed out from under it takes the `Conflict` → re-read → recompute → re-put branch and still lands a correct distinct `order` (not an error or a duplicate), within the bounded retry budget.
- **If approach (b) immutable-by-insertion instead:** assert two phases created from the same `v0` both persist with creation-ordered, distinct positions and **no shift-on-insert rewrite** of the sibling files — i.e. there is no read-modify-write left to lose.

## Non-goals

- Explicit reorder / move-phase verbs.
- The general CAS 3-way retry branch with full jitter (TDD §8) — Phase 1, with `S3Store`.
- CAS on task / artifact writes beyond what `extract-store-trait` already provides — this task is scoped to the `phase.add` ordering race only.
