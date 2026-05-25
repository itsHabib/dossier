**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `prune-shipped-followups` (id: `tsk_01KSE9D4VT116994WKF6JPFS90`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# docs: prune shipped items from docs/follow-ups.md — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `docs/follow-ups.md` (delete entries) | -30 to -45 | 0 (docs) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (docs-only)** |

Band: **amazing**. Pure deletion + commit-message provenance.

## Goal

`docs/follow-ups.md` lists 8 items under "Write side" + "Post-PR C". All 8 shipped this week. The file's own header says *"Cleared opportunistically. New entries go at the bottom; resolved entries get deleted (commit history is the record)."* Today the file lies about what's open. A reader landing on it gets a stale snapshot — drains trust in the doc.

## Behavior / fix

Delete the shipped entries; preserve the header (the file persists as the canonical landing spot for future entries).

Each deletion must be justified inline in the commit message by citing the shipping PR:

- "Slug validation on remaining entry points" → PR #29
- "Clean 'project not found' on `update_project`" → PR #33
- "Actor on update verbs" → PR #24
- "`Phase.created_by` parity with `Project.created_by`" → PR #31
- "Frontmatter field-drift risk" → PR #34
- "Uniform error-data taxonomy on MCP verbs" → PR #28
- "`find_task_path` should bail on duplicate hits" → covered by PR #28 (markers) + PR #36 (task.get refactor through `try_find_task_path`)
- "`read_dogfood_corpus` doesn't lock in the body / `## Notes` split" → covered by frontmatter-drift round-trip tests in PR #34

Confirm each PR actually merged via `git log --oneline --grep "(#28|#29|#31|#33|#34|#36)" main` before deleting the corresponding entry.

After pruning, the file likely shrinks to a 5–10 line header-only stub. That's correct — the convention is "delete when resolved."

## Acceptance

- `docs/follow-ups.md` contains only genuinely open items (probably zero today).
- The header convention paragraph ("Cleared opportunistically...") stays — the file remains structurally intact for future entries.
- A reader landing on `docs/follow-ups.md` sees an accurate snapshot.

## Test plan

- `grep -c "internal_or_invalid\|project_dir(slug)\|actor.*update verbs\|Phase\.created_by parity\|Frontmatter field-drift\|find_task_path should bail\|## Notes" docs/follow-ups.md` returns 0 (none of the shipped-marker phrases remain).
- The commit message enumerates each deletion with its shipping-PR justification.
- Manual: the remaining file (probably just the header) is well-formed markdown.

## Non-goals

- Adding new entries (this is purely a pruning pass).
- Migrating the file into dossier as tracked tasks (the file is the hand-edit landing spot; dossier tasks reference it but don't replace it).
- Renaming or restructuring the file headers.
- Adding a "Resolved (historical)" archive section — commit history IS the record per the file's own rule.
