**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-08
**Related**: dossier task `ci-cargo-audit` (id: `tsk_01KTJSHNQES9PTD0G69D5F2NVP`), [docs/follow-ups.md](../../follow-ups.md), `.github/workflows/ci.yml`

# CI: cargo-audit advisory scan on PRs — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| CI config (0×) | new `.github/workflows/audit.yml` (or a job in `ci.yml`) | ~35 | 0 |
| **Total** | | | **~0** |

Band: amazing (config-only; no weighted source).

## Goal

dossier has no dependency-advisory scan. `grep -riE "cargo-audit|cargo audit|cargo-deny|RUSTSEC" .github Makefile Cargo.toml` returns nothing; the four workflows (`ci.yml`, `coverage.yml`, `mutants.yml`, `claude.yml`) have no audit step. A `RUSTSEC` advisory against a transitive dep — the AWS SDK stack pulls a large tree — would land silently. Add an advisory scan so a vulnerable dep fails a check rather than slipping in unnoticed.

## Behavior / fix

Add a `cargo-audit` advisory scan to CI, running on PRs + push to `main`:

- Either a dedicated `.github/workflows/audit.yml` or a small job in `ci.yml`. A dedicated job keeps the audit's failure signal distinct from the build/test rollup; either is acceptable.
- Use the `rustsec/audit-check` action, or `cargo install cargo-audit && cargo audit -D warnings`.
- Fail on advisories.
- Provide a documented `--ignore RUSTSEC-YYYY-NNNN` escape hatch with a one-line comment per ignore explaining *why*, mirroring the repo's `#[allow]`-needs-justification posture (CLAUDE.md "Lint discipline").

Scope this to **advisory-only** (`cargo-audit`), not `cargo-deny`. Advisory-only is low-config and matches the operator's stance against version-churn tooling — this is a vuln scan, not a dependency-bump bot.

## Acceptance

- `grep -r "cargo audit\|audit-check" .github/workflows` hits.
- A clean dep tree passes the job; a known-vuln dep would fail it.
- The audit appears in the PR check rollup.

## Test plan

Open a PR (or `gh workflow run`) and confirm the audit job runs and is green against the current (clean) dep tree.

## Non-goals

- `cargo-deny`'s license / banned-source / duplicate checks (heavier, separate task).
- Remediating any advisory the scan surfaces — that's its own fix when one appears.
- A scheduled (cron) audit run — PR + push-to-main coverage is the v0 ask.
