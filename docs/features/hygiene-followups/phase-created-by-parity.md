**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `phase-created-by-parity` (id: `tsk_01KRSZFZY8DGTE19QSRYV0W3BA`), [docs/follow-ups.md](../../follow-ups.md)
**Depends on**: `actor-on-update-verbs` lands first — establishes the "actor only on create verbs" pattern so `add_phase`'s actor arg has a clear semantic role.

# Phase.created_by parity with Project.created_by — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/domain.rs`, `src/store.rs`, `src/server.rs`, `LAYOUT.md` | ~50 | 50 |
| Tests | `src/store.rs` test module | ~30 | 15 |
| **Total** | | | **~65** |

Band: amazing.

## Goal

`Project` carries `created_by`; `Phase` doesn't. LAYOUT.md's phase example also omits it. Inconsistency between primitives = a small cognitive tax that compounds.

## Behavior / fix

1. Add `created_by: String` to `Phase` in `src/domain.rs`.
2. Add `created_by` to `PhaseFrontmatter` in `src/store.rs`.
3. Plumb the value through `add_phase` — the `actor` arg currently discarded (`let _ = args.actor;`) gets stored as `created_by`.
4. Update `LAYOUT.md`'s phase example to show `created_by: ...`.
5. Existing phases without `created_by` in frontmatter default gracefully on read (e.g., `"unknown"`).

## Acceptance

- New phases created via `phase.add` persist `created_by` from the actor.
- Old phases without the field round-trip with a default value.
- `LAYOUT.md` phase example is current.

## Test plan

- `add_phase_persists_created_by` — round-trip the new field.
- `read_phase_with_missing_created_by_defaults_gracefully` — backwards-compat read.

## Non-goals

- Adding `created_by` to Task (it already has `assignee`).
- Adding `updated_by` to anything (audit out of scope).
- Migrating existing on-disk phases (default-on-read covers it).
