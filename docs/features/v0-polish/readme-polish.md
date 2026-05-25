**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `readme-polish` (id: `tsk_01KSE9FEYHJY334TRN3DN2M6E5`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# docs: expand README with MCP usage example + dossier-state pattern — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `README.md` (additions) | ~40 | 0 (docs) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (docs-only)** |

Band: **amazing**. README stays under ~150 lines total (currently 94).

## Goal

`README.md` covers the quick-start (markdown corpus + MCP server) but a fresh reader has no "what does using this actually look like" example. The dogfood corpus convention (`~/pers/dossier-state/projects/<project>/` per `CLAUDE.md`) isn't mentioned outside CLAUDE.md, even though it's how dossier is actually used. Per-primitive docs (PROTOCOL.md / LAYOUT.md sections) aren't pointed at from the README — a reader has to bounce through 3 files to understand dossier in practice.

## Behavior / fix

Three additions, keeping the file under ~150 lines:

### 1. `## Example session` section (after Quick start)

Show a realistic Claude-Code-style invocation sequence:

```
# Create a project
mcp__dossier__project_create { slug: "wellness-ai", title: "Wellness AI", actor: "human:michael" }

# Add a phase + task
mcp__dossier__phase_add { project: "wellness-ai", slug: "hyrox-coach-mvp", title: "HYROX coach MVP", actor: "human:michael", owner: "human:michael" }
mcp__dossier__task_create { project: "wellness-ai", phase: "hyrox-coach-mvp", slug: "auth-flow", title: "auth flow design", actor: "human:michael" }

# Link a PR when it merges
mcp__dossier__artifact_link { project: "wellness-ai", task: "tsk_...", kind: "pr", ref: "https://github.com/.../pull/42", label: "PR #42 — auth flow", actor: "human:michael" }
```

One short paragraph framing: this is what an agent does without operator prompting once dossier is wired into the session.

### 2. Dossier-state corpus pattern (short subsection, ~5 lines)

Document the convention: each operator keeps their corpus at `~/pers/dossier-state/` (or wherever they pick — dossier doesn't care, just open whatever directory you point `--corpus` at). The in-repo `projects/` directory is a test fixture, not the real corpus. New users typically `mkdir -p ~/pers/dossier-state/.dossier` and point `dossier serve --corpus ~/pers/dossier-state` at it; everything else is created by the write verbs.

### 3. Per-primitive doc pointers (additions to existing "Docs" / "More" section)

Inline section-anchor links from the README to specific sections of PROTOCOL.md / LAYOUT.md so a reader looking for, say, "what does an artifact look like on disk" can one-click there. Suggested anchors: project / phase / task / artifact / task state machine / on-disk layout per primitive.

## Acceptance

- README has an `## Example session` block with realistic MCP verb invocations covering project / phase / task / artifact_link.
- README mentions the corpus location convention with a short note that it's per-operator.
- Inline section-anchor links from README → specific PROTOCOL.md / LAYOUT.md sections (not just file roots).
- `wc -l README.md` returns ≤ 150.

## Test plan

- `wc -l README.md` ≤ 150.
- Manual: a fresh reader can answer "what does dossier do?" and "how do I start using it?" from the README alone, without opening other files.
- All in-file markdown anchor links resolve correctly (verify by clicking through on the rendered GitHub README).

## Non-goals

- Re-writing the existing Quick start section.
- Adding install / package badges (no published crate yet).
- A FAQ section.
- Per-repo READMEs in `pers/` siblings (dossier-only here).
- Tutorials beyond the single example session.
- Linking to the `dev-workbench` MCP/skill ecosystem from the README (those are operator-portfolio context; dossier's README stays project-scoped).
