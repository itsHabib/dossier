**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-07-23
**Related**: dossier task `artifact-meta-field` (id: `tsk_01KY86E5R4HW7WCV1DPREQK0WM`), [state-substrate TDD](../state-substrate/spec.md) §5/§6/§7.4/§8

# Artifact meta map end-to-end (domain, stores, artifact.link, caps) — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/domain.rs`, `src/store.rs`, `src/s3store.rs`, `src/server/artifact.rs`, `PROTOCOL.md`, `LAYOUT.md` | ~180 | ~180 |
| Tests | `src/store.rs` (`mod tests`), `src/s3store.rs` (`mod tests`), `tests/proptest_*.rs` | ~200 | ~100 |
| **Total** | | | **~280** |

Band: **amazing** (< 500 wLOC) per repo's PR sizing convention. Matches the TDD's ≤300 wLOC budget for Phase A task 1.

## Goal

Give `Artifact` a small, capped, structured `meta` map so verdicts/receipts carry a denormalized summary inline — so a retrospective read ("why did this PR merge?") answers from dossier alone instead of joining gate's and ship's stores. This is the base task of the state-substrate schema phase; the `ref` filter and the conventions docs both build on the shape this lands.

## Behavior / fix

Add `meta: BTreeMap<String, String>` to `Artifact` and `LinkArtifact` in `src/domain.rs`, with `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]` — `BTreeMap` for deterministic serialization (stable git diffs), `default` + `skip_serializing_if` so old meta-less rows parse unchanged and new empty-meta rows omit the field.

Round-trip the field in **both** stores: `FsStore` (`src/store.rs`) and `S3Store` (`src/s3store.rs`). The store stays a dumb append — no validation there.

Accept optional `meta` on `artifact.link`. Enforce the caps in the **policy layer above the store** (`link_artifact_outcome` in `src/server/artifact.rs` — note: the server monolith was split per PR #94/#95, so this lives in `src/server/artifact.rs` now, not `src/server.rs` as the TDD text says):

- ≤16 keys, key ≤64 bytes, value ≤512 bytes, ≤4 KiB total serialized (TDD §5 D5).
- Cap violation → `invalid_params` **naming the failing key**.
- No per-kind validation of keys or values (TDD §4 D4) — unknown keys/values round-trip untouched, like unknown kinds.

**Meta-immutability dedup** (per the task's PR #93 review note + TDD §6/§7.4/§8). The shipped `link_artifact_outcome` already dedups on `(task, kind, ref)` and returns the existing row. Extend that dedup to reason about `meta`:

- existing row found AND new `meta` byte-identical → return existing row (idempotent; crash-safe close-out re-run).
- existing row found AND new `meta` differs → `invalid_params` (`"meta is immutable for an existing (task, kind, ref); supersede instead"`).
- no existing row → append.

Correction is via the supersede convention (distinct `ref` + `meta.supersedes`), never mutation — consistent with append-only `artifacts.jsonl`.

Update `PROTOCOL.md` (Artifact primitive + `artifact.link` verb, the `meta` field + caps + immutability semantics) and `LAYOUT.md` (`artifacts.jsonl` row shape) in the **same PR**.

## Acceptance

- Old meta-less `artifacts.jsonl` rows parse unchanged; new rows omit `meta` when empty.
- Cap violation → `invalid_params` naming the failing key.
- Both stores round-trip `meta` byte-stably (BTreeMap ordering).
- Re-link with byte-identical meta → existing row (idempotent). Re-link with differing meta → `invalid_params` ("meta is immutable…").

## Test plan

- Unit: each cap limit at ±1 (16 keys, 64 B key, 512 B value, 4 KiB total), empty-map omission on serialize, unknown-kind + meta round-trip.
- Unit: dedup — identical-meta re-link returns existing row; differing-meta re-link → `invalid_params`.
- Proptest: arbitrary capped maps round-trip through both `FsStore` and `S3Store`.
- Dogfood-corpus read test still green (old rows parse).

## Non-goals

- `ref` filter on `artifact.list` — sibling task `artifact-list-ref-filter`.
- Well-known-kind / meta-key convention docs — sibling task `verdict-receipt-conventions`.
- Any emitter wiring (driver auto-wiring is a separate TDD, D6).
- Per-kind value validation (rejected by design, D4).
