**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-01
**Related**: dossier task `service-slug-validation` (id: `tsk_01KT1WPV5S2619WMFKEMHB5SDZ`), [docs/follow-ups.md](../../follow-ups.md)

# Validate slugs at the service boundary — design spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (`is_valid_slug` guards on 5 verbs) | ~30 | 30 |
| Tests | one rejection test per verb | ~60 | 30 |
| **Total** | | | **~60** |

Band: **amazing**. Runtime: **local**.

## Goal

After the write-verb lift (PR #69), several `MeshService` verbs — `create_project`, `update_project`, `add_phase` (project slug), `update_phase`, `link_artifact` — validate slugs only via the backend. An invalid project slug reaches the store and returns a backend-specific error (`FsStore`: `"invalid slug: …"`) that the MCP error mapper classifies as `internal_error` instead of a clean validation error. Slug validation is policy and belongs at the service boundary.

## Behavior / fix

Add `is_valid_slug` guards at the top of the affected verbs (mirroring `add_phase`'s existing phase-slug check), returning a clean `invalid_msg(format!("slug must be lowercase ascii (a-z, 0-9, -, _): {slug}"))` before touching the store. Validate the relevant slug per verb:

- `create_project` / `update_project` → the project's own slug.
- `add_phase` / `update_phase` / `link_artifact` → the `project` slug (`add_phase` already validates the phase slug; add the project one).

## Acceptance

- An invalid project slug to each of the 5 verbs returns a typed validation error (`invalid`), not `internal_error`.

## Test plan

- `make check` green.
- One test per verb asserting an invalid project slug is rejected at the service boundary with the validation message.

## Non-goals

- Changing `is_valid_slug`'s rules.
- The create-task slug path (already validated in unit 2).

## Source

PR #69 review (copilot, 5 inline comments). On-thesis for policy/mechanism.
