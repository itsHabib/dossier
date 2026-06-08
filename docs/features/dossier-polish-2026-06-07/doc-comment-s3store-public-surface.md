**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-08
**Related**: dossier task `doc-comment-s3store-public-surface` (id: `tsk_01KTJSJGP58B2BZ6Q2A9TSP9HH`), [docs/follow-ups.md](../../follow-ups.md), `src/s3store.rs`, PR #48 (public-surface doc pass), PR #64 (introduced s3store)

# docs: doc-comment the s3store.rs public surface — spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production source (doc comments, 1.0×) | `src/s3store.rs` | ~15 | 15 |
| **Total** | | | **~15** |

Band: amazing.

## Goal

`src/s3store.rs` landed in #64, *after* the #48 "document public surface (store/server/domain)" pass — so it was missed. Its public items lack `///` doc comments while the rest of the crate's public surface is documented; s3store is the outlier (8 `///` lines for a 792-LOC file that defines the entire S3 backend's public API). Close the gap so the S3 backend reads like the rest of the crate under `cargo doc`.

## Behavior / fix

Add behavior-describing `///` doc comments to the undocumented public items in `src/s3store.rs`. Concretely, the gaps are:

- `pub struct S3Config` — struct header doc.
- `pub struct S3Store` — struct header doc.
- `S3Store::new` (`pub async fn`) — doc describing what it does (and its failure mode, since it returns `Result<Self, StoreError>`).
- `S3Config` fields missing docs: `bucket`, `access_key_id`, `secret_access_key`, and the test-counter field (`test_list_call_counter`). The fields `prefix` / `endpoint_url` / `region` / `force_path_style` already have docs — leave those as-is.

Match the style of the #48 pass: describe *what it is / does*, not roadmap or design-doc references (per the repo's "no design-doc refs in code comments" convention — CLAUDE.md "Conventions"). The S3 design narrative stays in `docs/features/cloud-backend/spec.md`.

## Acceptance

- Every `pub struct` / `pub fn` / `pub async fn` in `src/s3store.rs` has a preceding `///`.
- `make check` stays green.
- `cargo doc` renders `S3Config` and `S3Store` with the new docs.

## Test plan

`awk 'prev !~ /^\s*\/\/\// && /^\s*pub (async fn|fn|struct|enum) /{print NR} {prev=$0}' src/s3store.rs` returns nothing (no undocumented public item). `cargo doc --no-deps` succeeds.

## Non-goals

- Private items — the doc-comment bar is public-API only.
- The S3 design narrative (lives in `docs/features/cloud-backend/spec.md`).
- Enabling the `missing_docs` lint — intentionally deferred past v0 per CLAUDE.md "Lint discipline".
