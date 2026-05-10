# Follow-ups

Non-blocking items spotted during review or implementation. Cleared
opportunistically. New entries go at the bottom; resolved entries get
deleted (commit history is the record).

## Write side

- [ ] **Slug validation on every entry point** — `is_valid_slug` is
  enforced in `create_project` / `add_phase` but not on `update_project`,
  `update_phase`, or any read path. A `root.join("projects").join("../...")`
  PoC reads outside the corpus root on Windows. Low severity for the
  tool-for-ourselves threat model, trivial fix. Add a `project_dir(slug)`
  helper that validates-and-joins, used everywhere a slug-derived path
  is built.
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

## Docs

- [ ] **CLAUDE.md / PROTOCOL.md reframe drift** — both still lead with
  "Agent Project Protocol — wire spec for any agent". The write-side
  spec already reframed dossier as "a tool we run for our own agents,
  not a reference implementation". Reconcile.
