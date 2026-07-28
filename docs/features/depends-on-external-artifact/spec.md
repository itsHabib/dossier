**Status**: draft
**Owner**: @codex
**Date**: 2026-07-28
**Related**: dossier task `depends-on-external-artifact` (id: `tsk_01KXEFMW06XGAGHX0WF1616MWZ`)

# External task blockers — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---:|---:|
| Domain + store + MCP/CLI surfaces | `src/domain.rs`, `src/store.rs`, `src/server/task.rs`, `src/bin/dossier.rs` | ~220 | 220 |
| Tests + protocol docs | focused server/store/property/CLI tests, `PROTOCOL.md`, `LAYOUT.md` | ~260 | 130 |
| **Total** | | | **~350** |

Band: **ideal**.

## Goal

Represent a task's external blockers as typed task data so both “what blocks this
task?” and “which tasks are blocked by this PR?” are answerable without parsing
notes or inventing proxy tasks.

## Decision

Add a separate `blocked_by: Vec<String>` task field. Keep `depends_on` task-only.
This preserves the existing task dependency graph and avoids turning every
consumer of `depends_on` into a tagged-union parser.

External blocker references use a small version-zero grammar:

- `pr:<owner>/<repo>#<positive-number>`
- `url:<absolute-https-url>`

Values are trimmed, unique, order-preserving, and validated at every write
boundary. Task IDs and arbitrary prose are rejected. Unknown reference schemes
fail closed; adding one later is an explicit protocol change.

## Behavior

- Persist `blocked_by` in task frontmatter, omitting it when empty. Missing fields
  deserialize to an empty list for backward compatibility.
- Return it from `task.get`, `task.list`, project hydration, the CLI, and MCP
  task records.
- Accept it on `task.create`; accept optional replacement on `task.update`, where
  omission leaves it unchanged and `[]` clears it.
- Add an exact-match `blocked_by` predicate to `task.list`. It composes with the
  existing filters and returns tasks containing that canonical external ref.
- Apply the same validation and round-trip behavior in every store backend and
  serialization path. Do not resolve or fetch the referenced system.
- Update `PROTOCOL.md`, `LAYOUT.md`, and tool descriptions/examples.

## Acceptance

- A task can be created with `blocked_by:
  ["pr:itsHabib/ship#203"]`; get/list/project/CLI/MCP reads return it unchanged.
- `task.list { blocked_by: "pr:itsHabib/ship#203" }` finds every live task carrying
  that blocker and composes with project/status filters.
- Update replaces or clears blockers without changing `depends_on`.
- Missing/empty fields remain backward compatible and are omitted on write.
- Duplicate, malformed, non-HTTPS, task-ID, and unknown-scheme references are
  rejected with a stable validation error.
- Existing `depends_on` behavior and task state transitions are unchanged.

## Test plan

- Table tests for the blocker-ref grammar and create/update validation.
- Store/frontmatter and property round trips for missing, empty, and populated
  `blocked_by`.
- Task-list exact-match and composed-filter tests.
- CLI and MCP schema/serialization coverage.
- Run `make check`.

## Non-goals

- Polling GitHub or automatically clearing a blocker when a PR merges.
- Adding external nodes to the task-to-task dependency DAG.
- Proxy tasks, artifact mutation, wildcard blocker searches, or URL dereferencing.

