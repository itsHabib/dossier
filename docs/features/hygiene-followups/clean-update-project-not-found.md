**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `clean-update-project-not-found` (id: `tsk_01KRSZFVCFPYRN3Q7RCFGQ89HQ`), [docs/follow-ups.md](../../follow-ups.md)

# Clean "project not found" on update_project — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/store.rs` | ~10 | 10 |
| Tests | `src/store.rs` test module | ~20 | 10 |
| **Total** | | | **~20** |

Band: amazing.

## Goal

`update_project` on a nonexistent slug today returns the raw OS error: `os error 2: The system cannot find the file specified`. The LLM caller can't tell whether the input was wrong or the server broke. Fix: surface a typed "project not found" error.

## Behavior / fix

Mirror the `add_phase` pattern — explicit existence check before mutation. If the project dir doesn't exist (or `project.md` isn't there), `bail!("project not found: {slug}")` with a typed error before any file I/O.

## Acceptance

- `update_project` on missing slug returns a typed error containing `"project not found"` and the slug.
- Existing happy-path behavior unchanged.

## Test plan

- `update_project_errors_on_nonexistent_slug` asserts the error message + type.

## Non-goals

- Generalizing to `update_phase` (covered in `slug-validation-remaining-paths` indirectly via the slug check there; can be its own follow-up if not).
- New error types beyond the consistent `bail!` shape `add_phase` uses.
