**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `coverage-workflow-dispatch-job` (id: `tsk_01KSE9BMBCGDPCEK738TF5QCCK`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# CI: coverage reporting as workflow_dispatch job — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `.github/workflows/coverage.yml` (new) | ~60 | 0 (config) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (config-only)** |

Band: **amazing**. Pure workflow YAML; no production-source or test deltas.

## Goal

No coverage tooling configured anywhere — no `cargo-llvm-cov` / `tarpaulin` / reports. Hard to know if a fresh agent's tests are actually exercising new code. Same operator preference as mutation testing: **not on the per-PR critical path**, but a CI job that's easy to trigger on-demand + emits something an agent can address. Report-only — no thresholds in v0.

## Behavior / fix

Add `.github/workflows/coverage.yml`:

- **Trigger**: `workflow_dispatch` only (not scheduled — coverage drift is less time-sensitive than mutation drift; operator triggers when curious).
- **Job**: `runs-on: ubuntu-latest`, checkout, Rust stable + cache, install `cargo-llvm-cov` (prefer `taiki-e/install-action` for speed), run `cargo llvm-cov --all-features --lcov --output-path lcov.info`.
- **Output**:
  - Upload `lcov.info` as an artifact (retention 30 days).
  - Generate a one-page summary via `cargo llvm-cov report --summary-only` and post to `$GITHUB_STEP_SUMMARY` — total line/region coverage % + a per-file breakdown showing the bottom 5 files by line coverage.
  - **No coverage gates** — operator policy: v0 ships without thresholds; report-only.

## Acceptance

- `gh workflow run coverage.yml` triggers the job and produces an `lcov.info` artifact + a summary visible in the Actions tab.
- Summary shows total line coverage plus a sorted-ascending list of the 5 lowest-coverage files (per-file %, lines, missed lines).
- No PR is blocked by coverage; the workflow is informational only.

## Test plan

- Trigger via `gh workflow run coverage.yml` once after merge; download the artifact; confirm `lcov.info` parses (`genhtml lcov.info -o coverage-html` works locally).
- Confirm bottom-5 list surfaces files with predictably low coverage (e.g. `src/bin/dossier.rs`, possibly some less-exercised store helpers).

## Non-goals

- Coverage thresholds / gates (operator policy: aspirational only for v0; reconsider when public API stabilizes past v0).
- Per-PR coverage diff (operator policy: not on the critical path; the on-demand summary is enough for solo-dev).
- Codecov / Coveralls integration (artifact + summary are enough for solo-dev; external service adds setup overhead).
- HTML report upload (`lcov.info` is the canonical interchange format; HTML is local-only convenience).
- Excluding test fixtures / binary entry from the report (the per-file bottom-5 will surface them; that's fine signal).
