**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-17
**Related**: dossier task `slug-validation-remaining-paths` (id: `tsk_01KRSZFQNBR0HNTV12D2D97MTH`), [docs/follow-ups.md](../../follow-ups.md)

# Slug validation on remaining entry points — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/store.rs` (wide-blast across update + read paths) | ~80 | 80 |
| Tests | `src/store.rs` test module | ~100 | 50 |
| **Total** | | | **~130** |

Band: amazing. Single PR.

## Goal

`is_valid_slug` is enforced on every create path (`create_project`, `add_phase`, `create_task`). It is NOT enforced on update paths (`update_project`, `update_phase`) or any read path (`get_project`, `list_phases`, `list_tasks`, `list_artifacts`). A malformed slug today reaches `path.join()` on those paths with no check — silent filesystem failure or a path-traversal-ish footgun.

## Behavior / fix

Add a helper in `src/store.rs`:

```rust
fn project_dir(&self, slug: &str) -> Result<PathBuf> {
    if !is_valid_slug(slug) {
        bail!("invalid slug: {slug}");
    }
    Ok(self.root.join("projects").join(slug))
}
```

Apply this helper everywhere a slug-derived path is built — every update path AND every read path. The existing create paths can adopt it too for consistency.

## Acceptance

- `update_project` with an invalid slug (e.g. `../etc/passwd`) returns a typed validation error, not an OS error.
- `update_phase` same shape.
- Each of `get_project`, `list_phases`, `list_tasks`, `list_artifacts` reject invalid slugs at the verb boundary with a typed error.

## Test plan

- One test per verb above asserting the validation rejection.
- Existing happy-path tests cover the validated case unchanged.

## Non-goals

- Changing the slug validation rules themselves (`is_valid_slug` stays).
- New error types beyond what `is_valid_slug` already produces.
- Auditing other path constructions outside `src/store.rs`.
