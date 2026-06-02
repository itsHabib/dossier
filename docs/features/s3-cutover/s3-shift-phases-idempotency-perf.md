**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-01
**Related**: dossier task `s3-shift-phases-idempotency-perf` (id: `tsk_01KT1WQ1NWPVBH5PMEDNJ2N596`), [docs/follow-ups.md](../../follow-ups.md)

# Fix S3Store::shift_phases — idempotency + O(n) listing — design spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/s3store.rs` (`shift_phases`) | ~40 | 40 |
| Tests | MinIO partial-failure + key-count test | ~40 | 20 |
| **Total** | | | **~60** |

Band: **amazing**. Runtime: **local** (MinIO).

## Goal

`S3Store::shift_phases` (PR #69) has two issues on the S3 path:

1. **Not idempotent on partial failure** — it writes the new key with `cas_put(…, None)` (`if-none-match=*`). If the process dies between the `cas_put` and the `delete_object`, a retry fails at `cas_put` with `Conflict` on every subsequent phase → the corpus is stuck.
2. **O(n) listing** — `find_phase_key` runs inside the shift loop, each doing an S3 list → O(n) lists for n phases.

## Behavior / fix

1. Replace the create-only `cas_put(&new_key, content, None)` with an **unconditional `put_object`** for the new key. Safe because the descending-order loop vacates each target slot before writing to it (the write never clobbers a live phase). This makes a partial shift replay-safe.
2. **Resolve all phase keys once** before the loop (single list), not per-iteration `find_phase_key`.

`FsStore::shift_phases` (rename-based) is unchanged — it's already idempotent.

## Acceptance

- A simulated mid-shift failure (crash between put and delete) is recoverable on retry — no permanent `Conflict`.
- An n-phase shift performs O(1) S3 list operations.

## Test plan

- `make check` green (FsStore path unaffected).
- Against MinIO: a shift that fails partway can be re-run to completion; assert the final `phases/` keys are exactly the expected `{order}.<slug>.md` set with no orphans.
- Assert the S3 list-call count for an n-phase shift is 1.

## Non-goals

- The FsStore shift path (already idempotent via `fs::rename`).
- Broader S3 retry/backoff policy (covered by the §8 budget elsewhere).

## Source

PR #69 review (claude finding 1, copilot s3store comment).
