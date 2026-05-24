**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-24
**Related**: dossier phase `reporting-metadata` (id: `phs_01KSDVPXZGW6ZDA33KQZAMK9K8`); tasks `phase-owner-field` (`tsk_01KSDVQST4YP73CYV9K7G7GZ8G`) and `task-depends-on-field` (`tsk_01KSDVRTF2Q817DXBFZW13VKP9`).

# Reporting-readiness metadata — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/domain.rs`, `src/store.rs`, `src/server.rs`, `LAYOUT.md` | ~120 | 120 |
| Tests | `src/store.rs` test module + `tests/proptest_frontmatter_roundtrip.rs` | ~80 | 40 |
| **Total** | | | **~160** |

Band: **amazing** (< 500 weighted).

## Goal

Give every primitive in the corpus the metadata an LLM needs to do useful cross-corpus reporting at org-scale, without bloating dossier toward Jira-tracker shape. Two pure-frontmatter additions:

1. **`Phase.owner`** — current responsible party for a phase. Mirrors `Task.assignee` (existing) and complements `Phase.created_by` (origin actor, shipped in PR #31). Closes the gap where an LLM can't answer *"what phases is the frontend team responsible for"* without convention work in phase titles.

2. **`Task.depends_on`** — first-class dependency edge between tasks. Pure metadata; no referential-integrity enforcement, no cycle detection, no cascade behavior. Closes the gap where an LLM can't topologically sort the open-work list or answer *"what's blocked across the frontend team's projects"*.

Both additions are vision-aligned: they're frontmatter fields, not new query primitives or enforcement. Reporting is the LLM's job; the store gives it the right raw material.

## Behavior — Phase.owner

### Domain (`src/domain.rs`)

Add `owner: String` to `Phase` (no struct-level default; field is required at creation time):

```rust
pub struct Phase {
    pub id: String,
    pub project: String,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub order: i32,
    pub status: PhaseStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub created_by: String,
    pub owner: String, // NEW — current responsible party; see actor-string convention below
}
```

### Frontmatter (`src/store.rs`)

Add `owner` to `PhaseFrontmatter` with `#[serde(default)]` so legacy on-disk phases (pre-this-PR) read as `owner: ""` without breaking:

```rust
#[serde(default)]
pub owner: String,
```

Do NOT add `#[serde(skip_serializing_if = "String::is_empty")]` — for `owner`, new phases always have a non-empty value (guard at create time), and legacy reads materialize as `""`. Serializing the empty string back to disk on a legacy phase is fine; round-trip stays stable.

### Args + verbs

- **`NewPhase`** — add `owner: String` (required). Plumb through `add_phase`. After the existing actor / project / slug guards, add:
  ```rust
  if args.owner.is_empty() {
      bail!("owner is required to add a phase");
  }
  ```
- **`UpdatePhase`** — add `owner: Option<String>`. In `phase.update`: when `Some(s)`, validate `!s.is_empty()` (bail `"owner must not be empty"` if violated) then replace; when `None`, leave the existing value unchanged.

### MCP server (`src/server.rs`)

Mirror to `PhaseAddArgs` and `PhaseUpdateArgs` (the rmcp-facing structs).

### Docs (`LAYOUT.md`)

Update the phase frontmatter example to show `owner: human:mh` alongside the existing `created_by: human:mh` line.

### Tests

In `src/store.rs` test module:

- `add_phase_persists_owner` — owner round-trips on disk and through `list_phases`.
- `add_phase_rejects_empty_owner` — error contains `"owner is required"`.
- `update_phase_replaces_owner` — `phase.update { owner: Some("team:platform") }` overwrites the value.
- `update_phase_rejects_empty_owner` — `phase.update { owner: Some("") }` errors.
- `update_phase_leaves_owner_when_none` — `phase.update { owner: None, title: Some("...") }` updates title, leaves owner unchanged.
- `read_phase_with_missing_owner_defaults_empty` — write a phase frontmatter without the `owner` field, read it back, assert `owner == ""`. (Same backwards-compat pattern as `created_by` had.)

In `tests/proptest_frontmatter_roundtrip.rs`:

- Extend `phase_round_trip` to assert `owner` survives serialize → deserialize.

## Behavior — Task.depends_on

### Domain (`src/domain.rs`)

Add `depends_on: Vec<String>` to `Task` with a struct-level default of `vec![]` (achieved via `#[serde(default)]` in the frontmatter wrapper; the domain struct just needs the field):

```rust
pub struct Task {
    // ... existing fields ...
    pub depends_on: Vec<String>, // NEW — list of task slugs or IDs this task depends on
}
```

### Frontmatter (`src/store.rs`)

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub depends_on: Vec<String>,
```

- `default` → legacy on-disk tasks read as `depends_on: []`.
- `skip_serializing_if = "Vec::is_empty"` → tasks with no deps don't carry the field in their frontmatter (keeps the on-disk format uncluttered for the common case).

### Args + verbs

- **`NewTask`** — add `depends_on: Vec<String>` with default `vec![]`. Plumb through `task.create`. No validation: store the strings as-is. (Per spec out-of-scope: no referential-integrity enforcement.)
- **`TaskUpdate`** — add `depends_on: Option<Vec<String>>`. In `task.update`:
  - `None` → leave the existing list unchanged
  - `Some([])` → clear the list (writes empty Vec; on disk the field disappears because of `skip_serializing_if`)
  - `Some(vs)` → replace with `vs` (no append semantics — matches how `body` and `status` updates work)

### MCP server (`src/server.rs`)

Mirror to `TaskCreateArgs` and `TaskUpdateArgs`. For `TaskCreateArgs.depends_on`, use `#[serde(default)]` so MCP clients can omit the field; default is `vec![]`.

### Docs (`LAYOUT.md`)

Update the task frontmatter example to show an example list (commented as optional):

```yaml
depends_on: [tsk_01KSDVQST4YP73CYV9K7G7GZ8G]   # optional; empty / absent means no deps
```

### Tests

In `src/store.rs` test module:

- `task_create_persists_depends_on` — list round-trips through `task.create` → list_tasks.
- `task_create_defaults_depends_on_empty` — omit the field, assert `depends_on == []`.
- `task_update_replaces_depends_on` — `Some(["tsk_FOO"])` overwrites whatever was there.
- `task_update_clears_depends_on_with_empty_list` — `Some([])` clears the list.
- `task_update_leaves_depends_on_when_none` — `None` leaves the existing list unchanged.
- `read_task_with_missing_depends_on_defaults_empty` — backwards-compat (write frontmatter without the field; read back; assert empty).
- `task_with_empty_depends_on_omits_field_on_write` — confirm `skip_serializing_if` actually skips. (Read a freshly-written task's `.md` file as raw string; assert it does NOT contain `depends_on:`.)

In `tests/proptest_frontmatter_roundtrip.rs`:

- Extend `task_round_trip` to include `depends_on` with both empty and non-empty variants.

## Acceptance

- `make check` green on CI (fmt + clippy + test).
- All new unit tests above pass.
- Proptest extensions pass (256 default cases; no shrinkage to a failing seed).
- `LAYOUT.md` examples render the new fields.
- Dossier corpus is forwards-compatible: a fresh checkout of this PR against the existing `~/pers/dossier-state/` corpus (which has legacy phases + tasks without these fields) reads cleanly.

## Test plan

Beyond the unit tests above, the implementer should run one manual smoke:

```sh
# in the worktree after impl:
cargo run -- serve --corpus ~/pers/dossier-state &
# then in another shell:
# mcp__dossier__phase_list { project: "dossier" } → all phases load with owner: "" for the legacy ones
# mcp__dossier__task_list { project: "dossier" } → all tasks load with depends_on: [] for the legacy ones
```

If the legacy-corpus read fails, the `#[serde(default)]` markers are missing or wrong.

## Non-goals

Explicit. Don't add even if tempted:

- Validating the `owner` string shape (any string accepted, matching `assignee`).
- Validating that `depends_on` IDs/slugs exist (vision: explicitly NOT building conflict detection).
- Detecting cycles in the `depends_on` graph (same — pure metadata).
- Cascading owner changes to child tasks (`Task.assignee` and `Phase.owner` stay independent).
- A `priority` field on tasks (operator policy: Jira creep; rejected).
- A `due_at` / `due_date` field on anything (deferred until a real workflow asks for it).
- A `team` primitive (`owner: "team:frontend"` uses the existing actor-string convention).
- A reverse-edge `blocks` field on tasks (derive at query time from `depends_on` when needed).
- Filter-side support like `task.list { blocked_by: "tsk_X" }` (separate concern — belongs in the `query-surface` phase if it lands).

## Open questions

- **Does `owner` default to `created_by` at `add_phase` time when omitted?** No — keep them independent. The caller passes both; that's intentional. If `add_phase` defaulted owner to actor, future ownership transfers would lose the original-creator signal.
- **Should `depends_on` accept both task IDs and task slugs?** Yes — the field is `Vec<String>`, opaque to the store. The LLM resolves at query time using `task.get` (by ID) or `task.list { slug_eq: "..." }` (slug, when that filter lands).
