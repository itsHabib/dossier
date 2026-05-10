# Write side — design spec

**Status:** draft
**Owner:** @itsHabib
**Date:** 2026-05-10
**Related:** [PROTOCOL.md](../../../PROTOCOL.md) (data model), [LAYOUT.md](../../../LAYOUT.md) (storage)

## Scope

Estimated weighted LOC: ~750–950 across 5 PRs (see [PR breakdown](#pr-breakdown)). Largest PR (task verbs + state machine) bumps stretch.

## Goal

Make dossier durable enough to track real project state, queryable enough to give our agents the context they need, and aware enough to flag conflicts before they cause double-work.

This is a tool we run for our own agents. Not a reference implementation a third party will conform to — that framing is premature for v0 and biases the design toward things that don't matter yet (request-id idempotency, strict unknown-field rejection, conformance language). The verbs and data model are real, but they exist to make dossier useful to us, not abstractly reusable.

## v0 success criteria

- Drive a project end-to-end through our agents: create the project, lay out phases, create tasks, claim them, post progress, complete them, link the resulting commits/PRs.
- Ask "what's open right now?", "what does this task expect?", "what context do I need to start phase 03?" and get useful answers.
- Surface conflicts before they hurt: same-assignee multi-claim, slug/title overlap with an open task, stale claims sitting too long without progress.

## Verbs

| Verb | Input | Output | Notes |
| --- | --- | --- | --- |
| `project.create` | `{slug, title, description, actor}` | `Project` | Errors on slug collision. |
| `project.update` | `{id, title?, description?, status?, actor}` | `Project` | Last-write-wins per field. |
| `phase.add` | `{project_id, title, body, after_phase_id?, actor}` | `Phase` | `after_phase_id` inserts in order; default appends. |
| `phase.update` | `{id, title?, body?, status?, actor}` | `Phase` | |
| `task.create` | `{project_id, phase_id?, title, body, actor}` | `Task + warnings[]` | Warnings include any conflicts. |
| `task.claim` | `{id, actor}` | `Task + warnings[]` | Errors if already claimed by a different actor. |
| `task.update` | `{id, body?, status?, note?, actor}` | `Task` | `note` appends to the task's progress log. |
| `task.complete` | `{id, note?, actor}` | `Task` | Errors if not in a completable state. |
| `artifact.link` | `{project_id, task_id?, kind, ref, label, actor}` | `Artifact` | Append-only. |
| `conflicts.list` | `{project_id?}` | `Vec<Conflict>` | Read-only query (read side, not write). |

`actor` is server-stamped onto provenance fields. Server generates ULIDs and stamps `created_at` / `updated_at`.

## Atomic write primitives

A small module — `src/store/write.rs` — owns the durable write helpers.

- **Files**: `.tmp` + `fs::rename` to final path. `create_dir_all` parent first. `fs::rename` is atomic on every supported FS (NTFS, APFS, ext4/xfs/btrfs) and does the right thing on Windows since 1.5.
- **`artifacts.jsonl`**: open with `O_APPEND`, take an exclusive file lock for the duration, write the line, drop the lock. Use the `fs2` crate (cross-platform `flock` / `LockFileEx`). Append-only forever; deletes are tombstone entries if we ever need them.

## Task state machine

```
                  ┌─────────────────────────┐
                  ▼                         │
todo ─claim─▶ claimed ─update(in_progress)─▶ in_progress ─complete─▶ done
  │              │                              │  ▲
  │              │                              ▼  │
  │              └──update(cancelled)─────▶ cancelled
  │                                            ▲
  │                                            │
  └─update(cancelled)──────────────────────────┘
```

| from \ to | claimed | in_progress | blocked | done | cancelled |
| --- | --- | --- | --- | --- | --- |
| todo | ✅ via claim | — | — | — | ✅ |
| claimed | — | ✅ | — | — | ✅ |
| in_progress | — | — | ✅ | ✅ via complete | ✅ |
| blocked | — | ✅ | — | — | ✅ |
| done | — | — | — | — | — (terminal) |
| cancelled | — | — | — | — | — (terminal) |

Implemented as a `match` over `(current, next)` returning `Result<(), TransitionError>`. Runtime guard, not type-state — tasks come from disk in arbitrary states; `Task<Todo>` doesn't honestly model "I just deserialized whatever". `TaskStatus` exhaustiveness still gives compile-time wins everywhere we match it.

## Conflict detection

The piece that makes dossier more than CRUD. Three checks in v0; more land when we hit them.

| Conflict | When it fires | How |
| --- | --- | --- |
| Same-assignee multi-claim | One actor holds > 1 task in `claimed` or `in_progress` | Group open tasks by assignee, flag groups of size > 1 |
| Slug/title overlap | New task's slug or title is suspiciously close to an open task | Levenshtein distance ≤ 3 on slug, or substring containment on lowercased title |
| Stale claim | A task is `claimed` or `in_progress` but its newest progress note (or `claimed_at` if no notes) is > 7 days old | Compare timestamps to `Utc::now()` |

How they surface:

- **Inline warnings** — `task.create` and `task.claim` return their result *plus* `warnings: Vec<Conflict>`. The action still succeeds; the warning is informational.
- **Query tool** — `conflicts.list { project_id? }` for explicit "what's currently messy?" lookups.

```rust
pub struct Conflict {
    pub kind: ConflictKind,         // SameAssigneeMultiClaim | SlugSimilarity | StaleClaim
    pub primary: String,            // task ID (or new task slug for SlugSimilarity)
    pub related: Vec<String>,       // other task IDs involved
    pub detail: String,             // human-readable explanation
}
```

Detection runs server-side in-memory at the moment of the verb call. Cheap at v0 corpus sizes.

**Not in v0**: file-level overlap (would need tasks to declare files they touch), cross-project conflicts, semantic similarity beyond string distance.

## ID generation

ULIDs server-side via `ulid = "1"`. Type-prefixed: `prj_`, `phs_`, `tsk_`, `art_`. Generated at write time.

## Referential integrity

Cheap, local checks at write time. Errors return `ErrorData::invalid_params` with a clear message.

- `project.create`: slug must not exist in `projects/`.
- `phase.add`: `project_id` must resolve.
- `task.create`: `project_id` must exist; if `phase_id` is set, the phase must belong to that project.
- `task.claim/update/complete`: task must exist; for `claim`, `assignee` must currently be empty (or the same actor — re-claim is a no-op).
- `artifact.link`: `project_id` must exist; if `task_id` is set, task must exist and belong to the project.

## Concurrency

Single tokio process. Wrap the write path in `tokio::sync::Mutex`; reads stay unguarded.

```rust
pub struct MeshService {
    store: Arc<FsStore>,
    write_lock: Arc<Mutex<()>>,
}
```

Single-writer per corpus. Cross-process coordination is out of scope.

## Out of scope (v0)

Each is a follow-up if and when it actually matters.

- **Multi-implementer concerns** — `request_id` idempotency, strict unknown-field rejection, protocol-version negotiation. Skipped because v0 isn't a third-party-conforming server.
- **Cross-process write coordination** — corpus-level lockfile.
- **Subscriptions / change notifications** — no watchers, no streaming. Pollers re-read.
- **Multi-write transactions** — `task.create` + `artifact.link` is two calls.
- **Bulk operations** — no `tasks.create_many`.
- **Slug rename** — slug is immutable in v0 (would mean moving a directory).
- **File-level conflict detection** — needs declared file metadata; defer.

## Acceptance criteria

- 9 write verbs + 1 read verb (`conflicts.list`) registered with `Json<T>` outputs and `ErrorData` errors.
- State machine guarded; every illegal transition returns `invalid_params`.
- Atomic write helper used by every file mutation; no direct writes to final paths.
- `artifacts.jsonl` append uses `O_APPEND` + file lock.
- Referential integrity checks cover the cases above; tests for each error branch.
- Conflict detection in place: 3 checks, inline warnings on `task.create` / `task.claim`, plus the `conflicts.list` query.
- One end-to-end integration test driving a fresh tempdir corpus from `project.create` through `task.complete` + `artifact.link`.
- The dogfood corpus gets a "writes shipped" task once implementation lands.
- `make check` green throughout.

## PR breakdown

5 PRs, each independently reviewable and `make check`-green.

| # | Title | LOC | Notes |
| --- | --- | --- | --- |
| A | Atomic write primitives + project verbs | ~150 | Foundation; subsequent PRs depend on this. |
| B | Phase verbs | ~120 | Includes ordering edge cases (`after_phase_id` insertion). |
| C | Task verbs + state machine | ~250 | Bumps stretch; tightly coupled, doesn't split cleanly. |
| D | Artifact verbs | ~80 | Append-only via the helper. |
| E | Conflict detection | ~150 | Stacks on D so real data is in the corpus to test against. |

## Testing strategy

- **Unit tests** colocated with the module (`mod tests` blocks; lint allows in place).
- **Integration test** under `tests/end_to_end.rs`: full happy-path drive of a fresh corpus.
- **Conflict tests** under `tests/conflicts.rs`: fixture corpora that trigger each conflict kind.
- **Dogfood smoke** per PR: point a Claude Code at the new mesh and drive the dossier project's own corpus.

## Open questions

- `task.update` allow re-assignment? Lean **no** for v0 — re-assignment is workflow, not state-machine; would need explicit unclaim semantics.
- Slug case-sensitivity? Filesystems differ. Lean **lowercase only**; reject uppercase at create time.
- Note timestamp format in the markdown body — RFC3339 verbose vs short `2026-05-10 17:42`? Lean **RFC3339** for machine-readability.
- Conflict thresholds — stale = 7 days, slug similarity = Levenshtein ≤ 3 — reasonable starts; tune from real usage.
