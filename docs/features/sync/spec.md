# init — design spec

**Status:** draft
**Owner:** @itsHabib
**Date:** 2026-05-10
**Related:** [docs/vision.md](../../vision.md), [LAYOUT.md](../../../LAYOUT.md)

## Scope

Estimated weighted LOC: ~250–350 in one PR. Bin-side new command +
import logic in the store layer + frontmatter parser. No new deps —
we already pull in `serde_yml`.

## Goal

Let a solo dev register an existing project in dossier without
hand-writing YAML or running ten verbs. `init` reads a conventional
layout under `docs/` and imports each file into the corpus
structure. Files can carry optional `dossier:` frontmatter to override
defaults — title, status, etc. — so a project in flight can be
imported with real state preserved.

## Principles

- **Convention over flags.** The user organizes docs in the ship-style
  `docs/features/<phase>/` layout once; `init` just reads it.
- **Files declare themselves when they need to.** Frontmatter is a
  per-file override, never the default. Most files need none.
- **Never parse markdown for structure.** Frontmatter is structured
  YAML; body is opaque text we never inspect.
- **State machine is the one thing import bypasses.** A task imported
  with `status: in_progress` lands in `in_progress` directly. The
  state machine guards user-facing writes; import is the user
  asserting "this is where things already are."

## Convention

```
<corpus>/
  docs/
    project.md                          # optional; → project description
    features/
      <phase-slug>/
        spec.md                         # → phase, slug = <phase-slug>
        tasks/                          # optional
          <task-slug>.md                # → task, anchored to <phase-slug>
```

Defaults derived from layout:

| Source path                                       | Becomes        | Slug                  | Title  | Body              |
| ------------------------------------------------- | -------------- | --------------------- | ------ | ----------------- |
| `docs/project.md`                                 | project body   | —                     | —      | file (frontmatter-stripped) |
| `docs/features/<x>/spec.md`                       | phase          | `<x>`                 | `<x>`  | file              |
| `docs/features/<x>/tasks/<y>.md`                  | task in phase `<x>` | `<y>`            | `<y>`  | file              |

Anything outside this convention is ignored. `init` doesn't search the
rest of the repo. Out-of-tree import is a future follow-up.

## Frontmatter (optional, per file)

A source file may begin with a YAML frontmatter block. dossier reads
the `dossier:` key; everything else (Jekyll, Hugo, whatever) is
ignored and stripped on the way in (body stored verbatim sans the
frontmatter block).

```yaml
---
dossier:
  kind: project | phase | task          # optional; defaults from path
  slug: my-slug                          # optional; default from filename/dir
  title: Custom title                    # optional; default = slug
  status: <enum>                         # optional; default depends on kind
  # task-only:
  phase: <phase-slug>                    # optional override; default = parent dir
  assignee: human:michael                # required if status ∈ {claimed,in_progress,blocked}
  # timestamps (RFC3339; all optional):
  created_at: 2026-05-10T15:00:00Z
  updated_at: 2026-05-10T15:30:00Z
  claimed_at: 2026-05-10T15:10:00Z       # tasks only
  completed_at: 2026-05-10T16:00:00Z     # required if status = done
---

# whatever heading you want

…body…
```

### `kind`
Defaults from path location. Frontmatter `kind` overrides the
default *for files dossier already found via the convention scan* —
e.g., a file at `docs/project.md` with `dossier: { kind: phase }`
gets imported as a phase. The discovery pass itself is convention-
only (init never scans for `dossier:` frontmatter outside the
canonical paths); importing a file from outside the convention is
not supported in v0. See [Path vs frontmatter conflicts](#path-vs-frontmatter-conflicts)
for the consistent-shape edge cases.

### `status` defaults

| kind    | default   | values                                                       |
| ------- | --------- | ------------------------------------------------------------ |
| project | `active`  | `planning` / `active` / `paused` / `done` / `abandoned`      |
| phase   | `pending` | `pending` / `active` / `done` / `skipped`                    |
| task    | `todo`    | `todo` / `claimed` / `in_progress` / `blocked` / `done` / `cancelled` |

### Required when `status` is non-default

- Task `status ∈ {claimed, in_progress, blocked}` → `assignee` required
- Task `status = done` → `completed_at` required (and implicitly `assignee` since it had to be claimed at some point — required too)
- Task `status = cancelled` → no extra requirements

Init errors with a clear message on missing fields rather than
inventing defaults.

### Error catalog for pre-read validation

- `--slug <a>` and `docs/project.md` frontmatter slug `<b>` disagree
- task `<slug>` references phase `<phase-slug>` which isn't in the import (create `docs/features/<phase-slug>/spec.md` or remove the `phase:` field)
- file `<path>`: dossier frontmatter status `<s>` requires `assignee` (missing)
- file `<path>`: dossier frontmatter status `done` requires `completed_at` (missing)
- file `<path>`: invalid kind `<k>` (expected project / phase / task)
- file `<path>`: invalid status `<s>` for kind `<k>`
- file `<path>`: slug `<s>` is not valid (lowercase ASCII / digits / `-` / `_`)

### Path vs frontmatter conflicts

A file at `docs/project.md` with `dossier: { kind: phase }` is honored
as a phase, not a project. The principle is "files declare themselves";
the path is only a default. Weird-shaped, but consistent.

## CLI

```
dossier-mesh init [--corpus <path>]
                  --slug <slug>
                  [--title <title>]
                  [--actor <name>]
```

That's it. No `--phases-from` / `--tasks-from` flags — the convention
drives discovery.

- `--corpus <path>` — directory to import. Default `.`.
- `--slug <slug>` — required. Project slug, validated by `is_valid_slug`.
- `--title <title>` — optional. Defaults to `--slug` value if neither
  the flag nor `docs/project.md` frontmatter provides one.
- `--actor <name>` — defaults to `human:$USER` from env. Errors if both
  unset.

## Behavior

1. Resolve `--corpus` to an absolute path; verify it's a directory.
2. Error if `<corpus>/.dossier/` already exists.
3. Validate `--slug`. Resolve actor.
4. **Pre-read pass** (no disk writes yet):
   - Read `docs/project.md` if present → split frontmatter from body.
   - Glob `docs/features/*/spec.md`, sorted by dir name → phase entries.
   - For each phase dir, glob `tasks/*.md` → task entries.
   - For every file, parse the YAML frontmatter (if any), strip from
     body, validate the `dossier:` block against the schema.
   - Resolve every default (slug from path, title from slug, etc.) and
     surface any validation errors (invalid slug, missing required
     field, unknown enum value, phase reference to nonexistent phase).
5. Create `.dossier/`, an empty `config.toml`, and a `.dossier/.gitignore`
   that lists `cache/` (per LAYOUT.md — runtime-only artifacts live there).
6. Call `FsStore::create_project` with the resolved project metadata.
   Project ULID is now known.
7. For each phase (in lex order), call `FsStore::add_phase` or
   `import_phase` (the latter when frontmatter status ≠ default).
   Each phase's ULID is captured into a `slug → phase_id` map.
8. For each task, resolve the parent-phase slug to its ULID via the map,
   then call `import_task` (module-private helper) with a fully-stamped
   `Task`. This is the **only** place the state machine is bypassed.
9. Print: `created project <id> in <corpus>` + counts.

### Slug conflict resolution

If `--slug foo` is supplied AND `docs/project.md` has frontmatter
`dossier: { slug: bar }`, init errors with a clear message. The CLI is
the more explicit signal so the rule is "they must agree" rather than
"one silently wins." Same for `--title`.

## Failure handling

The pre-read pass catches *all* validation errors before any disk
mutation, which is the common failure mode (typo in frontmatter,
unknown phase reference, etc.).

If a write fails mid-import (disk full, permissions changed), the
corpus is half-state. Recovery is **manual**: `rm -rf <corpus>/.dossier
<corpus>/projects/<slug>` and re-run. The error message says so.

## Frontmatter stripping

When reading a source file:

- If it starts with `---\n…\n---\n`, the frontmatter block is removed
  from the body before persisting.
- The body is stored verbatim after stripping (no transformation, no
  trimming beyond removing the frontmatter delimiter and the blank
  line immediately after).

This means a source file with both Jekyll and dossier frontmatter
sees both stripped (we only read what we need from `dossier:`, but the
whole block is removed). If the user wants their Jekyll metadata
preserved for round-tripping back, that's a future concern.

## What's NOT in init

- **Out-of-tree imports** (files outside `docs/`) — defer; user can
  call `phase.add` / `task.create` for those.
- **Artifact import** — `artifact.link` is already scriptable.
- **Markdown structure parsing** — no H1 extraction, no checkbox
  parsing, no link rewriting.
- **`sync` / re-derive** — `init` is cold-start only. If new docs
  appear later, the user calls `phase.add` / `task.create` once.
- **`--force` / re-init** — error on existing corpus, manual cleanup
  documented.
- **`order` frontmatter** — phase order is lex order over dir names;
  user prefixes with `01-` / `02-` if they care (same convention
  dossier already uses on disk).

## Implementation sketch

`src/store.rs`:
- `pub fn init_corpus(root: &Path) -> Result<()>` — creates `.dossier/`,
  empty `config.toml`, and `.dossier/.gitignore` listing `cache/`.
  Errors if `.dossier/` already exists.
- `pub(crate) fn import_task(&self, task: Task) -> Result<()>` —
  module-private helper that writes a fully-stamped task to disk,
  bypassing the state machine. Asserts structural validity at the top
  (status / assignee / claimed_at / completed_at must be coherent) so
  callers can't quietly write a corrupt task. Not exposed via MCP.
- `pub(crate) fn import_phase(&self, phase: Phase) -> Result<()>` —
  same pattern for phases when status ≠ default.
- A shared `read_with_frontmatter(path) -> (Option<DossierMeta>, String)`
  helper that returns the parsed `dossier:` block and the body
  with frontmatter stripped.

`src/bin/dossier-mesh.rs`:
- New `init` subcommand alongside `serve`.
- Walks the convention via `std::fs::read_dir`, builds a list of
  resolved entries, calls into the store.

`src/import.rs` (new, or inlined into `store.rs` if small):
- `DossierMeta` struct (the frontmatter shape).
- `resolve_entry(path, kind_default) -> ResolvedEntry` — applies
  defaults, validates, surfaces errors.

No new deps. `serde_yml` already pulled in.

## Acceptance criteria

- `dossier-mesh init --slug alpha` in a fresh tempdir with no `docs/`
  creates a project with empty description and zero phases / tasks.
- `docs/project.md` body is preserved verbatim as `project.description`.
- `docs/features/auth/spec.md` becomes phase `auth` with the file
  content as body.
- `docs/features/auth/tasks/x.md` becomes a task with slug `x`,
  anchored to phase `auth`.
- A task file with `dossier: { status: in_progress, assignee: ship }`
  lands on disk in that state without going through claim.
- A task file with `status: in_progress` but no `assignee` errors
  in the pre-read pass (no partial corpus on disk).
- Re-running `init` on the same corpus errors with the documented
  message and points at the cleanup command.

## Testing strategy

- **Unit tests** for `init_corpus` and `read_with_frontmatter`.
- **Unit tests** for the frontmatter schema validation (each error
  branch).
- **Integration test** under `tests/init_round_trip.rs`: build a
  tempdir with the full convention (project, phases with tasks, mix
  of statuses), invoke the binary, verify counts and a sampling of
  fields.

## PR breakdown

One PR. Realistically lands in the **stretch** band (700-1000 weighted
LOC) once the integration test and frontmatter validator error paths
are properly covered. Splitting into "init_corpus + scaffolding" then
"frontmatter import" looks tempting but the integration test that
proves the convention actually works needs everything together — so
splitting would land a half-finished verb behind a feature gate, which
is worse than one larger reviewable unit.

If review feels too dense, the natural cut is to land import
**without** frontmatter status preservation first (all imports start
at default status), then a follow-up that adds status / assignee /
timestamp honoring. The state-machine-bypass surface is what makes
this PR feel bigger than ~80-LOC PR D was.
