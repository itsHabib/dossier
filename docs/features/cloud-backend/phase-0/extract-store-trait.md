**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-29
**Related**: dossier task `extract-store-trait` (id: `tsk_01KSV24D8HS85NBZJ587W6TA9R`), cloud-backend TDD [spec.md](../spec.md) §6 + decisions D1/D8/D10 (PR #59), [docs/follow-ups.md](../../../follow-ups.md)

# Extract async `Store` trait + `FsStore` impl + `Arc<dyn Store>` — design spec

This is **Phase 0** of the cloud-backend rollout — the no-regret backend seam that every later phase (S3 backend, warm cache, multi-tenant) builds on. It must land before `search-to-service-layer` and `phase-ordering-concurrency`, which sit on top of the trait + CAS model introduced here.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production source | `src/store.rs` (trait + types + `impl Store for FsStore` + SHA-256 CAS in write path), `src/server.rs` (`Arc<FsStore>` → `Arc<dyn Store>` + `.await` ripple), `src/lib.rs` (re-exports) | ~195 | ~195 |
| Tests | `src/store.rs` `mod tests` — CAS unit test | ~50 | ~25 |
| **Total** | | | **~220** |

Band: **amazing** per repo's PR sizing convention (< 500 weighted). This is the widest of the three Phase-0 tasks — the `async` conversion (D8) ripples a `.await` through every `MeshService` handler that calls the store, so raw line-touch count is higher than the net logic change. If the ripple comes out larger than estimated it may approach **ideal**; do **not** split — the trait, its `FsStore` impl, and the server wiring must land together to compile (single coupled refactor, which the repo's sizing convention explicitly permits).

## Goal

Storage is currently a concrete `Arc<FsStore>` on `MeshService` (`src/server.rs:41`); nothing is swappable, so no remote backend is possible. Introduce a backend seam: an async `Store` trait that `FsStore` implements, with the server holding `Arc<dyn Store>`. This is the Terraform local-vs-remote playbook — everything above the trait stays backend-agnostic; `S3Store` (Phase 1, out of scope here) slots in behind the same trait later. **No MCP verb signatures change.**

## Behavior / fix

Introduce the version + error types and the async trait (from TDD §6.1; reproduced here so this spec is self-contained):

```rust
pub struct Version(String);                 // FsStore: SHA-256 of the file's raw bytes
pub struct Versioned<T> { pub value: T, pub version: Version }
pub enum StoreError { NotFound, Conflict /*412*/, Unavailable, Invalid(String), Io(std::io::Error) }

pub trait Store: Send + Sync {
    // reads return the current version for a later CAS; list_* are versioned (D10)
    async fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError>;
    async fn list_projects(&self, f: ProjectListFilter) -> Result<Vec<Versioned<Project>>, StoreError>;
    async fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError>;
    async fn list_phases(&self, f: PhaseListFilter) -> Result<Vec<Versioned<Phase>>, StoreError>;
    async fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError>;
    async fn list_tasks(&self, f: TaskListFilter) -> Result<Vec<Versioned<Task>>, StoreError>;
    async fn list_artifacts(&self, f: ArtifactListFilter) -> Result<Vec<Artifact>, StoreError>;

    // writes: expected = None ⇒ create-only; Some(v) ⇒ update-if-version-matches
    async fn put_project(&self, p: &Project, expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_phase(&self,   p: &Phase,   expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_task(&self,    t: &Task,    expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_artifact(&self, a: &Artifact) -> Result<(), StoreError>; // unique id ⇒ create-only, no CAS
    // NOTE: no delete_* — archival/deletion is out of scope through the validation gate.
}
```

Implementation notes:

- **`async` trait (D8).** Use stable `async fn` in traits (RPITIT, stable since 1.75; repo is on 1.95+). `S3Store` will do network I/O later; a sync trait blocking a tokio worker would starve the executor, so the trait is async from day one even though `FsStore` is sync-on-disk. `MeshService` handlers that call the store gain `.await`.
- **`Version` for `FsStore`** = SHA-256 of the file's raw bytes (intrinsic, not a stored field; platform-stable regardless of serialization order — see TDD §5).
- **CAS** = compare-the-stored-file's-current-hash against `expected` before the atomic rename, all under the existing `write_lock` mutex. `expected = None` → create-only (fail if the file already exists, `Conflict`). `expected = Some(v)` → re-hash the on-disk file; if it differs from `v`, return `Conflict` and do not write; if it matches (or the prior contents hash to `v`), proceed with the existing `.tmp` + atomic-rename helper (`write_atomic`, `src/store.rs:1325`) and return the new `Version`.
- **`list_*` returns `Vec<Versioned<T>>` (D10)** so a list→claim flow already has the version for CAS without an extra `get_*` round-trip.
- **Server wiring.** `MeshService.store` becomes `Arc<dyn Store>` (`src/server.rs:41`); `MeshService::new` takes `impl Store + 'static` (or an `Arc<dyn Store>`). The `#[tool]` handlers map `StoreError` onto the existing rmcp error mapping (`internal_or_invalid` / the conflict path) — a `Conflict` surfaces as the protocol's conflict error, `NotFound` as today.
- **`search` stays put for now.** It remains an inherent `FsStore` method this task; the sibling task `search-to-service-layer` moves it to the service layer and removes it from the trait surface. Do **not** add `search` to the `Store` trait (D9).
- **Layering unchanged.** `domain → store → server → bin`. New types (`Version`, `Versioned<T>`, `StoreError`, `Store`) live in `src/store.rs`; no downward import into `domain`.

## Acceptance

- `MeshService` compiles against `Arc<dyn Store>`; `FsStore: Store`.
- The full existing test suite passes unchanged (read/write parity — no verb behavior change).
- A write with a **stale** expected version returns `Conflict`; a write with the **correct** expected version succeeds and returns the new `Version`.
- `make check` green (fmt + clippy `-D warnings` + test) on the repo's matrix.

## Test plan

- Existing suite green (regression parity gate).
- New CAS unit test in `src/store.rs` `mod tests` (against a temp corpus): create a project (`expected = None`) → succeeds, returns `v0`; `put_project` with `expected = Some(wrong_version)` → `Conflict`, on-disk content unchanged; `put_project` with `expected = Some(v0)` → succeeds, returns `v1 != v0`; create-only on an existing slug (`expected = None`) → `Conflict`.

## Non-goals

- `S3Store` — Phase 1 (`If-Match`/`If-None-Match`, HTTP 412 → `Conflict`).
- Moving `search` out of the store — sibling task `search-to-service-layer`.
- Wiring CAS into `phase.add` for the ordering race — sibling task `phase-ordering-concurrency` (consumes this CAS model).
- Artifact sharding (`artifacts.jsonl` → `artifacts/<id>.json`) — Phase 2.
- The CAS retry / 3-way re-read branch (TDD §7.1/§8) — lands with `S3Store` in Phase 1; Phase 0 only needs the single-shot CAS primitive.
