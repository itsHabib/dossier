**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-07-23
**Related**: dossier task `artifact-list-ref-filter` (id: `tsk_01KY86EG5KR3Q8SQMK28CYC0C5`), [state-substrate TDD](../state-substrate/spec.md) §6/§7.2. **Depends on** `artifact-meta-field` (meta in output).

# artifact.list ref exact filter + meta in output — design spec

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/store.rs`, `src/s3store.rs`, `src/server/artifact.rs`, `PROTOCOL.md` | ~90 | ~90 |
| Tests | `src/store.rs` (`mod tests`), `src/s3store.rs` (`mod tests`) | ~120 | ~60 |
| **Total** | | | **~150** |

Band: **amazing** (< 500 wLOC) per repo's PR sizing convention. Matches the TDD's ≤200 wLOC budget for Phase A task 2.

## Goal

"Why did this PR merge?" starts from a PR URL / run id / gate ref. Today `artifact.list` filters only by project/task/kind, so callers scan and filter client-side. Add an exact-match `ref` filter so a canonical PR URL (receipt) or gate audit ref (verdict) resolves directly to its records, and ensure `meta` (landed by `artifact-meta-field`) appears in list output.

## Behavior / fix

Add an optional `ref` exact-match filter to `artifact.list`, threaded through **both** store paths (`FsStore` in `src/store.rs`, `S3Store` in `src/s3store.rs`) via the `ArtifactListFilter` at the store seam. The verb handler lives in `src/server/artifact.rs`.

- `ref` is an **exact-match** filter over the canonical ref form for the kind (TDD §5): the caller passes the canonical GitHub PR URL (receipt) or gate audit ref (verdict) verbatim. No substring/prefix matching.
- The filter **AND-composes** with the existing project/task/kind predicates. Absent `ref` = no ref filtering (today's behavior, unchanged).
- Returned artifacts **include `meta`** when present (the field already round-trips after `artifact-meta-field`; confirm it surfaces through the list output path).

Update the `PROTOCOL.md` verb table for `artifact.list` (the new `ref` param) in the **same PR**.

## Acceptance

- `artifact.list { project, kind: "receipt", ref: <PR URL> }` returns exactly the matching rows.
- Filter AND-composes with project/task/kind; absent `ref` = no filtering (today's behavior).
- `meta` appears in list output for rows that have it.

## Test plan

- Unit: `ref` hit / miss / AND-with-kind; `meta` passthrough in output.
- Store-parity test across `FsStore` + `S3Store` (same query, same result shape).

## Non-goals

- Substring/prefix `ref` matching (exact only).
- `meta`-key filters.
- `search`-index coverage of `meta` (TDD §10 Q5).
- The `meta` field itself — landed by the upstream `artifact-meta-field` task.
