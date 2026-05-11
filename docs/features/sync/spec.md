# Sync (init) — design spec

**Status:** draft
**Owner:** @itsHabib
**Date:** 2026-05-10
**Related:** [docs/vision.md](../../vision.md), [LAYOUT.md](../../../LAYOUT.md)

## Scope

Estimated weighted LOC: ~150–200 in one PR. Bin-side new command + a thin scaffolding helper in the store layer.

## Goal

Make it trivial to start tracking a new project in dossier. Walk into a
random repo, run `dossier-mesh init`, and dossier scaffolds a corpus
from what it sees — the marker dir, a `project.md` populated from the
repo's README, and nothing else. The adoption unlock.

## v0 success criteria

- `dossier-mesh init` in a clean directory creates a usable corpus that
  passes `FsStore::open` and round-trips through `project.list` /
  `project.get`.
- `dossier-mesh init` in a directory that already has `.dossier/` fails
  loud with a clear "already a corpus" error — no destructive surprises.
- Subsequent writes via the existing verbs work against the scaffolded
  corpus end-to-end (verified by an integration test).

## Resolved decisions

Three open questions from the kickoff are answered below. Comment if
you want them changed before implementation.

- **Just `init`, no separate `sync`.** Cold-start is the real adoption
  pain; re-derive ("sync the corpus to match the current repo state")
  introduces correctness questions (what wins on conflict? what about
  user-edited frontmatter?) that don't pay off until we have multiple
  corpora drifting from their repos. Ship `init` now; revisit `sync`
  when there's a real reason.
- **Auto-populate `project.md` only.** Phases-from-existing-design-docs
  and artifacts-from-merged-PRs each carry their own gotchas (parsing
  arbitrary markdown for phase order, requiring a git remote and
  network for PR detection, deciding which PRs count). The smallest
  thing that solves "I don't want to hand-edit YAML to start" is
  populating `project.md` from the repo's README + current dir name.
- **CLI subcommand on `dossier-mesh`.** The MCP server can't bootstrap
  a non-existent corpus — its only addressing key is the corpus root,
  and the whole point of `init` is to create that root. A subcommand
  on the existing binary keeps deployment trivial (one binary, no new
  artifacts).

## Behavior

```
dossier-mesh init [--corpus <path>] [--slug <slug>] [--title <title>]
```

- `--corpus <path>` (default: `.`) — the directory to turn into a
  corpus. Must exist. Must be writable.
- `--slug <slug>` (default: lowercase ASCII derivation of the dir name,
  validated by `is_valid_slug`) — the project's slug.
- `--title <title>` (default: the first H1 in `<corpus>/README.md`, or
  the slug if no README is present) — the project's title.

The command:

1. Verifies `<corpus>` exists and is writable.
2. Errors if `<corpus>/.dossier/` already exists ("already a corpus").
3. Creates `<corpus>/.dossier/` and an empty `<corpus>/.dossier/config.toml`.
4. Derives `slug` from the dir name (lowercased, non-ASCII stripped,
   spaces / underscores → hyphens). Errors if the result isn't a valid
   slug after derivation (e.g. an all-numeric dir name with leading
   special chars).
5. Reads `<corpus>/README.md` if present; extracts the first H1 as the
   title (default if `--title` was not supplied) and the body (text
   after the first blank line following the H1) as the project
   description.
6. Calls `FsStore::open(<corpus>)` then `FsStore::create_project` with
   the derived fields and `actor = "dossier-mesh:init"`.
7. Prints the resulting project id + the path of the created
   `project.md` to stdout (one line each, `key=value` form, like the
   other CLI output).

Idempotency: re-running `init` on an existing corpus errors. No `--force`.

## Errors (loud, lowercased)

- `<corpus> does not exist`
- `<corpus> is not a directory`
- `<corpus> is already a dossier corpus (.dossier/ present)`
- `derived slug is not valid: "<slug>" — pass --slug explicitly`
- `--slug must be lowercase ascii (a-z, 0-9, -, _): <slug>`
- `<corpus>/README.md exists but has no H1; pass --title explicitly`

The last is a soft error — if README.md is *missing* entirely, the
title defaults to the slug. Only an existing README with no H1 is an
error, because that's a likely caller-bug shape (silent default would
hide it).

## Implementation sketch

`src/store.rs`:

- `pub fn init_corpus(root: &Path) -> Result<()>` — creates
  `.dossier/` + an empty `config.toml`. No-ops if the dir already
  exists (so `init_corpus` is itself idempotent at the FS level, even
  though the CLI guards against re-running).

`src/bin/dossier-mesh.rs`:

- New `init` subcommand alongside `serve`. Parses flags via the same
  hand-rolled loop pattern (`while let Some(arg) = iter.next() { ... }`).
- Pulls README extraction into a small helper (read first H1, read
  body-after-H1-and-blank-line).
- Constructs `NewProject` and calls `FsStore::create_project`.

No new dependencies. No regex; line-by-line parse on README.

## Acceptance criteria

- `dossier-mesh init` on a fresh tempdir creates a corpus that
  `FsStore::open` accepts.
- Round-trip: `dossier-mesh init` then `dossier-mesh serve` then
  `project.list` returns the created project.
- Re-running `init` on the same corpus errors.
- Slug derivation from common dir names (`my-project`, `My Project`,
  `MyProject_2`) produces valid slugs.
- README without H1 errors; README missing entirely falls back to
  slug-as-title.

## Out of scope (this PR)

- Phases auto-derived from `docs/features/<feature>/spec.md`.
- Artifacts auto-linked from recent merged PRs (would need `gh` /
  `git` shell-out + a remote).
- `sync` (re-derive). Defer until a concrete need surfaces.
- Bulk-import from existing markdown organization (Notion / Obsidian).

These are all candidates for future PRs once `init` is in use and we
have evidence of the next bottleneck.

## Testing strategy

- **Unit tests** for `init_corpus` (creates `.dossier/`, errors when
  already present).
- **Unit tests** for the README parsing helper (H1 extraction, body
  extraction, no-H1 case, no-README case).
- **Integration test** under `tests/init_round_trip.rs`: invoke the
  binary on a tempdir, verify the corpus opens, verify `list_projects`
  returns the seeded project.

## PR breakdown

One PR. ~150–200 weighted LOC. Within "ideal" band.
