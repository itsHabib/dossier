**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `sort-determinism-secondary-key` (id: `tsk_01KRV8788KM9JCS9YDWD0H6WJ5`); originally from Claude review on [PR #16](https://github.com/itsHabib/dossier/pull/16#issuecomment-4471190657), round 2.

# Sort determinism — secondary id key — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/store.rs` (sort_tasks / sort_phases / sort_projects) | ~30 | 30 |
| Tests | `src/store.rs` test module | ~50 | 25 |
| **Total** | | | **~55** |

Band: amazing.

## Goal

`sort_tasks` / `sort_phases` / `sort_projects` use `sort_by_key` on a single timestamp. Rust's sort is stable, but the input order comes from `fs::read_dir`, which is not guaranteed across platforms or even runs on some Linux filesystems. Under `limit`, the same query can return different subsets on different machines when several rows share a timestamp.

## Behavior / fix

Add a secondary `id` key on every sort branch. ULIDs are time-sortable and unique, so they're a deterministic tiebreaker:

```rust
TaskOrderField::CreatedAt => out.sort_by(|a, b| {
    a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id))
}),
```

Apply to every `order_by` branch in all three sorters (`sort_tasks`, `sort_phases`, `sort_projects`).

## Acceptance

- Two rows with identical timestamps sort in a predictable order (smaller `id` first).
- A query like `task.list { order_by: "created_at", limit: 1 }` always returns the same row across runs on the same corpus.

## Test plan

- `sort_tasks_ties_break_on_id_asc` constructs two tasks with identical `created_at` and asserts the lower-`id` row sorts first.
- Mirror for `sort_phases` and `sort_projects`.

## Non-goals

- Changing the primary sort key options.
- Adding a `desc` semantics other than reverse of the combined key.
