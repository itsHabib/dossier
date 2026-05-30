# Follow-ups

Non-blocking items spotted during review or implementation. Cleared
opportunistically. New entries go at the bottom; resolved entries get
deleted (commit history is the record).

## cloud-backend

- **(Phase 1 — S3 backend) Per-object create-only CAS doesn't protect collection invariants.**
  The §6 `Store` contract creates objects with `put_*(expected = None)` — a CAS on the *new
  object alone*. But some creates depend on **collection** state: `phase.add` picks the next
  `order` by reading existing phases, and slug uniqueness is checked across the project. Under
  multi-client S3 (Phase 1) two writers can both read the same list, mint different keys, and
  have both create-only PUTs succeed → duplicate `order` or duplicate slugs even though every
  per-object CAS passed. Phase-0 `phase-ordering-concurrency` covers the `order` half (CAS
  `project.md`, the collection); the general case (any create that depends on list state) needs
  a parent/index-version CAS or deterministic slug/order keys before the Phase 1 validation
  gate. Suggest a one-paragraph addition to [`spec.md`](features/cloud-backend/spec.md) §6/§8.
  (codex, [PR #59](https://github.com/itsHabib/dossier/pull/59))

- **(Phase 2 — artifact sharding) `artifact.link` loses idempotency under D3 sharding.**
  Sharding `artifacts.jsonl` → `artifacts/art_<ulid>.json` (D3) drops the de-dup the single
  file gave for the same `(task, kind, ref)` link. Two clients concurrently linking the same
  PR/commit both see no existing artifact, mint different ULIDs, and both `If-None-Match:*`
  PUTs succeed → duplicate links in `artifact.list`. Fix: a deterministic object key /
  idempotency key for link identity, or CAS a small index. (`artifact.link` isn't shipped yet
  — capture before it lands sharded.) (codex, [PR #59](https://github.com/itsHabib/dossier/pull/59))

