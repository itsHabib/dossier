**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `mutants-workflow-dispatch-job` (id: `tsk_01KSE9A1N8P81HJF4W13KBTPY1`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# CI: mutation testing as workflow_dispatch job — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `.github/workflows/mutants.yml` (new) | ~80 | 0 (config) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (config-only)** |

Band: **amazing**. Pure workflow YAML; no production-source or test deltas.

## Goal

`cargo-mutants` is configured (`.cargo/mutants.toml`) with Makefile targets, but only runs locally — surviving mutations only surface when the operator remembers. Wire it as a CI job that's easy to trigger on-demand AND runs on a weekly cron so drift gets caught without operator action. Output must be agent-readable so a follow-up task can address surviving mutations.

## Behavior / fix

Add `.github/workflows/mutants.yml`:

- **Trigger**: `workflow_dispatch` (manual via UI / `gh workflow run`) + `schedule` (weekly cron, Sunday 06:00 UTC).
- **Job**: `runs-on: ubuntu-latest`, checkout, Rust stable + cache, install `cargo-mutants` (prefer `taiki-e/install-action` for speed; fall back to `cargo install cargo-mutants --locked`), run `make mutants` (full pass — on-demand cadence justifies it).
- **Output**:
  - Upload `mutants.out/` directory as an artifact (retention 30 days).
  - Generate a markdown summary from `mutants.out/outcomes.json` and post to `$GITHUB_STEP_SUMMARY` listing each surviving mutation: file, line, mutation kind, suggested test addition.
  - On scheduled cron runs (not on `workflow_dispatch`), if any mutations survived, **open or update a GitHub issue** labeled `mutation-drift` with the surviving list. Use `gh issue list --label mutation-drift --state open` to find an existing one; if present, post a comment with the new findings; otherwise create one.

## Acceptance

- `gh workflow run mutants.yml` triggers the job manually and produces an artifact + summary visible in the Actions tab.
- Scheduled run fires weekly without operator action.
- Surviving mutations are visible in the job summary and (on cron) raise / update a tracking issue.

## Test plan

- Trigger via `gh workflow run mutants.yml` once after merge; confirm artifact downloadable + summary present in the run page.
- Smoke: deliberately remove one test, push to a throwaway branch, manually trigger; confirm the dropped coverage shows up as a surviving mutation in the summary.
- Confirm cron parses: `gh api repos/itsHabib/dossier/actions/workflows/mutants.yml` returns the workflow with the schedule.

## Non-goals

- Gating PRs on mutant survival (operator policy: not a regression gate).
- Mutation-testing every PR diff (`mutants-quick` already exists for local use; CI doesn't need to repeat it).
- A separate badge / dashboard for mutation results.
- Auto-generating fix PRs for survived mutations.
- Caching the `mutants.out/` directory across runs (each run starts fresh).
