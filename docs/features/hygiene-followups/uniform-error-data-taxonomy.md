**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `uniform-error-data-taxonomy` (id: `tsk_01KRSZG60JG3S0JF294AA3459V`), [docs/follow-ups.md](../../follow-ups.md)

# Uniform error-data taxonomy on MCP verbs — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (wide-blast — every handler) | ~80 | 80 |
| Tests | `src/server.rs` test module | ~60 | 30 |
| **Total** | | | **~110** |

Band: amazing.

## Goal

Every task / phase / project handler in `src/server.rs` routes user errors (unknown id, illegal state transition, empty actor) through `internal()`. MCP clients see them as server faults (`internal_error`, code -32603) rather than request validation errors (`invalid_params`, code -32602). The LLM can't tell whether to retry the call or ask the user to correct input.

## Behavior / fix

1. Add an `internal_or_invalid(err: anyhow::Error) -> ErrorData` helper in `src/server.rs`.
2. Inspect the error's chain / message for known user-error patterns: `"not found"`, `"invalid slug"`, `"invalid transition"`, `"empty actor"`, etc. (Pattern list extracted from the bodies of existing `bail!` calls in `src/store.rs`.)
3. Return `ErrorData::invalid_params(msg)` for user errors; `ErrorData::internal_error(msg)` for everything else.
4. Sweep every handler in `src/server.rs` — replace `.map_err(internal)?` with `.map_err(internal_or_invalid)?` (or thread through the appropriate call site).

## Acceptance

- `task.update` with an unknown ID returns `invalid_params` (-32602), not `internal_error` (-32603).
- `task.complete` on a `todo` task returns `invalid_params`.
- `project.update` with empty actor returns `invalid_params`.
- A genuinely internal error (disk I/O failure) still returns `internal_error`.

## Test plan

- One test per known user-error class confirms the response code.
- One test confirms a genuinely internal-looking error (e.g., explicit panic in a mocked path) still maps to `internal_error`.
- Existing handler tests remain green.

## Non-goals

- Restructuring the error type itself (still `anyhow::Error` underneath).
- Adding error codes beyond JSON-RPC's existing `invalid_params` / `internal_error`.
- Touching `internal()` callers outside `src/server.rs`.
