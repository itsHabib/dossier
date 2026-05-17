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

## Mutation-testing baseline (PR B, #18)

First `make mutants` run against the proptest-enriched suite (#17):
261 mutants, 157 caught, **16 missed**, 88 unviable. Survival rate
~9.2% on viable mutations. **Zero** survivors in state-machine guards
or slug validation — the model-based proptest from #17 kills those
cleanly. All 16 survivors cluster in `server.rs` / `store.rs` around
filter and predicate code (mostly from #16 filter-expansion):

- [ ] **`project_get` doesn't verify scope of phases/tasks** —
  [src/server.rs:516, 520](../src/server.rs). Removing the `project`
  field from the `PhaseListFilter` / `TaskListFilter` constructions
  inside `project_get` doesn't fail any test. Add a `project_get`
  integration test with two projects + assertions that the returned
  record carries the *right* phases/tasks, not just any.
- [ ] **`artifact_list` boolean conditions untested** —
  [src/server.rs:750-751](../src/server.rs). `||` → `&&` and `==` →
  `!=` mutations on those two lines all survive (4 mutations). The
  predicate combining task-id and task-slug matching isn't exercised
  by tests that drive both branches.
- [ ] **Filter-matcher AND combinators untested** —
  [src/store.rs:1428 / :1454 / :1506](../src/store.rs) (`project_matches`,
  `phase_matches`, `task_matches`). `&&` → `||` in the predicate-
  conjunction logic survives because tests set one predicate at a time.
  Test combos: assignee + status, assignee + date range, body_contains
  + date range, etc.
- [ ] **Date-range boundary (`< → <=`) not tested** —
  [src/store.rs:1404](../src/store.rs) `in_range`. No test has a
  fixture row whose timestamp is exactly on the `_after` / `_before`
  boundary, so inclusive vs. exclusive semantics aren't pinned.
  Test should explicitly check inclusive-`_after` / exclusive-`_before`.
- [ ] **`From<*ListArgs> for *ListFilter` impls untested** —
  [src/server.rs:225, 241](../src/server.rs). Replacing the body with
  `Default::default()` survives — the conversion from MCP args to
  filter is exercised only on the happy path. Either add a server-
  layer round-trip test or accept this as a defensive layer with a
  one-line conversion that doesn't merit its own test.
- [ ] **`task_status_str` output text not asserted** —
  [src/store.rs:1072](../src/store.rs). Returning `""` or `"xyzzy"`
  from this function passes all tests. Behaviour is correct; tests
  just check `.is_err()` without inspecting the error message. Low
  impact — flag for awareness; tighten only when an error-format
  contract gets locked in.
- [ ] **`update_phase` `||` → `&&` mutation survives** —
  [src/store.rs:643](../src/store.rs). One branch in update_phase
  isn't exercised. Worth a focused test once we look at the file.
- [ ] **`split_task_body` `!` deletion survives** —
  [src/store.rs:354](../src/store.rs). The whitespace-trim guard at
  the head of the notes section. Add a test with leading blank notes
  lines and assert they're trimmed.

Re-run baseline after addressing any of the above: `make mutants`
(~40 min wall-clock on this machine, 261 mutants).
