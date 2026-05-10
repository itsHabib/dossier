# Write side — design spec

**Status:** draft
**Owner:** @itsHabib
**Date:** 2026-05-10
**Related:** [PROTOCOL.md](../../../PROTOCOL.md), [LAYOUT.md](../../../LAYOUT.md)

## Scope

Estimated weighted LOC: ~750–900 across 4 PRs (see [PR breakdown](#pr-breakdown)). Each PR targets the < 700 ideal band; the largest (task verbs + state machine) bumps stretch.

## Goal

Make implementer agents — ship today, others tomorrow — capable of *driving* a project through dossier, not just reading it. The read side proves the wire works; the write side makes the protocol useful.

A successful write side means: a fresh agent (no prior knowledge of the corpus) can `project.create`, `phase.add`, `task.create`, `task.claim`, do work in the world, `task.update` with progress notes, `artifact.link` the resulting commit/PR, and `task.complete` — all through MCP, with the resulting corpus diffable in git and readable by a human.

## Verbs

The complete v0 write surface from PROTOCOL.md:

| Verb | Input | Output | Idempotent? |
| --- | --- | --- | --- |
| `project.create` | `{slug, title, description, actor}` | `Project` | no (slug-collision errors) |
| `project.update` | `{id, title?, description?, status?, actor}` | `Project` | yes (last-write-wins) |
| `phase.add` | `{project_id, title, body, after_phase_id?, actor}` | `Phase` | no |
| `phase.update` | `{id, title?, body?, status?, actor}` | `Phase` | yes |
| `task.create` | `{project_id, phase_id?, title, body, actor}` | `Task` | no |
| `task.claim` | `{id, actor}` | `Task` | no (rejects if already claimed by another actor) |
| `task.update` | `{id, body?, status?, note?, actor}` | `Task` | yes-ish (notes append) |
| `task.complete` | `{id, note?, actor}` | `Task` | no (rejects if not in a completable state) |
| `artifact.link` | `{project_id, task_id?, kind, ref, label, actor}` | `Artifact` | yes (treated as append; duplicates are duplicates) |

Every input takes `actor: String` to match PROTOCOL.md identity rules. The mesh stamps `created_at` / `updated_at` server-side.

## Atomic write primitives

A separate small module — `src/store/write.rs` — owns the durable write helpers so the verb implementations stay readable.

### File writes

Pattern: write to `<path>.tmp`, fsync, `fs::rename` to final path. `rename` is atomic on every supported filesystem (NTFS, APFS, ext4 / xfs / btrfs).

```rust
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
```

Subtleties to handle:
- Parent directory must exist; `create_dir_all` upfront.
- Don't follow symlinks at the destination — `rename` over a symlink replaces the symlink, not the target.
- Windows quirk: `rename` to an existing path requires `MOVEFILE_REPLACE_EXISTING` semantics. Rust's `fs::rename` does this on Windows since 1.5, so it's fine, but worth a comment.

### JSONL append (`artifacts.jsonl`)

Pattern: open with `O_APPEND`, take a file lock for the duration of the append, write the line, drop the lock. `O_APPEND` makes the kernel-level write atomic up to PIPE_BUF (4096 on Linux); the line is well below that. The lock guards against torn writes between *processes* that share a corpus (out of scope per LAYOUT.md, but cheap to add).

Use the `fs2` crate for cross-platform file locking. Add to `Cargo.toml`:

```toml
fs2 = "0.4"
```

Sketch:

```rust
fn append_artifact(path: &Path, line: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.lock_exclusive()?;
    writeln!(file, "{line}")?;
    file.unlock()?;
    Ok(())
}
```

Append-only, never rewrite. If a future feature needs to "delete" an artifact, the answer is a tombstone entry, not a rewrite.

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

Transitions table — implemented as a `match` against `(current, next)` returning `Result<(), TransitionError>`:

| from \ to | claimed | in_progress | blocked | done | cancelled |
| --- | --- | --- | --- | --- | --- |
| todo | ✅ via claim | — | — | — | ✅ |
| claimed | — | ✅ | — | — | ✅ |
| in_progress | — | — | ✅ | ✅ via complete | ✅ |
| blocked | — | ✅ | — | — | ✅ |
| done | — | — | — | — | — (terminal) |
| cancelled | — | — | — | — | — (terminal) |

`task.claim` is the only path into `claimed`. `task.complete` is the only path into `done`. `task.update` handles the rest, rejecting illegal transitions with a structured error.

**Why a runtime check, not type-state.** Rust's type-state pattern fits a builder where you control allocation. Tasks come from disk in whatever state they happen to be in — `Task<Todo>` doesn't model "I just deserialized a task whose status field happens to be Todo." A runtime enum with a transition guard is the honest shape. The compile-time win we *do* get is `TaskStatus` exhaustiveness — every consumer must handle every variant.

## ID generation

ULIDs server-side via the `ulid` crate. Type-prefixed:

```rust
fn new_id(prefix: &str) -> String {
    format!("{prefix}_{ulid}", ulid = Ulid::new())
}
```

Add to `Cargo.toml`:

```toml
ulid = "1"
```

Clients MAY supply their own ID; the mesh accepts any prefix-correct ULID and rejects the call if it collides with an existing record. v0 keeps the bar low — server-generated by default.

## Referential integrity

Cheap, local checks at write time. Not a foreign-key DB; just early errors so the on-disk corpus stays consistent.

- `project.create`: `slug` must not exist in `projects/`.
- `phase.add`: `project_id` must resolve to an existing project.
- `task.create`: `project_id` must exist; if `phase_id` is set, the phase must belong to that project.
- `task.claim/update/complete`: task must exist; for `claim`, `assignee` must currently be empty (or the same actor — re-claiming yourself is a no-op).
- `artifact.link`: `project_id` must exist; if `task_id` is set, task must exist and belong to the project.

Errors return `ErrorData::invalid_params` (MCP code -32602) with a clear message.

## Concurrency

The mesh runs as a single tokio process. The `MeshService` already wraps `FsStore` in `Arc`; for the write side, wrap the `FsStore` *internals* such that:

- Reads (`list_*`, `get_*`) take a shared snapshot — they can run unguarded against the filesystem.
- Writes (`*_create`, `*_claim`, `*_update`, `*_complete`, `*_link`) acquire a `tokio::sync::Mutex` on the mesh's `WriteLock` for the duration of the write.

This is enough for v0's "single-writer per corpus" assumption from LAYOUT.md and avoids races between concurrent write tool calls within one mesh process. Cross-process coordination remains out of scope.

```rust
pub struct MeshService {
    store: Arc<FsStore>,
    write_lock: Arc<Mutex<()>>,
}
```

## Out of scope (v0 of the write side)

Deliberately deferred. Each is a candidate follow-up once a real consumer needs it.

- **Idempotency / `request_id`.** PROTOCOL.md mentions it; v0 verbs do not implement it. Add when we actually see retries in the wild.
- **Cross-process / multi-mesh write coordination.** LAYOUT.md says "two meshes against the same corpus will eventually corrupt it." Solving requires a corpus-level lockfile; defer.
- **Subscriptions / change notifications.** No watchers, no streaming. Pollers re-read.
- **Transactions across multiple writes.** `task.create` + `artifact.link` is two calls. v0 doesn't atomic-bundle.
- **Bulk operations.** No `tasks.create_many`. One at a time is fine.
- **Slug rename / project rename.** Mutating `slug` would mean moving a directory; not worth the complexity at v0. Treat slug as immutable.

## Acceptance criteria

- All 9 write verbs registered on the MCP server, each with `Json<T>` outputs and `ErrorData` errors.
- State machine guarded — every illegal transition test returns `invalid_params`, every legal one succeeds.
- Atomic write helper used by every file mutation; never write directly to a final path.
- `artifacts.jsonl` append uses `O_APPEND` + file lock.
- Referential integrity checks for the cases above; corresponding unit tests for each error branch.
- One end-to-end integration test: create a fresh tempdir corpus, drive a project from `project.create` through `task.complete` + `artifact.link`, and verify the resulting on-disk tree matches expectation.
- The mesh's own dogfood corpus (`projects/dossier/`) gets a new task documenting "write side shipped" once implementation lands.
- `make check` green throughout.

## PR breakdown

Each is independently reviewable and `make check`-green on its own.

### PR A — atomic write primitives + project verbs

**~150 weighted LOC.** Lays the foundation. Subsequent PRs depend on this.

- `src/store/write.rs` (new): `write_atomic`, `append_jsonl`, `new_id`, `now_utc`, `WriteLock` type.
- `src/store.rs`: `FsStore::create_project`, `update_project`. Returns the persisted record.
- `src/server.rs`: `project.create`, `project.update` MCP tools.
- Tests: write helpers (round-trip, partial failure, lock semantics on platforms we care about); store CRUD round-trip; MCP smoke for the two verbs against a tempdir corpus.

### PR B — phase verbs

**~120 weighted LOC.**

- `FsStore::add_phase`, `update_phase`. `add_phase` handles ordering: `after_phase_id` inserts after; otherwise appends to end.
- MCP: `phase.add`, `phase.update`.
- Tests: ordering edge cases (`after_phase_id` not present, inserting at head), update preserves `created_at`.

### PR C — task verbs + state machine

**~250 weighted LOC.** The biggest. Bumps the *ideal* band; justified by tightly coupled state machine that doesn't split cleanly.

- `src/domain.rs`: `transition(current, next) -> Result<(), TransitionError>` — pure function over `TaskStatus`.
- `FsStore::create_task`, `claim_task`, `update_task`, `complete_task`. Each writes the task file atomically; `update_task` appends to the `## Notes` section if a note is supplied.
- Note append: read existing body, find or create `## Notes\n`, append a line, write back atomic. Documented as "non-streaming for v0; if note frequency becomes a hot path, switch to a sidecar `notes.jsonl` per task."
- MCP: `task.create`, `task.claim`, `task.update`, `task.complete`.
- Tests: every legal transition; every illegal transition returns `invalid_params`; claim collision (different actors); claim re-entry (same actor is a no-op); note append idempotency on parse.

### PR D — artifact verbs

**~80 weighted LOC.**

- `FsStore::link_artifact`. Append-only via `append_jsonl`.
- MCP: `artifact.link`. (Read-side `artifact.list` already exists.)
- Tests: append round-trip; concurrent appends within one process produce N distinct lines; unknown `kind` round-trips untouched.

## Testing strategy

- **Unit tests** colocated with the module (`mod tests` blocks). Lint allows already in place.
- **Integration test** under `tests/end_to_end.rs` (new): full happy-path drive of a fresh corpus.
- **Dogfood smoke** (manual, post-merge): point a local Claude Code at the new mesh, drive the dossier project's own corpus through `task.create` + `task.claim` + `task.complete`, verify the markdown looks right.

## Open questions

- Should `task.update` allow `assignee` change (re-assignment)? Leaning no for v0 — re-assignment is a workflow concern, not a state-machine one. Re-claim by another actor would require unclaim semantics which complicates things.
- Slug case-sensitivity? Filesystems differ (NTFS / APFS case-insensitive by default; ext4 case-sensitive). Lean: store lowercase, reject uppercase at create time. Document.
- Note timestamp format in markdown body — RFC3339 verbose vs short `2026-05-10 17:42`? Lean: RFC3339 for machine-readability; humans can scan it.
