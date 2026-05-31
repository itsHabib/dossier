**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-31
**Related**: dossier task `domain-extract-pure-core` (id: `tsk_01KSZT03F9GB5YNRATRM4BAX70`), phase `policy-mechanism-separation`, [docs/follow-ups.md](../../follow-ups.md)

# Extract the pure policy core to domain.rs — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/domain.rs` (pure fns + arg-structs, moved from store), `src/store.rs` (methods delegate) | ~240 | 240 |
| Tests | `tests/proptest_state_machine.rs` (retargeted to drive pure fns) | ~90 | 45 |
| **Total** | | | **~285** |

Band: **amazing**. Single PR. No deps — lands first in the phase.

## Goal

dossier's task state machine, validation, id minting, and phase-order computation are baked into `FsStore` (`src/store.rs`) — the persistence/mechanism layer. They're protocol decisions that belong above the storage seam, not inside one backend. Before the write verbs can move up to `MeshService` (units 2–3), the shared pure core has to come out of the mechanism so both the service and any `Store` impl — `FsStore` today, the merged S3Store from #64 next — reference one copy.

`src/domain.rs` today is plain types only (no I/O, 1:1 with PROTOCOL.md). This PR adds the pure *behavior* alongside those types — still no I/O.

## Behavior / fix

Extract to `src/domain.rs` as pure functions (no `std::fs`, no I/O):

- **Task transitions** as pure `(Task, args) -> outcome`:
  - claim: pure `(Task, actor) -> outcome` over the **full** matrix (mirrors `claim_task`); the test plan exercises every branch:
    - `todo` + empty assignee → `claimed` (stamp assignee + `claimed_at`).
    - held state + `assignee == actor` → no-op, return the task unchanged.
    - held state + `assignee != actor` (non-empty) → reject `task already claimed by <assignee>`.
    - terminal (`done` / `cancelled`) → reject `cannot claim task in terminal state`.
    - corrupt (`todo` + non-empty assignee, or non-`todo` + empty assignee) → reject `corrupt state`.
  - update: already pure — `validate_task_update_transition`; move as-is.
  - complete: must be `in_progress` → `done`.
- **Phase-order computation**: the `new_order` calc from `after_phase` / max+1 — pure given the existing-phases slice.
- **Validation**: `is_valid_slug`, `validate_task_body`, the single-line / newline guards.
- **`new_id` minting**.
- **The verb arg-structs** — `NewProject`, `NewTask`, `ClaimTask`, `UpdateTask`, `CompleteTask`, `NewPhase`, `UpdatePhase`, `UpdateProject`, `LinkArtifact` — protocol DTOs; move from `store.rs` to `domain.rs`.

`FsStore`'s inherent write methods then delegate to the new pure fns — behavior-preserving, no verb behavior changes. House style: no `else` / early-return / line-of-sight, ≤2 nesting, errors-as-values.

## Acceptance

- Pure fns live in `src/domain.rs` with no `std::fs` / I/O.
- `FsStore` methods call the pure fns instead of inline logic.
- A no-I/O proptest drives the transitions directly; the existing `tests/proptest_state_machine.rs` and the full suite still pass unchanged.

## Test plan

- `make check` green (fmt + clippy-strict `-D warnings` + test).
- `tests/proptest_state_machine.rs` retargeted to also drive the pure transition fns directly (no `FsStore`); state-machine invariants 1–5 hold.
- The pure claim fn is asserted across **every** matrix branch: `todo`→claimed, same-actor no-op, different-actor reject, terminal reject, and both corrupt-state rejects (the different-actor and corrupt cases are the ones a summary drops — cover each explicitly so the proptest has no blind spot).
- `grep -n "std::fs" src/domain.rs` returns nothing.

## Non-goals

- Moving the verbs themselves to the service (units 2–3). `FsStore` still owns the inherent write methods after this PR — they just delegate to `domain`.
- No `fs`-field change on `MeshService`.
