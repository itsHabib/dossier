**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `windows-matrix-test-job` (id: `tsk_01KSE9ASHQS3WQG1AN0A93AMH7`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# CI: add Windows to the test job matrix — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `.github/workflows/ci.yml` (edit) | ~10 | 0 (config) |
| Tests | — | 0 | 0 |
| **Total** | | | **0 (config-only)** |

Band: **amazing**. Pure workflow YAML; no production-source or test deltas. Likely the smallest task in this phase.

## Goal

`.github/workflows/ci.yml`'s `test` job runs on `ubuntu-latest` only, but CLAUDE.md explicitly calls out Windows-specific gotchas (path separators `/` vs `\`, CRLF line endings, `Path::join` over manual `\n` splits, MSVC toolchain). A regression that only fails on Windows would slip past CI today. dossier is operator-developed primarily on Windows; CI green while local-Windows red is the exact "tests pass on Linux, fail on Windows" pattern the CLAUDE.md "When you're stuck" section warns about.

## Behavior / fix

Update the `test` job in `.github/workflows/ci.yml`:

```yaml
test:
  strategy:
    fail-fast: false
    matrix:
      os: [ubuntu-latest, windows-latest]
  runs-on: ${{ matrix.os }}
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
    - run: cargo test --all-features
```

- `fail-fast: false` so one OS failing doesn't cancel the other matrix entry.
- Leave `fmt` and `clippy` jobs on `ubuntu-latest` only (no Windows-specific lint output worth checking, and line-ending differences would create noise).
- `rust-toolchain@stable` installs MSVC by default on `windows-latest`, which is what dossier's `chrono` dep needs per the CLAUDE.md toolchain note.

## Acceptance

- Both matrix entries (`ubuntu-latest` + `windows-latest`) run `cargo test --all-features` and pass on the existing test suite.
- A Windows-only regression (e.g. hardcoded `/` in a path) would fail the windows-latest matrix entry while ubuntu stays green.

## Test plan

- PR opens, CI fires both matrix entries; both pass.
- Operator confirms on a fresh push that local `make test` on Windows still matches the windows-latest CI behavior (same stdlib, same toolchain).

## Non-goals

- macOS matrix entry (no operator dev box; no macOS-specific code paths in dossier).
- Windows in `fmt` / `clippy` (line-ending differences would cause noise; not worth the matrix overhead).
- Caching-strategy tuning for Windows (Swatinem/rust-cache handles it transparently).
- Adding Windows-specific test fixtures (existing tests should already be portable per CLAUDE.md conventions).
