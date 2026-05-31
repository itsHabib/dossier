**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-31
**Related**: dossier task `lift-task-state-machine` (id: `tsk_01KSZT0833NJTH8Q1HE8ZCHQVZ`), phase `policy-mechanism-separation`, [docs/follow-ups.md](../../follow-ups.md)

# Lift the task verbs into MeshService as CAS loops — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (4 verbs → CAS loops), `src/store.rs` (delete inherent task methods) | ~250 | 250 |
| Tests | `tests/proptest_state_machine.rs` (reseed via service) + new concurrent-claim CAS test | ~120 | 60 |
| **Total** | | | **~310** |

Band: **amazing**. Single PR. **Depends on `domain-extract-pure-core`** (must land first — the lifted verbs call the pure transition fns it extracts).

## Goal

The task write verbs (create / claim / update / complete) run as inherent sync methods on `FsStore` (`src/store.rs:1170-1367`), so `MeshService` must hold a concrete `fs: Arc<FsStore>` to call them. The state machine — the policy — lives in the mechanism layer. It needs to run at the service layer over `Arc<dyn Store>` so S3Store gets the identical state machine.

## Behavior / fix

Lift `create_task` / `claim_task` / `update_task` / `complete_task` into `MeshService` (`src/server.rs`). Each becomes a:

```
get → run-transition (the pure domain fns from domain-extract-pure-core) → put_task(expected = version)
```

CAS loop, per cloud spec §7.1's three-way branch on `Conflict` (`docs/features/cloud-backend/spec.md`):

- **terminal** — the state machine now rejects the transition → surface the error, no retry.
- **idempotent** — desired state already reached (e.g. same-actor re-claim) → return Ok.
- **true-retry** — version moved under us → re-read, re-apply, bounded full-jitter backoff.

Delete the inherent `FsStore` task methods once the service owns them. Rewrite the task test-seed helpers (`src/server.rs` ~1248-1265 + `tests/proptest_state_machine.rs`) to seed via the service rather than `fs.create_task`. Add a service-layer concurrent-claim CAS test: two writers race a claim on one task → exactly one `Ok`, the loser re-reads to a terminal "already claimed" (no third outcome), looped.

## Acceptance

- `MeshService` task verbs run over `self.store` (the trait), not `self.fs`.
- The inherent `FsStore` task methods are gone.
- The state-machine proptest passes through the service path.
- The new concurrent-claim CAS test passes (one winner, looped).

## Test plan

- `make check` green.
- State-machine invariants 1–5 hold via the service.
- New claim-race test green (one winner; the loser re-reads to a terminal already-claimed; no third outcome).
- `grep -n "fn claim_task\|fn complete_task\|fn create_task\|fn update_task" src/store.rs` returns nothing (methods lifted out).

## Non-goals

- The phase / project / artifact verbs (unit 3).
- The `fs: Arc<FsStore>` field stays for now — `lift-phase-project-artifact-drop-fs` drops it once the remaining verbs are lifted.
- The `write_lock` stays (recommended KEEP per the phase body): the process-local mutex is FsStore's intra-process belt-and-suspenders over the get→put CAS authority — don't change two things at once.
