**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `actor-on-update-verbs` (id: `tsk_01KRSZFXMNZYKCMWYV3Z85E6XJ`), [docs/follow-ups.md](../../follow-ups.md), [docs/vision.md](../../vision.md)

# Actor on update verbs — drop — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` | ~10 | 10 |
| Tests | `src/server.rs` test module | ~10 | 5 |
| **Total** | | | **~15** |

Band: amazing.

## Goal

`ProjectUpdateArgs.actor` and `PhaseUpdateArgs.actor` are accepted with `#[allow(dead_code)]` and discarded. The arg surface lies to callers about what's persisted. Per vision.md, audit log / `last_updated_by` is out of v0 scope.

## Decision

**Drop** the `actor` field from both arg structs. (The "commit to `last_updated_by`" alternative is rejected per vision.md.)

## Behavior / fix

- Remove `actor` from `ProjectUpdateArgs`.
- Remove `actor` from `PhaseUpdateArgs`.
- Drop the `#[allow(dead_code)]` annotations that gated each.
- Update any tests that were passing `actor` to the update verbs.

Note: this change is upstream of `phase-created-by-parity`, which adds `created_by` to `Phase` and would use the actor arg on **`add_phase`** (a create verb — actor stays there). Land this PR first so the "actor only on create verbs" pattern is established cleanly.

## Acceptance

- `project.update` schema no longer surfaces `actor`.
- `phase.update` schema no longer surfaces `actor`.
- Calling either verb succeeds without supplying `actor`.

## Test plan

- Existing update-verb tests verify the new arg shape compiles and runs.

## Non-goals

- Adding `last_updated_by` to the domain model.
- Touching `actor` on create verbs.
