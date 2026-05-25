**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-25
**Related**: dossier task `doc-comment-audit-public-surface` (id: `tsk_01KSE9GD1VPY08WJHRCXV61X9Y`); phase `v0-polish` (id: `phs_01KSE997QX8153N72D0HMZ1WJN`).

# code-docs: every pub fn/struct in store/server/domain has a doc comment — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/store.rs`, `src/server.rs`, `src/domain.rs` (doc-comment additions only) | ~200 | 200 |
| Tests | — | 0 | 0 |
| **Total** | | | **~200** |

Band: **amazing**. Comment-only diff; no behavioral change.

**Calibration note**: estimate is loose because actual count depends on how many `pub fn` / `pub struct` / `pub enum` / `pub trait` declarations lack docs today — could be as low as ~80 weighted LOC (mostly server.rs verb handlers already documented) or as high as ~300 (lots of store.rs helpers).

## Goal

Current doc-comment density (audit 2026-05-24):
- `src/domain.rs` — 35 doc / 295 lines (~12%) — good
- `src/server.rs` — 137 doc / 1730 lines (~8%) — good
- `src/store.rs` — 216 doc / 5415 lines (~4%) — soft spot (big file; some `pub fn` helpers likely undocumented)

Not catastrophic, but `missing_docs` is intentionally deferred per CLAUDE.md, which means the lint won't catch a public-surface fn that someone forgets to document. A drive-by audit closes that without paying for the deferred lint.

## Behavior / fix

For each `pub fn`, `pub struct`, `pub enum`, and `pub trait` in `src/store.rs`, `src/server.rs`, and `src/domain.rs`:

- If a doc comment (`/// ...`) already exists, leave it alone.
- If missing, add a doc comment covering:
  - **Intent**: one sentence on what the fn/type does and why it exists.
  - **Invariants** (where applicable): what callers must guarantee, what the impl guarantees on return.
  - **Failure modes** (where applicable): which `bail!` messages the fn can emit and under what conditions.

Use existing well-documented fns as the reference shape — `create_project`, `add_phase`, `link_artifact` in `src/store.rs`; the MCP verb handlers in `src/server.rs`. Match voice: terse, technical, no marketing. Lowercase errors per Go convention.

Scope discipline:
- **Only public surface.** Private helpers stay as-is unless the public fn's doc comment naturally references them.
- **No re-organization.** Don't move code around to make docs read better — that's churn obscuring the doc-only diff.
- **No `missing_docs` lint enablement** — operator policy: deferred until v1.

## Acceptance

- Every `pub fn` / `pub struct` / `pub enum` / `pub trait` in the three files has a `///` doc comment.
- Doc-comment voice matches existing examples (terse, technical, lowercase errors).
- The diff is purely additive comment lines — no production-code line changes.
- `make check` stays green (clippy + tests unchanged).

## Test plan

- `make check` green.
- Manual diff scan: walk every new comment block, confirm each describes intent + non-obvious invariants where relevant, not just signature paraphrase.
- Quick verification: `grep -cE "^pub fn|^pub struct|^pub enum|^pub trait" src/store.rs src/server.rs src/domain.rs` and confirm against an immediately-preceding `///` line via a manual sample of 5 randomly-picked declarations.
- Spot-check: pick 3 newly-documented fns and ask "does this doc tell a fresh reader what they need to know to call this correctly?" If the answer is no for any, tighten.

## Non-goals

- Adding doc comments to private helpers (separate concern; would balloon the PR).
- Adding doc-comment coverage to `src/bin/dossier.rs` (CLI entry, not library surface).
- Adding inline `//` comments (different purpose; not the gap being closed).
- Enabling the `missing_docs` lint (operator policy: deferred until v1).
- Rewriting existing doc comments for style (only add where missing).
- Adding doc examples (`/// ```rust ...`) — out of scope unless the doc would be confusing without one.
- Adding crate-level (`//!`) docs or module-level overview docs (different concern).
