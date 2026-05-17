# Follow-ups

Non-blocking items spotted during review or implementation. Cleared
opportunistically. New entries go at the bottom; resolved entries get
deleted (commit history is the record).

## Write side

- [ ] **Slug validation on remaining entry points** — `is_valid_slug` is
  now enforced on every create path (`create_project`, `add_phase`,
  `create_task` covers project / phase / task slugs). Still missing on
  `update_project`, `update_phase`, and all read paths (`get_project`,
  `list_phases`, `list_tasks`, `list_artifacts`). A `project_dir(slug)`
  helper that validates-and-joins, applied everywhere a slug-derived
  path is built, closes the remaining gap.
- [ ] **Clean "project not found" on `update_project`** — currently
  surfaces `os error 2: The system cannot find the file specified`.
  Mirror the `add_phase` explicit-existence-check pattern.
- [ ] **Actor on update verbs** — `ProjectUpdateArgs.actor` /
  `PhaseUpdateArgs.actor` are accepted with `#[allow(dead_code)]` and
  discarded. Either drop them from the args (audit log is out of v0
  scope) or commit to a `last_updated_by` domain field. Lean drop.
- [ ] **`Phase.created_by` parity with `Project.created_by`** — Project
  carries `created_by`, Phase doesn't. LAYOUT.md's phase example omits
  it too. Add the field on both Phase and the LAYOUT example, drop the
  `let _ = args.actor;` ceremony in `add_phase`.
- [ ] **Frontmatter field-drift risk** — `ProjectFrontmatter` /
  `PhaseFrontmatter` are hand-maintained alongside the domain types.
  Adding a `Project` field won't fail to compile, it'll just silently
  not persist. Mitigation: an exhaustive round-trip test per type;
  optional `#[serde(flatten)]` marker.

## Post-PR C (from Claude review on #7)

- [ ] **Uniform error-data taxonomy on MCP verbs** — every task / phase
  / project handler in `src/server.rs` routes user errors (unknown id,
  illegal transition, empty actor) through `internal()`, so MCP clients
  see them as server faults rather than request validation errors. Pre-
  existing across all verbs; fix uniformly in one tidy-up PR (probably
  via an `internal_or_invalid(err)` helper that distinguishes).
- [ ] **`find_task_path` should bail on duplicate hits** — currently
  returns the first match and keeps walking; a `bail!` on the second
  matching ID is one line and turns a near-impossible ULID collision
  into an explicit error rather than silent misrouting.
- [ ] **`read_dogfood_corpus` doesn't lock in the body / `## Notes`
  split** — one extra assertion (`!task.body.contains("## Notes")`)
  would pin the new semantics introduced in PR C against regressions on
  the read side. Write side is already covered by
  `task_body_rejects_notes_heading`.
