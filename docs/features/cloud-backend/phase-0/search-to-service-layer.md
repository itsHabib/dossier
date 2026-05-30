**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-29
**Related**: dossier task `search-to-service-layer` (id: `tsk_01KSV24EGWKFYR6EE0S1S00S9M`), cloud-backend TDD [spec.md](../spec.md) §6 + decision D9 (PR #59), [docs/follow-ups.md](../../../follow-ups.md)
**Depends on**: `extract-store-trait` (the `Store` trait must exist before `search` can be removed from it)

# Move `search` out of `Store` into the service layer — design spec

Phase 0 of the cloud-backend rollout. Lands **after** `extract-store-trait`.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/store.rs` (remove `fn search` + `search_snippet` helper from `FsStore`), `src/server.rs` (`search` runs in `MeshService` over the corpus) | ~60 changed | ~60 |
| Tests | relocate search tests from `store.rs` `mod tests` → `server.rs` `mod tests` | ~70 moved | ~35 |
| **Total** | | | **~95** |

Band: **amazing** (< 500 weighted). This is largely a **relocation**, not new logic — `search`'s body (`src/store.rs:183`, ~130 LOC incl. the `search_snippet` helper at `src/store.rs:1600`) moves up a layer; behavior is byte-for-byte identical. Weighted figure reflects the changed/moved surface, not a net addition.

## Goal

`search` conflates storage with application query. Once the corpus can live behind a remote backend (`S3Store`, Phase 1), a `search` *inside* the `Store` trait would have to either download-and-scan the whole corpus or secretly reach into the local warm cache — both leaky abstractions. Per decision **D9**, `search` is an **application query, not a storage operation**: it belongs in `MeshService`, over the corpus the store exposes (today the live `FsStore`; later the warm local cache). Keep the `Store` trait to pure storage CRUD.

## Behavior / fix

- Implement `search` in `MeshService` (`src/server.rs`), operating over the same corpus surface it scans today. The ranking, snippet generation (`search_snippet`), `kinds` / `project` / `limit` filtering, and score-then-`updated_at`-descending ordering are all preserved exactly — this is a move, not a redesign.
- Remove `search` (and its private `search_snippet` helper) from `FsStore` in `src/store.rs`. `search` is **not** a `Store` trait method (it was never added to the trait in `extract-store-trait`, per D9 — this task removes the inherent `FsStore::search` that the trait-extraction left in place).
- The `search` MCP verb handler (`src/server.rs:808`) stops delegating to `self.store.search(...)` and instead runs the service-layer implementation. The verb's wire signature, description, and output shape are unchanged.
- Because `FsStore` exposes the corpus the scan needs (project/phase/task reads), the service-layer `search` reads through the `Store` trait it already holds. No new public storage surface is added.

## Acceptance

- The `search` MCP verb returns **identical** results to today (same hits, same `score`, same `snippet`, same ordering) for every existing search test case.
- `search` is **no longer** an `FsStore` method and is **not** on the `Store` trait — `grep`-confirmable: no `fn search` in `src/store.rs`.
- `make check` green.

## Test plan

- The existing search tests (`search_filters_in_temp_corpus`, `search_title_and_body_hits_in_temp_corpus`, `search_rejects_whitespace_only_query`, `search_args_rejects_bad_kind`, and the corpus-rename-resilience cases around `src/store.rs:4796`) move to exercise the service-layer `search` and must pass unchanged in assertion content.
- No new behavioral assertions required — parity with the pre-move suite is the gate. If any test reached into `FsStore::search` directly, repoint it at the `MeshService` entry point.

## Non-goals

- Cache-aware search (scanning the warm local cache instead of live reads) — arrives with the warm cache, Phase 3a.
- A derived / semantic / inverted index — explicitly deferred (TDD §10 open question 2).
- Any change to ranking, snippet width, or the verb's description/filters.
