**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-30
**Related**: dossier tasks `search-to-service-layer` (`tsk_01KSV24EGWKFYR6EE0S1S00S9M`) + `phase-ordering-concurrency` (`tsk_01KSV24HKSZRA3YCQ76YWKGV62`); cloud-backend TDD [spec.md](../spec.md) §6/§8 (D9); builds on merged PR #62 (Store trait + CAS).

# Phase 0 batch 2 — move `search` to the service layer **and** fix `phase.add` concurrency (one PR)

**Two related Phase-0 changes in a single PR** (both small, both sit on `src/store.rs` + `src/server.rs`, and both build on the now-merged `Store` trait + `cas_write` from PR #62). Full per-task detail in the sibling specs — implement both:
- [search-to-service-layer.md](search-to-service-layer.md)
- [phase-ordering-concurrency.md](phase-ordering-concurrency.md)

## Context from batch 1 (PR #62, already on main)

`MeshService` now holds **two** handles: `store: Arc<dyn Store>` (async reads, returning `Versioned<T>`) and `fs: Arc<FsStore>` (inherent write verbs + `search`), plus the `write_lock: Arc<Mutex<()>>`. The `Store` trait exposes CAS writes (`put_*` with `Option<Version>`) backed by `cas_write` (SHA-256 compare-before-atomic-rename). `cas_write` is documented as single-writer-assumed and is **not yet wired into any MCP write verb** — that wiring is part B below.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/store.rs` (remove `FsStore::search` + `search_snippet`; CAS the `phase.add` write path), `src/server.rs` (`search` in `MeshService`; `phase_add` made concurrency-safe) | ~140 | ~140 |
| Tests | search parity (relocated) + a store/CAS-layer phase-ordering race test | ~130 | ~65 |
| **Total** | | | **~205** |

Band: **amazing/ideal** (< 700 weighted). One coherent PR per the bigger-PR sizing preference — two ~90-100 line changes that would otherwise be wasteful as separate tiny PRs.

---

## Part A — move `search` out of `Store`/`FsStore` into the service layer (D9)

**Goal.** `search` is application query, not storage. Over a remote backend it would leak (download-and-scan or secret cache access). Keep the storage layer to CRUD; run `search` in `MeshService` over the corpus.

**Behavior.**
- Move the `search` implementation (currently `FsStore::search` + the private `search_snippet` helper in `src/store.rs`) **up** into `MeshService` (`src/server.rs`). Behavior is byte-for-byte identical — same ranking, snippet generation, `kinds` / `project` / `limit` filtering, score-then-`updated_at`-desc ordering. This is a **relocation**, not a redesign.
- Remove `FsStore::search` and `search_snippet` from `src/store.rs`. `search` is **not** a `Store` trait method (D9).
- The `search` MCP verb handler stops calling `self.fs.search(...)` and runs the service-layer implementation, reading the corpus through the handles `MeshService` already holds. The verb's wire signature / description / output are unchanged.

**Acceptance.** `search` returns identical results to today for every existing search test; `grep`-confirmable that there is no `fn search` in `src/store.rs`. `make check` green.

---

## Part B — resolve `phase.add` order lost-update under concurrency (§8)

**Goal.** Two concurrent `phase.add` calls both read `project.md`, both compute a new `order`, both write — the second silently stomps the first. Resolve it in Phase 0 using the merged CAS model.

**Behavior.** Pick **one** (either satisfies acceptance — choose whichever is simpler against the current code):
- **(a) CAS the `project.md` / phase-order writes** *(recommended — reuses PR #62's `cas_write`)*. Route the `phase.add` read-compute-order-write through a compare-and-swap: read the collection state with its version, compute the new `order`, `put_*(expected = <version read>)`; on `Conflict`, re-read, recompute a fresh distinct `order`, retry (bounded — surface a typed conflict on exhaustion, no unbounded spin). **This is the moment `cas_write` gets wired into a write verb, so the caller must hold `write_lock` across the read-modify-write** (see the SAFETY note on `cas_write` from PR #62).
- **(b) `order` immutable-by-insertion**. Derive ordering from creation (timestamp / ULID), never explicitly reorder/shift — removes the read-modify-write race at its root.

> Note: `MeshService.write_lock` only serializes writes *within one process*; it does not prevent the lost update across independent writers (the multi-writer S3 case). The fix is the write-time CAS, not the lock — which is why the test must drive the CAS path directly, not spawn two threads against one shared service.

**Acceptance.** Two writers creating a phase from the same starting version both land with **distinct, stable `order`** and no lost update (the second gets `Conflict` → re-read → fresh `order`). Existing phase tests pass. `make check` green.

**Test plan.** A store/CAS-layer race test (not two threads on one `MeshService` — the `write_lock` would serialize them and mask the race): two writers read at `v0`; A `put_*(expected=v0)` succeeds → `v1`; B `put_*(expected=v0)` → `Conflict` → re-reads `v1` → recomputes a distinct `order` → succeeds. Assert both phases present, distinct/stable `order`. (Approach (b): assert two phases from `v0` persist creation-ordered with no shift-on-insert rewrite.)

---

## Non-goals (both parts)

- Cache-aware search / derived index (Phase 3a+ / deferred).
- The general CAS 3-way retry branch with full jitter (Phase 1, with `S3Store`).
- Migrating the *other* write verbs (task/artifact) onto the trait's CAS — only `phase.add` is in scope here.
- The read-path redundant-I/O optimization flagged on PR #62 (tracked separately).
