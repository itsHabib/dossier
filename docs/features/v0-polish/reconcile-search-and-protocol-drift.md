**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `reconcile-search-and-protocol-drift` (id: `tsk_01KSE9EDSQZYMC625NVSMEZDGT`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# docs: reconcile search verb + bump PROTOCOL.md for task.get + search — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `docs/vision.md`, `docs/PROTOCOL.md` (edits) | ~40 | 0 (docs) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (docs-only)** |

Band: **amazing**. Two small surgical edits across two doc files.

## Goal

Two doc-drift items spotted in the 2026-05-24 audit:

1. **Vision drift**: `docs/vision.md` lists *"Search / RAG inside dossier"* in the explicit-NOT-building list, but `mcp__dossier__search` shipped. Vision is the canonical samurai-sword discipline anchor — when it drifts, scope creep becomes invisible.
2. **PROTOCOL.md drift**: Two MCP verbs shipped without protocol updates — `task.get { id }` (PR #36) and `search { query, kinds?, project?, limit? }`. A reader hitting PROTOCOL.md misses both.

This spec does NOT cover the existing open task `tsk_01KSBFEK42ES74ETY29B5V7EHR` (Phase + Artifact wire-name table fix) — that stays in its current phase as a separate concern.

## Behavior / fix

### `docs/vision.md`

Locate the "Explicitly NOT building (yet, possibly ever)" list. The line *"Search / RAG inside dossier. LLMs already do retrieval over MCP tool outputs — build the query engine in the LLM, not the store."* is now false. Replace with the **(a) document scope** option:

- Remove `search` from the NOT-building list.
- Add a short paragraph (preferably in the "What you have today" section or adjacent to it) describing what `search` does: literal-substring across project/phase/task titles + bodies, single-call ranked list, no semantic/vector — the LLM still does the heavy lifting (snippet → next action).
- Keep "RAG / vector / semantic search" on the NOT-building list explicitly — that's the scope discipline.

### `docs/PROTOCOL.md`

Add `task.get` and `search` to whichever table / section enumerates verbs. For each:

- **`task.get { id }`**: returns the single Task matching `id` (walks every project; no project slug required). Errors: malformed ULID → `invalid_params("invalid id format")`; well-formed-but-absent → `invalid_params("task not found: <id>")`. Match the row format of the other task verbs.
- **`search { query, kinds?, project?, limit? }`**: returns a ranked list of hits across project / phase / task titles + bodies. Args: `query` (non-empty literal substring, case-insensitive); `kinds` (subset of `["project", "phase", "task"]`, default all); `project` (filter to a single project slug; default all); `limit` (default 50, applied after sort). Each hit has `score` (overlapping literal match count in title+body), `snippet` (~80 chars centered on first match), `kind`, `project`, `slug` (and `id` for task hits).

Match the existing rows' shape exactly.

## Acceptance

- `grep -c "Search / RAG inside dossier" docs/vision.md` returns 0.
- `docs/vision.md` mentions `search` in the "what's shipped" framing.
- `docs/vision.md` still excludes "RAG / vector / semantic" from the surface explicitly.
- `docs/PROTOCOL.md` documents `task.get` and `search` alongside the other verbs in the same format.
- A reader hitting either doc sees an accurate description of the current MCP surface.

## Test plan

- `grep -c "Search / RAG inside dossier" docs/vision.md` returns 0.
- `grep -E "^\| .task\.get\b" docs/PROTOCOL.md` returns at least 1.
- `grep -E "^\| .search\b" docs/PROTOCOL.md` returns at least 1.
- Manual: read both docs end-to-end; flag anywhere search/task.get is implied but not stated.

## Non-goals

- The Phase + Artifact wire-name table fix (`tsk_01KSBFEK42ES74ETY29B5V7EHR` — separate, hygiene-followups phase).
- Documenting `search`'s ranking algorithm in detail (one-line summary + "literal substring match count" is enough).
- Adding example queries to PROTOCOL.md (lower-priority; can land later).
- Adding a separate `docs/search.md` design doc (vision + protocol updates are sufficient for v0).
- Updating `LAYOUT.md` (search doesn't change the on-disk format).
- Updating `CLAUDE.md` references to search (operator can hand-edit if needed; not blocking).
