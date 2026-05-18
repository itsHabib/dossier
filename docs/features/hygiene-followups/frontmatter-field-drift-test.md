**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `frontmatter-field-drift-test` (id: `tsk_01KRSZG2ZYV3BTY6E8HCPHG8S1`), [docs/follow-ups.md](../../follow-ups.md), `tests/proptest_frontmatter_roundtrip.rs` (from PR #17)

# Frontmatter field-drift round-trip tests — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Tests | `src/store.rs` test module (or `tests/` sibling) | ~80 | 40 |
| **Total** | | | **~40** |

Band: amazing.

## Goal

`ProjectFrontmatter` / `PhaseFrontmatter` / `TaskFrontmatter` are hand-maintained alongside the domain types. Adding a `Project` field today does NOT fail to compile — it just silently doesn't persist (the field isn't in the frontmatter struct). Mitigate with an exhaustive round-trip test per type that catches drift.

## Behavior / fix

Add one round-trip test per primitive frontmatter struct. Each test:
1. Constructs a fully-populated domain instance (every field set, non-default values).
2. Serializes to frontmatter via the existing write path.
3. Reads it back from disk via the existing read path.
4. Asserts every field on the deserialized value matches the original.

Co-locate with the existing proptest roundtrip module from PR #17 if natural; otherwise add as unit tests in `src/store.rs` test module.

## Acceptance

- A deliberate test: temporarily add a field to `Project` without updating `ProjectFrontmatter` → the new round-trip test fails with a clear "field X not persisted" signal. Revert; test passes.

## Test plan

- `project_frontmatter_roundtrip_all_fields`
- `phase_frontmatter_roundtrip_all_fields`
- `task_frontmatter_roundtrip_all_fields`

## Non-goals

- Introducing `#[serde(flatten)]` markers (mentioned as optional in the task body — this PR is test-only).
- Touching production frontmatter structs.
