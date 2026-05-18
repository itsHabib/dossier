**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `phase-slug-cross-project-collision` (id: `tsk_01KRW29YMNAG063612PHZY4EH6`), [docs/follow-ups.md](../../follow-ups.md)

# Document phase-slug collisions across projects — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Docs | `PROTOCOL.md` (or `LAYOUT.md`) — one paragraph in `Common gotchas` / slug-uniqueness section | ~15 | 0 |
| **Total** | | | **0** |

Band: amazing (docs-only, no PR-size impact).

## Goal

Phase slugs (and task slugs) are unique *within* a project (enforced) but not across the corpus (unguarded by design). Two projects can both have a `write-side` or `query-surface` phase — and several already do across the portfolio. Genuine feature (slug is a per-project name, not a global ID — the ULID covers global uniqueness), but it's not documented anywhere a tool author would notice.

## Behavior / fix

Add one paragraph under PROTOCOL.md's slug-uniqueness section (or LAYOUT.md if more apt) clarifying:

> **Slug scope.** Phase and task slugs are unique **within their parent project**, not globally. Two projects can both have phases named `write-side` (and several in the dossier portfolio do). Use the project slug + phase slug as the addressing tuple at the MCP boundary; the ULID is the corpus-global identifier. Tooling that takes a bare `phase:<slug>` argument must disambiguate across projects (or require a project hint).

Same observation applies to task slugs.

## Acceptance

- A reader scanning PROTOCOL.md / LAYOUT.md for slug semantics encounters this clarification.

## Test plan

None (docs-only).

## Non-goals

- Changing the protocol to globalize slugs.
- Adding any code-level enforcement.
- Updating skill docs in `~/.claude/skills/` (that's separate).
