# dossier Cloud — Technical Design Document

**Status:** draft / proposal — **NOT a build commitment.** The artifact we decide *from*, and the plan we'd execute *if* validation (Phase 1) earns it.
**Owner:** @itsHabib
**Date:** 2026-05-29
**Supersedes:** the initial RFC cut of this file (PR #59 v1).
**Related:** [vision.md](../../vision.md) (this deliberately re-opens v0 non-goals — multi-writer, audit, identity, web UI), [PROTOCOL.md](../../../PROTOCOL.md) (data model), [LAYOUT.md](../../../LAYOUT.md) (on-disk format).

> **Reviewers — focus areas:** (1) the `Store` trait API contract in §6, (2) the CAS write flow + retry policy in §7.1/§8, (3) the consistency/staleness model in §8, (4) the phase sequencing + validation gate in §9, (5) whether tenant isolation by routing (§5, §7.5) is the right call vs a tenant field.

---

## 1. Problem & hypothesis

dossier today is local, single-writer, markdown-on-disk, stdio MCP — excellent for one developer. The bet: a **team** running a fleet of agents + humans needs a *shared* project-memory plane — one corpus many seats read and write so they don't collide or lose context. That shared, hosted plane is the paid product.

This TDD designs the backend that makes the corpus **remote and multi-writer**, behind dossier's existing storage seam, following Terraform's remote-state playbook — plus the rollout plan to get there. Go/no-go is gated on Phase 1 (§9).

## 2. Goals & non-goals

**Functional**
- MCP verbs + the four primitives are **unchanged at the boundary** — clients don't know where the corpus lives.
- Corpus lives in **remote storage**; multiple clients on multiple machines share one corpus safely.
- **Concurrent writes are safe** — no lost updates, no corruption.
- Storage is **pluggable**: local `FsStore` stays the default; remote is an alternate backend.
- *(Post-validation)* per-team isolation + auth; a seat sees only its team's corpus.

**Non-functional**

| Property | Target |
|---|---|
| Read latency | interactive — warm-cache `list`/`get` under agent-loop tolerances |
| Durability | corpus is source of truth; no loss on crash or concurrent write |
| Consistency | remote: strong read-after-write (S3 guarantees this); local cache: bounded staleness, with write-time CAS as the authority (§8) |
| Portability | `dossier export` reproduces the identical on-disk corpus `FsStore` reads — anti-lock-in is a selling point |
| Operability | minimal infra; cheap per team |
| Security | per-tenant isolation; authn + authz; no cross-tenant leakage |

**Non-goals (this doc):** billing/pricing/GTM mechanics; RAG/vectors; conflict-detection semantics beyond write-safety; web UI design.

## 3. Architecture overview

```
        MCP clients (Claude, cursor, ship, humans)
                      │  stdio (local)  |  HTTP/SSE + token (cloud)
                      ▼
            ┌──────────────────────┐
            │   MeshService (verbs) │   unchanged verb logic + state machine
            └──────────┬───────────┘
                       │  Store trait  (the seam)
        ┌──────────────┼─────────────────┐
        ▼              ▼                  ▼
   FsStore        S3Store (+ warm     [GitStore]   ← optional, later
   (local disk)    local cache)        (deferred)
                       │
                       ▼
              S3 / R2 / GCS  (per-tenant prefix)
```

The only new structural idea: **everything above the `Store` trait is backend-agnostic.** The verbs, the task state machine, and the MCP surface don't change. A cloud deploy = `S3Store` + remote transport + auth in front; a local install = `FsStore` over stdio, exactly as today.

## 4. Key decisions (incorporating review)

| # | Decision | Rationale |
|---|---|---|
| D1 | Extract a `Store` trait; `FsStore` implements it | The backend seam (Terraform local-vs-remote). No-regret; improves the codebase regardless. |
| D2 | **Remote substrate = object store (S3/R2/GCS) + per-object CAS** — not git | git's "conflict-free by construction" fails on `artifacts.jsonl` + shared `project.md`/phase files (review finding). S3 CAS is unambiguous; git stays a later option behind the trait. |
| D3 | **Shard artifacts**: `artifacts.jsonl` → `artifacts/<art_id>.json` (one object each) | Kills the hot shared-file contention; one CAS-free PUT per artifact (unique id); `artifact.list` = prefix scan. Applies to FsStore too, keeping formats identical. |
| D4 | Concurrency = per-object CAS (`If-Match` on ETag), bounded retry w/ backoff | Fine-grained; two seats on different tasks never contend. §8. |
| D5 | Read path = warm local working copy synced from remote | `list`/`search` scan; per-query remote scans are slow/pricey. Mirrors Terraform download→operate→writeback. |
| D6 | Tenant isolation by **routing** (per-tenant prefix/bucket + credentials), not a `tenant` data field | One `Store` instance = one corpus = one team. Isolation at the routing/credential layer is simpler and safer than field-filtering. |
| D7 | Consistency: remote strong; cache bounded-stale; **claim safety enforced by write-time CAS**, not by cache freshness | A stale `list` is safe because the authoritative check is the CAS on the mutating write (§8). |

## 5. Data model

**On-disk corpus shape: unchanged** ([LAYOUT.md](../../../LAYOUT.md)), except artifacts sharding (D3):

```
.dossier/
projects/<slug>/
  project.md            # frontmatter + description
  phases/NN-<slug>.md   # ordered phases
  tasks/tsk_<ulid>.md   # one per task (+ ## Notes log)
  artifacts/art_<ulid>.json   # NEW: one object per artifact (was artifacts.jsonl)
```

**Remote (S3) key layout** — paths mirror the tree under a per-tenant prefix:

```
s3://<bucket>/tenants/<tenant>/projects/<slug>/project.md
s3://<bucket>/tenants/<tenant>/projects/<slug>/tasks/tsk_<ulid>.md
s3://<bucket>/tenants/<tenant>/projects/<slug>/artifacts/art_<ulid>.json
```

**Versioning:** no stored version field — the version token is **intrinsic** (S3 object ETag; FsStore: content hash). Carried in `Versioned<T>` at the API layer (§6) and used for CAS.

**Identity/tenancy:** no `tenant` field in the documents (D6). The tenant is the S3 prefix; the bearer token resolves to a tenant + actor. The existing `actor` / `assignee` strings carry the seat identity, unchanged.

## 6. API contract

### 6.1 The `Store` trait (the real new internal API)

Reads return the value plus its version; writes take the expected version and fail on mismatch. CAS lives in the trait; the verb layer drives the read-modify-write loop.

```rust
/// Opaque version token: S3 ETag, or FsStore content hash.
pub struct Version(String);
pub struct Versioned<T> { pub value: T, pub version: Version }

pub enum StoreError { NotFound, Conflict /*412*/, Unavailable, Invalid(String), Io(..) }

pub trait Store: Send + Sync {
    // ---- reads (return current version for later CAS) ----
    fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError>;
    fn list_projects(&self, f: ProjectFilter) -> Result<Vec<Project>, StoreError>;
    fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError>;
    fn list_phases(&self, f: PhaseFilter) -> Result<Vec<Phase>, StoreError>;
    fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError>;
    fn list_tasks(&self, f: TaskFilter) -> Result<Vec<Task>, StoreError>;
    fn list_artifacts(&self, f: ArtifactFilter) -> Result<Vec<Artifact>, StoreError>;
    fn search(&self, q: SearchQuery) -> Result<Vec<SearchHit>, StoreError>;

    // ---- writes (CAS: expected = None means create-only) ----
    fn put_project(&self, p: &Project, expected: Option<Version>) -> Result<Version, StoreError>;
    fn put_phase(&self,   p: &Phase,   expected: Option<Version>) -> Result<Version, StoreError>;
    fn put_task(&self,    t: &Task,    expected: Option<Version>) -> Result<Version, StoreError>;
    fn put_artifact(&self, a: &Artifact) -> Result<(), StoreError>; // unique id ⇒ create-only, no CAS
}
```

- `FsStore` implements this with the current mutex + atomic-rename writes; `Version` = content hash; CAS = compare-hash-before-rename under the lock.
- `S3Store` implements `get_*` via GET (capture ETag), `put_*` via PUT with `If-Match: <etag>` (or `If-None-Match: *` for create); `Conflict` on HTTP 412.
- The server changes `store: Arc<FsStore>` → `store: Arc<dyn Store>`. **No verb signatures change.**

### 6.2 Backend & transport config

- `DOSSIER_BACKEND = fs | s3` (default `fs`); S3 needs bucket + region + creds + tenant prefix.
- `DOSSIER_TRANSPORT = stdio | http` (default `stdio`); `http` exposes rmcp's streamable-HTTP/SSE endpoint.

### 6.3 Auth surface (Phase 5, sketch)

- Bearer token per seat → resolves to `{ tenant, actor }`.
- Server routes to that tenant's `Store` (D6) and stamps `actor` on writes.
- authz: every call scoped to the token's tenant; cross-tenant access impossible by construction (different prefix/credentials).

## 7. Key flows

### 7.1 Write (e.g. `task.claim`) — read-modify-write with CAS
```
1. get_task(id)                       → (task, v0)
2. validate transition (state machine, unchanged)
3. mutate in memory (status=claimed, assignee=actor)
4. put_task(task, expected=v0)        → v1   |  Conflict(412)
5. on Conflict: re-read (→ v_n), re-validate, re-apply, retry (§8)
6. on success: update warm cache to v1
```
The state machine runs in step 2 exactly as today; CAS only guards the persistence.

### 7.2 Read (`task.list` / `search`) — warm cache
```
1. serve from local working copy (synced from remote)
2. cache refresh: on local write (push-through) + lazy TTL on reads (§8)
3. claim decisions made off a slightly-stale list are SAFE — the claim
   write (7.1) re-checks via CAS, so a lost race just retries.
```

### 7.3 `artifact.link` — sharded, contention-free
```
1. mint art_<ulid>
2. put_artifact(a)  → PUT artifacts/art_<ulid>.json  (If-None-Match:* )
   unique id ⇒ never collides, no CAS retry loop, no hot file
3. artifact.list = prefix scan of artifacts/
```

### 7.4 Concurrent claim race (two agents, same task)
```
A: get_task(v0) ─┐
B: get_task(v0) ─┤  both read same version
A: put(exp=v0) ─→ 200, v1  (A wins)
B: put(exp=v0) ─→ 412       (stale) → B re-reads v1 → task already claimed
                              → B surfaces "already claimed" (state machine), no retry
```

### 7.5 Tenant routing (Phase 6)
```
token → { tenant=T, actor=seat } → Store bound to s3://bucket/tenants/T/
all verbs operate within that prefix; no tenant id in payloads
```

### 7.6 Offline / degraded (policy)
```
remote reachable    → normal
remote unreachable  → reads: serve warm cache, flagged possibly-stale
                       writes: fail fast with typed Unavailable (NO queueing in v1 —
                       queued writes risk lost-update semantics)
```

## 8. Concurrency & consistency

- **CAS:** every mutating write carries the expected version; backend rejects on mismatch (S3 `If-Match` → 412).
- **Retry policy:** on `Conflict`, re-read → re-validate against the state machine → re-apply → re-put. Max **5** attempts, exponential backoff with jitter (e.g. 25ms·2^n ± jitter). On exhaustion → return a typed `conflict` error to the MCP caller (don't silently drop). Most "conflicts" are actually terminal state-machine outcomes (7.4) and resolve in one re-read, not a livelock.
- **Consistency:** remote is strongly consistent (S3 read-after-write since 2020). The warm cache is bounded-stale; **correctness never depends on cache freshness** because the authoritative check is the write-time CAS (D7). Cache staleness only affects how often a write has to retry, not whether it's safe.
- **Cache refresh:** push-through on local writes; lazy TTL (e.g. 2–5s) on reads; full resync on startup. Tunable; not correctness-critical per above.

## 9. Rollout / implementation plan

Phases are sequenced; **the validation gate sits after Phase 1.** Phases 0–1 (and the slices of 2–3 needed to spike) are cheap and partly no-regret; everything from Phase 4 on is the "real product" and only happens if Phase 1 proves the thesis.

| Phase | Goal | High-level tasks | Depends on | Gate |
|---|---|---|---|---|
| **0 — Store abstraction** | Make storage swappable | Extract `Store` trait; `Version`/`Versioned<T>`; `FsStore: Store` (hash-based CAS under existing mutex); server holds `Arc<dyn Store>`; full test parity | — | none — **ships to main now (no-regret)** |
| **1 — S3 backend + CAS** | Prove shared remote multi-writer corpus | `S3Store: Store` (GET/PUT + `If-Match`); path→key mapping under tenant prefix; CAS retry loop (§8); two-client + concurrent-claim/artifact **stress test** | 0 | **GO/NO-GO.** If shared remote memory isn't sticky, stop here. |
| **2 — Artifact sharding** | Remove the hot shared file | `artifacts.jsonl` → `artifacts/<id>.json`; update LAYOUT.md; migrate existing corpora; `artifact.list` = prefix scan (both stores) | 0 | pre-validation (needed for clean Phase 1 stress) |
| **3 — Warm cache + sync** | Interactive read latency over remote | Local working copy; refresh policy (§8); offline/degraded policy (7.6) | 1 | pre-validation (spike-grade ok; harden later) |
| **4 — Remote MCP transport** | Reach the server over the network | rmcp HTTP/SSE transport alongside stdio; bearer-token middleware (stub identity) | 3 | post-validation |
| **5 — Identity & auth** | Real seats | token → `{tenant, actor}`; authz per call; seat/token mgmt | 4 | post-validation |
| **6 — Multi-tenancy** | Many teams, isolated | per-tenant Store routing (prefix/bucket); provisioning; isolation tests | 5 | post-validation |
| **7 — Audit & history** | "who changed what, when" | change-feed or S3 versioning surfaced as audit; revisit deferred `last_updated_by` | 6 | post-validation |
| **8 — Ops, infra & security** | Run it safely | deploy, secrets, backups, rate limits, tenant deletion, security review | 6 | post-validation |
| **9 — Billing & accounts** | Charge for it | subscription, metering, account mgmt | 6 | post-validation |
| **10 — Web UI** *(optional)* | Non-CLI team members | read-mostly team view | 6 | demand-gated |
| **11 — Migration tooling** | Local ↔ cloud, portability | `dossier export` (S3→disk = FsStore corpus), `import`/`sync` | 2 | spans; export usable early |

**Critical path to a decision:** 0 → 2 → 1 (+ thin 3) → **validate**. That's the only work we commit to before the gate.

## 10. Open questions / risks

1. **Tenant model** (D6/§7.5): routing vs field — proposed routing; confirm before Phase 5.
2. **`search` at scale** over S3: warm-cache scan is fine early; a derived index may be needed later (deferred, flag it).
3. **rmcp HTTP/SSE maturity + auth hooks** (Phase 4): validate the transport supports the middleware we need before committing the phase shape.
4. **Phase ordering metadata** under concurrency: `phase.add` rewrites `order`; needs the same CAS discipline or an immutable-order scheme (review note). Resolve in Phase 0/2.
5. **Commitment level**: is this committed, or exploration? Gates investment past Phase 1.

## 11. Validation plan (the gate)

1. **This TDD** → reviewed, decisions locked.
2. **Phase 0** — `Store` trait (no-regret refactor PR).
3. **Phase 1 (+2,3 slices)** — S3 backend; spike a concurrent stress: two agents claim different tasks while a third links artifacts; one agent races a claim (7.4).
4. **Signal:** does shared remote memory across agents measurably cut collisions / lost context vs the local-only baseline? (Weak positive evidence already: multiple agents share one local `dossier-state` today.)
5. **Only if yes:** Phases 4→9.

## 12. Strategic note

Not a standalone SaaS bet — the **team-tier of the cloud workbench already being built** (ship-cloud + huddle + the cloud-driver vision). dossier-cloud is the shared *state plane* for that multi-agent story; it likely emerges alongside that work, not as a separate sprint.
