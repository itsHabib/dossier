**Status**: draft
**Owner**: @itsHabib
**Date**: 2026-05-30
**Related**: dossier task `s3store-cas-proof` (`tsk_01KSVMJHQ6DKB4Y1KG6Y8FB34C`); builds on PR #62 (`Store` trait + CAS); cloud-backend TDD [spec.md](../spec.md).

# `S3Store` — prove multi-writer CAS holds on S3 (against MinIO)

## Goal

Answer the one question that gates the whole cloud direction: **when two writers race on the same object, does compare-and-swap reliably let exactly one win, with no lost update?** Implement `S3Store` against the **existing** async `Store` trait, mirror the on-disk layout as closely as possible, and prove the race with an integration test against **MinIO** (local S3-compatible server). Keep it simple — mirror disk, no cleverness; optimizations are deferred until something shows they're needed.

**Not in scope (deliberately — do not touch):** wiring S3 into the running server (`MeshService`/`bin`), changing `FsStore`'s behavior, artifact sharding, warm cache, transport, auth. `S3Store` stands alone, proven by its test. The server keeps running on `FsStore`.

## Scope

| Bucket | Files | Est. LOC | Weighted |
|---|---|---|---|
| Production | `src/s3store.rs` (NEW — the whole backend), `src/store.rs` (small: `pub(crate)` on reused helpers + factor string-based parse cores), `src/lib.rs` (exports), `Cargo.toml` (deps) | ~350 | ~350 |
| Tests | `tests/s3_store_minio.rs` (NEW — env-gated MinIO integration test), property-test additions for round-trip parity | ~200 | ~100 |
| **Total** | | | **~450** |

Band: **amazing/ideal**.

## Two non-negotiables (operator priorities)

1. **Minimal public API.** The crate lints `unreachable_pub`. Export **only** `S3Store` and `S3Config` from `lib.rs`. Everything else in `s3store.rs` is private. The reused `store.rs` helpers become **`pub(crate)`**, never `pub`. We're doing right by downstream users — the public surface stays tiny and intentional.
2. **Property-tested byte-parity.** The load-bearing correctness is that an S3 object is the **exact same bytes** FsStore writes to disk, so the two backends are interchangeable and `dossier export` stays a literal copy. Prove it with a property test: for arbitrary valid `Project`/`Phase`/`Task`, `parse(serialize(x)) == x`. This guarantees the shared format is lossless regardless of backend. (The repo already has `tests/proptest_frontmatter_roundtrip.rs` and `tests/proptest_slug_roundtrip.rs` — follow that style.)

## Design — mirror disk

### Reuse FsStore's serialization (byte-parity seam)
S3Store must call the **same** serialize/parse code FsStore uses. In `src/store.rs`:
- Make `pub(crate)`: the three `serialize_*_file` fns + `notes_lines_for_task` (already take a domain value → `String`; reuse directly).
- The parse side is currently `&Path`-based (it does `fs::read_to_string` then parses). S3 hands you **bytes in memory**, not a path. So **factor out string-based cores** and have the disk path delegate (FsStore behavior must stay bit-identical — the existing proptests are the safety net):
  - `pub(crate) fn parse_project(raw: &str, with_body: bool) -> Result<Project>`
  - `pub(crate) fn parse_phase(raw: &str) -> Result<Phase>`
  - `pub(crate) fn parse_task(raw: &str, project_slug: &str) -> Result<(Task, Vec<String>)>`
  - refactor `read_frontmatter`/`load_phase`/`load_task_with_notes`/`load_project` to read the file then call these.
- Make `pub(crate)`: `task_filename`, `phase_filename`, `is_valid_slug`, and the `*_matches`/`sort_*` filter/sort fns (so `list_*` semantics match disk exactly).

### Keys mirror the on-disk tree (under an optional prefix)
- `{P}projects/<slug>/project.md`
- `{P}projects/<slug>/phases/<order:02>-<slug>.md`   (use `phase_filename`)
- `{P}projects/<slug>/tasks/<tsk_id>-<slug>.md`        (use `task_filename`)
- `{P}projects/<slug>/artifacts.jsonl`
Build keys by **joining with `/`** — S3 keys are not OS paths; never `Path::join` (it emits `\` on Windows). Validate `slug` via `is_valid_slug`.

### `Version` = the S3 ETag
Opaque per-backend token (the trait never compares versions across backends — fine that it's an ETag here vs SHA-256 for disk). After a PUT, the new version is `put_resp.e_tag()`. For reads, bind the returned `Versioned<T>.version` to the **GET response ETag** (not a listing ETag) so value and version stay consistent.

### CAS → S3 conditional PUTs
Map the trait's existing semantics:
- `expected = None` → `put_object().if_none_match("*")` (create-only; object-exists → 412).
- `expected = Some(v)` → `put_object().if_match(v.as_str())` (update-only-if-matches; mismatch → 412).
- **HTTP 412 → `StoreError::Conflict`.** `If-Match` on a **missing** key returns 404 → map to `StoreError::NotFound` (distinct from Conflict — matches `cas_write`'s absent branch).

### Error mapping (one small helper)
`map_sdk_err`: 412 → `Conflict`; 404/`NoSuchKey` → `NotFound`; dispatch failure / timeout / 5xx → `Unavailable`; UTF-8 / parse / anything else → `Invalid(msg)`. Detect 412/404 via the SDK error's `raw_response().status()`.

### Reads / lists
- `get_*` = `GetObject` → body → `parse_*` → pair with the GET ETag.
- `list_*` = `ListObjectsV2` (prefix; `delimiter("/")` only for corpus-wide `list_projects` → project slugs from `CommonPrefixes`) → `GetObject` per key with **bounded concurrency** (`futures::stream::...buffer_unordered(16)` or a `JoinSet`) → parse → reuse the `*_matches`/`sort_*` helpers for identical filter semantics. Resolve `filter.phase` (slug→id) the same way `FsStore::list_tasks` does.

### Artifacts — mirror disk, NO sharding
`artifacts.jsonl` stays a single object (same as disk). `put_artifact` = `GetObject` the current jsonl (+ its ETag; absent ⇒ empty), append the new JSON line, `PutObject` conditionally (`if_match` the ETag read, or `if_none_match("*")` if it was absent); on 412 retry the read-append-put a bounded number of times. `list_artifacts` = `GetObject` + parse JSONL (absent ⇒ empty). (Yes, a shared hot file is contention-prone under heavy concurrency — that's a deliberate "optimize when it's actually a problem" call, not now.)

### Construction
```rust
pub struct S3Config {
    pub bucket: String,
    pub prefix: String,                 // tenant segment; "" == bucket root; no leading/trailing '/'
    pub endpoint_url: Option<String>,   // Some("http://localhost:9000") for MinIO; None = real AWS
    pub region: String,                 // MinIO ignores it; SDK requires one; default "us-east-1"
    pub access_key_id: String,
    pub secret_access_key: String,
    pub force_path_style: bool,         // true for MinIO
}
pub struct S3Store { /* client + bucket + prefix, all private */ }
impl S3Store { pub async fn new(cfg: S3Config) -> Result<Self, StoreError>; }
```
Client build: `aws_config::defaults(BehaviorVersion::latest())` + `.region(...)` + `.credentials_provider(Credentials::from_keys(...))` + (if `endpoint_url` set) `.endpoint_url(...)`; then `aws_sdk_s3::config::Builder::from(&shared).force_path_style(cfg.force_path_style).build()`. `new` does **not** create the bucket. No `from_env`/`bin` wiring in this PR.

### Dependencies (`Cargo.toml`)
`aws-config = "1"`, `aws-sdk-s3 = "1"`, `aws-smithy-runtime-api = "1"` (for `raw_response()` / the 412 status check), `futures = "0.3"` (bounded list fan-out). Default features on to start (avoid the TLS/connector feature rabbit hole). Confirm the build on the MSVC toolchain. Keep clippy-strict clean (small `?`-propagating helpers — `map_sdk_err`, `body_to_string`, `apply_cas`, a per-key GET fn; **no `unwrap`/`expect`/indexing in non-test code**; cognitive complexity ≤ 20 per fn).

## Tests

### Property tests (byte-parity — the cheap guarantee)
Add round-trip property tests proving `parse_*(serialize_*(x)) == x` for arbitrary valid `Project`, `Phase`, `Task` (with notes). Put them where the existing roundtrip proptests live / a sibling file. These don't need S3 at all — they lock the shared format.

### MinIO integration test — `tests/s3_store_minio.rs`
Gate the **whole** file behind env var `DOSSIER_S3_TEST_ENDPOINT` (absent ⇒ every test early-returns, so `cargo test`/CI stays green without MinIO). Helper builds `S3Config` from `DOSSIER_S3_TEST_ENDPOINT` + `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` (default `minioadmin`/`minioadmin`), bucket `dossier-it`, **unique `prefix` per run** (`format!("it/{}", Ulid::new())`) for isolation. `ensure_bucket` at the top (create, ignore already-exists). Lint header like `tests/cli_subcommands.rs`. Scenarios, each a `#[tokio::test]`:
1. create-only success; duplicate create → `Conflict`.
2. CAS update with correct version → `Ok(new etag)`; **stale version → `Conflict`**.
3. get → ETag → put round-trip: `put(None)`→v0; `get`→`Versioned{value,version}`; assert `version == v0` (ETag stable) and `value` equals the original; then `put(mutated, Some(version))`→`Ok`.
4. update-if-absent (`if_match` on a never-created key) → `NotFound` (not Conflict).
5. **CENTERPIECE — concurrent-writer race:** seed at v0; two `tokio` tasks both `put(variant, Some(v0))` concurrently; assert **exactly one `Ok`, the other `Conflict`** — never two Ok, never two Err. **Loop N≈30** (fresh key each iteration). This is the proof.
6. artifact round-trip: `put_artifact(a)` then `list_artifacts({project})` contains `a`.

## Acceptance
- `make check` green with MinIO **down** (S3 integration tests skip cleanly; property tests + existing suite pass).
- With MinIO up + `DOSSIER_S3_TEST_ENDPOINT=http://localhost:9000`: `cargo test --test s3_store_minio` passes — **the concurrent-writer race in particular** (exactly one winner, looped).
- Public API is exactly `S3Store` + `S3Config`; reused helpers are `pub(crate)`.
- FsStore behavior unchanged (existing proptests green) after the parse-core refactor.

## Non-goals
Server wiring / `MeshService` / `bin` selection; lifting the state machine above the trait; artifact sharding; warm cache; HTTP transport; auth; real-AWS validation (MinIO is the local proof). All deferred — decided after we see this result.
