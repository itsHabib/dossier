# init — parked

**Status:** parked
**Date last touched:** 2026-05-10

A `dossier init` command that scaffolds a corpus from an existing
repo, so a user doesn't have to hand-create `.dossier/` and `project.md`
the first time. We drafted a design and then parked it.

## Why parked

Because we don't need it yet. The write verbs (`project.create`,
`phase.add`, `task.create`, `artifact.link`) scaffold the directory
tree as they're called, so the minimum bootstrap a user needs is:

```sh
mkdir -p <corpus>/.dossier
```

Then a Claude Code session with dossier registered as MCP can drive
the rest. Until that one mkdir feels like enough friction to warrant
code, init is over-design.

## What we'd want, when we revisit

The most useful shape from the design pass before we parked it:

- **Convention over flags.** Ship's `docs/features/<slug>/spec.md` +
  `docs/features/<slug>/tasks/<slug>.md` layout. The user organizes
  docs once; init reads them.
- **Per-file frontmatter as escape hatch.** A `dossier:` YAML block on
  any file overrides the default kind / title / status / assignee /
  timestamps. Most files don't need it; tasks-in-flight do (to
  preserve real state on import).
- **State machine bypassed only at import**, via `pub(crate)`-scoped
  `import_task` / `import_phase` helpers with structural-validity
  asserts. Not exposed via MCP.
- **No filesystem search outside the convention.** No markdown
  parsing for structure. No README extraction. Files declare
  themselves; dossier doesn't infer.

## Revisit when

- We've used dossier across 3+ projects and the manual mkdir + initial
  `project.create` call has become annoying enough to count.
- We have evidence of what shape the input docs actually take in real
  repos (vs. the ship-style guess).
- The state-machine-bypass surface feels worth its own review — and
  not before.

Until then: ignore this file. Use the verbs.
