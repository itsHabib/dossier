**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-06-01
**Related**: dossier task `wire-s3-backend-bin-config` (id: `tsk_01KT1WPR0ND4RYCB5MTC5VYANZ`), [docs/follow-ups.md](../../follow-ups.md)

# Wire S3 backend into bin + config — design spec

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | `src/server.rs` (add an `Arc<dyn Store>` constructor), `src/bin/dossier.rs` (`run_serve` backend selection + S3Store construction) | ~110 | 110 |
| Tests | backend-selection unit test | ~30 | 15 |
| **Total** | | | **~125** |

Band: **amazing**. Runtime: **local** (S3 boot verified against local MinIO).

## Goal

The policy/mechanism lift is done — `MeshService` runs on `Arc<dyn Store>`, and both `FsStore` and `S3Store` (PR #64) implement the trait. But the server only boots on `FsStore`: `MeshService::new(store: FsStore)` takes a concrete FsStore and `run_serve` calls `FsStore::open` directly. There's no way to select S3 — the payoff this whole phase unblocked.

## Behavior / fix

- **`src/server.rs`** — `MeshService::new(store: FsStore)` is concrete. Add a constructor that accepts the trait — e.g. `MeshService::from_store(store: Arc<dyn Store>) -> Self` — and have `new(FsStore)` delegate to it. **Keep `new(FsStore)`** so the existing test + CLI call sites are untouched (minimal blast radius).
- **`src/bin/dossier.rs`** — `run_serve` reads `DOSSIER_BACKEND` (default `fs`): on `fs`, `FsStore::open(corpus)` as today; on `s3`, construct an `S3Store` from env (bucket / region / endpoint / credentials per the cloud spec) and pass it via `from_store`. Unknown value → clear error.
- The local CLI subcommands (`complete` / `update` / `link` / `list`) stay on `FsStore` — they operate on a local corpus path, not the served backend.

## Acceptance

- `DOSSIER_BACKEND=s3 dossier serve …` boots `MeshService` over `S3Store` and serves the verbs end-to-end against a bucket (MinIO locally).
- Unset / `DOSSIER_BACKEND=fs` still boots `FsStore` (default unchanged).
- An unknown `DOSSIER_BACKEND` value fails with a clear message.

## Test plan

- `make check` green.
- A backend-selection unit test: `fs` (and unset) → FsStore path; `s3` → S3Store construction (env-gated; skip the live-bucket leg when MinIO env is absent).
- Manual: boot against MinIO, run a `project.create` + `project.get` round-trip over S3.

## Non-goals

- Warm cache / read-latency work (separate step).
- Multi-tenant routing / auth (post-gate s3-cloud phases).
- The N=100 lost-update validation gate (its own task).

## Source

Named next-unblock in the `policy-mechanism-separation` phase body. Umbrella: `docs/features/cloud-backend/spec.md`.
