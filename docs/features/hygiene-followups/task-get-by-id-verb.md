**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `task-get-by-id-verb` (id: `tsk_01KRW29PGBCV2MDQ7KJTMJ27EJ`), [docs/follow-ups.md](../../follow-ups.md)

# Add task.get { id } verb — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/store.rs`, `src/server.rs`, `src/domain.rs` | ~60 | 60 |
| Tests | `src/store.rs` + `src/server.rs` test modules | ~50 | 25 |
| **Total** | | | **~85** |

Band: amazing.

## Goal

Dossier has no `task_get { id }` MCP verb. Tooling that wants to operate on a task ID without project context has to walk every project via `project_get` — N+1 dossier calls. Expose a one-call task lookup.

## Behavior / fix

1. Add `FsStore::get_task(id: &str) -> Result<Option<Task>>` that delegates to the existing `find_task_path` (which already walks the corpus internally with `bail!` on duplicate hits per PR #21), reads the file if found, parses, returns the `Task`.
2. Add `TaskGetArgs { id: String }` to `src/domain.rs`.
3. Register `task.get` tool in `src/server.rs` that wraps the store call and returns `Json<Task>` on hit, `invalid_params { task not found: {id} }` on miss, `invalid_params { invalid id format }` on malformed input.

## Acceptance

- `task.get { id: "tsk_..." }` returns the task in one MCP call, regardless of which project owns it.
- Missing id returns `invalid_params` with `"task not found: {id}"`.
- Malformed id (not matching `tsk_` + 26-char Crockford ULID) returns `invalid_params` upfront, before walking the corpus.

## Test plan

- `get_task_hits_across_projects` — task exists in project A, lookup by ID succeeds without naming the project.
- `get_task_errors_on_unknown_id`.
- `get_task_errors_on_malformed_id`.

## Non-goals

- Removing or deprecating `project_get`'s task bundle (callers still want all-tasks-for-a-project).
- Adding `task.delete` or other new task verbs.
- Caching the id→path map (every call walks; cheap at solo-dev sizes; matches the existing `find_task_path` cost model).
