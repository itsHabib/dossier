# dossier Cloud — Technical Design Document

**Status:** draft / proposal — **NOT a build commitment.** The artifact we decide *from*, and the plan we'd execute *if* validation (Phase 1) earns it.
**Owner:** @itsHabib
**Date:** 2026-05-29
**Revision:** v3 — folds in round-2 review (async `Store` trait, `search` moved out of the trait, §9 phase-dependency fixes + Phase 3 split, phase-ordering concurrency, retry 3-way branching, cache TTL semantics, binary validation gate, per-phase scope).
**Related:** [vision.md](../../vision.md) (this deliberately re-opens v0 non-goals — multi-writer, audit, identity, web UI), [PROTOCOL.md](../../../PROTOCOL.md), [LAYOUT.md](../../../LAYOUT.md).

> **Reviewers — focus areas:** (1) the `Store` trait API in §6, (2) the CAS write/retry 3-way branching in §7.1/§8, (3) the consistency/cache model in §8, (4) the phase sequencing + validation gate in §9.

---

## 1. Problem & hypothesis

dossier today is local, single-writer, markdown-on-disk, stdio MCP — excellent for one developer. The bet: a **team** running a fleet of agents + humans needs a *shared* project-memory plane — one corpus many seats read and write so they don't collide or lose context. That shared, hosted plane is the paid product.

This TDD designs the backend that makes the corpus **remote and multi-writer**, behind dossier's existing storage seam, following Terraform's remote-state playbook — plus the rollout plan. Go/no-go is gated on Phase 1 (§9). Billing/pricing/GTM are out of scope here.

## 2. Goals & non-goals

**Functional**
- MCP verbs + the four primitives are **unchanged at the boundary**.
- Corpus lives in **remote storage**; multiple clients on multiple machines share one corpus safely.
- **Concurrent writes are safe** — no lost updates, no corruption.
- Storage is **pluggable**: local `FsStore` stays the default; remote is an alternate backend.
- *(Post-validation)* per-team isolation + auth.

**Non-functional**

| Property | Target |
|---|---|
| Read latency | interactive — warm-cache `list`/`get` under agent-loop tolerances |
| Durability | corpus is source of truth; no loss on crash or concurrent write |
| Consistency | **remote: strong read-after-write** (S3 guarantees this, same-region; cross-region replication would be eventually consistent — single-region assumed). **Local cache: bounded-stale** — correctness never depends on cache freshness because the authoritative check is the write-time CAS (§8). |
| Portability | `dossier export` reproduces the identical on-disk corpus `FsStore` reads — anti-lock-in selling point |
| Operability | minimal infra; cheap per team |
| Security | per-tenant isolation; authn + authz; no cross-tenant leakage |

**Non-goals (this doc):** billing/pricing/GTM; RAG/vectors; conflict-detection beyond write-safety; web-UI design; delete/archival semantics (no `delete_*` through the gate — see §6).

## 3. Architecture overview

```
        MCP clients (Claude, cursor, ship, humans)
                      │  stdio (local)  |  HTTP/SSE + token (cloud)
                      ▼
            ┌───────────────────────────┐
            │ MeshService (verbs + search) │  unchanged verb logic + state machine;
            └──────────┬──────────────────┘  search runs HERE over the warm cache
                       │  Store trait  (the seam)
        ┌──────────────┼─────────────────┐
        ▼              ▼                  ▼
   FsStore        S3Store (+ warm     [GitStore]   ← optional, later
   (local disk)    local cache)        (deferred)
                       │
                       ▼
              S3 / R2 / GCS  (per-tenant prefix)
```

Everything above the `Store` trait is backend-agnostic. The verbs, the task state machine, and the MCP surface don't change. `search` lives in the **service layer over the warm cache**, not in `Store` (§6).

## 4. Key decisions (incorporating both review rounds)

| # | Decision | Rationale |
|---|---|---|
| D1 | Extract a `Store` trait; `FsStore` implements it | The backend seam (Terraform local-vs-remote). No-regret. |
| D2 | **Remote substrate = object store (S3/R2/GCS) + per-object CAS** — not git | git's conflict-free claim fails on shared files (`artifacts.jsonl`, `project.md`, phase order). S3 CAS is unambiguous; git stays a later option behind the trait. |
| D3 | **Shard artifacts**: `artifacts.jsonl` → `artifacts/<art_id>.json` | Kills the hot shared file; one create-only PUT per artifact; `artifact.list` = prefix scan. Applies to FsStore too. |
| D4 | Concurrency = per-object CAS (`If-Match` ETag), bounded retry w/ full jitter | Fine-grained; §8. |
| D5 | Read path = warm local working copy synced from remote | `list`/`search` scan; per-query remote scans are slow. |
| D6 | Tenant isolation by **routing** (per-tenant prefix/bucket + creds), not a `tenant` field | One `Store` instance = one corpus = one team. |
| D7 | Consistency: remote strong; cache bounded-stale; **claim safety from write-time CAS** | A stale `list` is safe — the CAS on the mutating write is the authority. |
| **D8** | **`Store` trait is async** (`async fn` in traits / RPITIT, stable 1.75) | `S3Store` does network I/O; a sync trait in a tokio runtime starves the executor. Affects the whole impl surface — decided before Phase 0. |
| **D9** | **`search` is NOT in `Store`** — it's a service-layer op over the warm cache | Search is app-query, not storage; over S3 it'd download-and-scan or secretly hit the cache (leaky). Keep the trait to storage CRUD. |
| **D10** | **`list_*` returns `Vec<Versioned<T>>`** | So a list→claim flow has the version for CAS without an extra `get_*` round-trip. |

## 5. Data model

**On-disk corpus shape: unchanged** ([LAYOUT.md](../../../LAYOUT.md)) except artifact sharding (D3):

```
projects/<slug>/
  project.md
  phases/NN-<slug>.md
  tasks/tsk_<ulid>.md
  artifacts/art_<ulid>.json   # NEW: one object per artifact (was artifacts.jsonl)
```

**Remote (S3) keys** mirror the tree under a per-tenant prefix: `s3://<bucket>/tenants/<tenant>/projects/<slug>/...`.

**Versioning:** the version token is **intrinsic**, not a stored field — S3 object **ETag**, or for `FsStore` the **SHA-256 of the file's raw UTF-8 bytes** (platform-stable, independent of serialization order). Carried in `Versioned<T>` (§6) for CAS.

**Tenancy:** no `tenant` field in documents (D6). Tenant = the S3 prefix; the bearer token resolves to tenant + actor. Existing `actor` / `assignee` strings carry seat identity, unchanged.

## 6. API contract

### 6.1 The `Store` trait (async, versioned, CAS)

```rust
pub struct Version(String);                 // S3 ETag, or FsStore SHA-256 of file bytes
pub struct Versioned<T> { pub value: T, pub version: Version }
pub enum StoreError { NotFound, Conflict /*412*/, Unavailable, Invalid(String), Io(std::io::Error) }

pub trait Store: Send + Sync {
    // reads return current version for later CAS; list_* are versioned (D10)
    async fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError>;
    async fn list_projects(&self, f: ProjectFilter) -> Result<Vec<Versioned<Project>>, StoreError>;
    async fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError>;
    async fn list_phases(&self, f: PhaseFilter) -> Result<Vec<Versioned<Phase>>, StoreError>;
    async fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError>;
    async fn list_tasks(&self, f: TaskFilter) -> Result<Vec<Versioned<Task>>, StoreError>;
    async fn list_artifacts(&self, f: ArtifactFilter) -> Result<Vec<Artifact>, StoreError>;

    // writes: expected = None ⇒ create-only (If-None-Match: *); Some(v) ⇒ update-if-version-matches (If-Match: v)
    async fn put_project(&self, p: &Project, expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_phase(&self,   p: &Phase,   expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_task(&self,    t: &Task,    expected: Option<Version>) -> Result<Version, StoreError>;
    async fn put_artifact(&self, a: &Artifact) -> Result<(), StoreError>; // unique id ⇒ create-only, no CAS
    // NOTE: no delete_* — archival/deletion is out of scope through the validation gate.
}
```

- `search` is **not** here — it lives in `MeshService` over the warm cache (D9).
- `FsStore`: `Version` = SHA-256(file bytes); CAS = compare-hash-before-atomic-rename under the existing mutex.
- `S3Store`: GET captures ETag; PUT uses `If-Match`/`If-None-Match`; `Conflict` on HTTP 412.
- Server changes `Arc<FsStore>` → `Arc<dyn Store>`. **No verb signatures change.**

### 6.2 Config
- `DOSSIER_BACKEND = fs | s3` (default `fs`); S3 needs bucket + region + creds + tenant prefix.
- `DOSSIER_TRANSPORT = stdio | http` (default `stdio`); `http` exposes rmcp's streamable-HTTP/SSE endpoint.

### 6.3 Auth surface (Phase 5)
Bearer token per seat → `{ tenant, actor }`; server routes to that tenant's `Store` (D6), stamps `actor` on writes; cross-tenant access impossible by construction.

## 7. Key flows

### 7.1 Write (`task.claim`) — read-modify-write with CAS + 3-way re-read branch
```
1. get_task(id) → (task, v0)
2. run state machine (unchanged)
3. mutate in memory
4. put_task(task, expected=v0) → v1  |  Conflict(412)
5. on Conflict, RE-READ and branch:
   (a) state machine now rejects (e.g. already claimed) → surface error, DO NOT retry  (terminal)
   (b) desired state already reached → return success                                  (idempotent)
   (c) version changed but transition still valid → re-apply, re-put                    (true retry, §8)
6. on success: push-through update the warm cache to v1
```

### 7.2 Read (`task.list` / `search`) — warm cache w/ TTL
```
1. serve from local working copy
2. each cached object carries a last-fetched timestamp; a read re-fetches from remote
   iff (now - last_fetched) > TTL, then atomically replaces the entry
3. claim decisions off a slightly-stale list are SAFE — the claim write (7.1) re-checks via CAS
```

### 7.3 `artifact.link` — sharded, contention-free
`put_artifact` → PUT `artifacts/art_<ulid>.json` with `If-None-Match: *`; unique id ⇒ no hot file, no CAS loop. `artifact.list` = prefix scan.

### 7.4 Concurrent claim race
Two agents read v0; A's `put(exp=v0)` → 200; B's `put(exp=v0)` → 412 → B re-reads → state machine says "already claimed" → terminal (7.1a), no retry.

### 7.5 Tenant routing (Phase 6)
`token → {tenant=T, actor} → Store bound to s3://bucket/tenants/T/`; all verbs scoped to that prefix.

### 7.6 Offline / degraded
Remote unreachable → reads serve warm cache (flagged stale); writes fail fast with typed `Unavailable` (NO queueing in v1 — queued writes risk lost updates).

## 8. Concurrency & consistency

- **CAS:** every mutating write carries the expected version; S3 `If-Match` → 412 on mismatch.
- **Retry:** on `Conflict`, re-read and take the 3-way branch (7.1: terminal / idempotent / retry). True-retry path uses **full jitter** — sleep `uniform(0, min(cap, base·2^n))` with `base=25ms`, `cap=2s`, **max 5 attempts**; on exhaustion return a typed `conflict` error to the caller (never silent). A `get_*` that returns `Unavailable` *during* the loop surfaces immediately (the retry budget covers write conflicts, not transient read failures).
- **Consistency:** remote strong (same-region); cache bounded-stale; correctness rests on the write-time CAS, not cache freshness (D7).
- **Cache:** push-through on local writes; lazy TTL on reads (§7.2). **Startup sync = list keys only** (cheap `ListObjectsV2`); content fetched lazily on first read — never download the whole corpus cold.
- **Phase ordering (correctness, not deferral):** concurrent `phase.add` both recompute `order` on `project.md`/phase files → the second stomps the first. **Resolve in Phase 0:** either CAS `project.md` writes too, or make `order` immutable-by-insertion (sort by creation, never explicitly reorder). Pick one.

## 9. Rollout / implementation plan

Validation gate sits after Phase 1. Critical path to the gate: **0 → 2 → 1 (+ 3a) → validate.** Phases 4+ are the product, gated on Phase 1 *and* the broader ship-cloud timeline (§12).

| Phase | Goal | High-level tasks | Depends on | Gate | ~wLOC |
|---|---|---|---|---|---|
| **0 — Store abstraction** | Swappable storage | Extract async `Store` trait; `Version`/`Versioned<T>`; `FsStore: Store` (SHA-256 CAS under mutex); `Arc<dyn Store>`; move `search` to service layer; phase-ordering decision (§8); test parity | — | none — **ships now (no-regret)** | ≤200 |
| **2 — Artifact sharding** | Remove hot file | `artifacts.jsonl` → `artifacts/<id>.json`; LAYOUT.md; migrate; `artifact.list` prefix scan (both stores) | 0 | pre-gate | ≤200 |
| **1 — S3 backend + CAS** | Prove shared multi-writer | `S3Store` (GET/PUT + `If-Match`); path→key under tenant prefix; CAS 3-way retry (§8); **concurrent stress test**; `dossier export` (11a) | **0, 2** | **GO/NO-GO** | 400–600 |
| **3a — Warm cache (spike)** | Read latency for the spike | local working copy + push-through on write | 0 | pre-gate | ≤150 |
| **3b — Cache hardened** | Production cache | TTL refresh (§7.2), offline/degraded (§7.6), startup list-only sync | 1 | post-gate | ~250 |
| **4 — Remote MCP transport** | Reach over network | rmcp HTTP/SSE alongside stdio; bearer-token middleware (stub identity) | 3b | post-gate | ~300 |
| **5 — Identity & auth** | Real seats | token → `{tenant, actor}`; authz per call; seat/token mgmt | 4 | post-gate | — |
| **6 — Multi-tenancy** | Many teams, isolated | per-tenant Store routing; provisioning; isolation tests | 5 | post-gate | — |
| **7 — Audit & history** | who/what/when | change-feed or S3 versioning; revisit `last_updated_by` | 6 | post-gate | — |
| **8 — Ops/infra/security** | Run it safely | deploy, secrets, backups, rate limits, tenant deletion, security review | 6 | post-gate | — |
| **9 — Billing & accounts** | Charge for it | subscription, metering, accounts | 6 | post-gate | — |
| **10 — Web UI** *(optional)* | Non-CLI members | read-mostly team view | 6 | demand-gated | — |
| **11 — Migration tooling** | Local ↔ cloud | `11a: dossier export` (S3→disk, part of Phase 1); `import`/`sync` | 2 | 11a pre-gate | — |

## 10. Open questions

1. **Tenant model** (D6/§7.5): routing vs field — proposed routing; confirm before Phase 5.
2. **`search` at scale** over the cache: fine early; a derived index may be needed later (deferred).
3. **rmcp HTTP/SSE + auth hooks** (Phase 4): validate the transport supports the middleware before committing the phase.
4. **Commitment level**: committed direction, or exploration? Gates investment past Phase 1.

## 11. Validation plan (the gate)

1. **This TDD** → reviewed, decisions locked.
2. **Phase 0** — async `Store` trait (no-regret refactor PR).
3. **Phases 2 + 1 (+ 3a)** — sharding, then `S3Store`, then the spike.
4. **Gate signal (binary, baseline-free):** the concurrent stress — two agents claim different tasks while a third links artifacts, plus one agent races a claim (7.4) — completes with **0 lost updates across N=100 runs.** That's the engineering gate. The qualitative "does shared memory cut collisions" question is a *product* hypothesis, validated separately by dogfooding, not the gate.
5. **Only if green:** Phases 3b → 9.

## 12. Strategic note

Not a standalone SaaS bet — the **team-tier of the cloud workbench already being built** (ship-cloud + huddle + cloud-driver vision). dossier-cloud is the shared *state plane* for that multi-agent story. Phase 1 proves *the backend works*; Phases 4–9 (auth, multi-tenant, billing) are gated on the broader ship-cloud timeline — *when we build the product around it* — not on Phase 1 alone.
