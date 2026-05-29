# dossier Cloud — backend design (RFC)

**Status:** draft / proposal — **NOT a build commitment.** This is the artifact we decide *from*.
**Owner:** @itsHabib
**Date:** 2026-05-29
**Related:** [vision.md](../../vision.md) — note this deliberately revisits v0 non-goals (multi-writer, audit log, identity, web UI). Those were the right cuts for a solo tool; a team product re-opens them.

## 1. Problem & hypothesis

dossier today is local, single-writer, markdown-on-disk, stdio MCP — excellent for one developer. The bet: a **team** running a fleet of agents + humans needs a *shared* project-memory plane — one corpus that many seats read and write so they don't collide or lose context. That shared, hosted plane is the thing a team would pay for.

This doc designs the **backend** that makes the corpus remote and multi-writer — following Terraform's remote-state playbook, behind dossier's existing storage seam. It does **not** decide go/no-go; it defines the design so we can spike cheaply and decide on evidence.

Non-goal of this doc: billing, pricing, GTM. Those come after the thesis is validated.

## 2. Functional requirements

- MCP verbs + the four primitives are **unchanged at the boundary** — clients don't know or care where the corpus lives.
- The corpus lives in **remote storage**; multiple clients on multiple machines share one corpus.
- **Concurrent writes are safe** — no lost updates, no corruption.
- Storage is **pluggable**: local `FsStore` stays the default (free/local path); remote is an alternate backend.
- *(Later)* **per-team isolation** + **auth** — a seat sees only its team's corpus.

## 3. Non-functional requirements

| Property | Target |
|---|---|
| Read latency | interactive — warm-cache `list`/`get` fast enough for agent loops |
| Durability | corpus is the source of truth; no loss on crash or concurrent write |
| Portability | user can extract their data (plain text / git) — anti-lock-in is a *selling point*, not an afterthought |
| Auditability | who changed what, when (cheap if git-backed) |
| Operability | minimal infra; cheap to run per team |
| Security | per-tenant isolation; authn + authz; no cross-tenant leakage |

## 4. Scope

**This effort (to a go/no-go):**
- Extract a `Store` trait; `FsStore` implements it (refactor, no behavior change).
- One remote `Store` impl as a spike — enough for two clients to share a corpus.
- A concurrency model (per-object compare-and-swap).

**Later, only if validated:** remote MCP transport + auth, multi-tenancy, web UI, billing.

**Out (unchanged from vision for now):** RAG / vectors; conflict-detection semantics beyond write-safety; anything not needed to prove the shared-memory thesis.

## 5. Engineering decisions & trade-offs

### 5.1 Backend abstraction = a `Store` trait
Today the server holds a concrete `Arc<FsStore>` (`server.rs:42`). We lift that to a `Store` trait — exactly Terraform's local-vs-remote backend split. Everything above storage is untouched.
**Decision: yes.** This is step 1 regardless of backend choice, and it improves the codebase even if cloud never ships (no-regret).

### 5.2 Remote substrate — git vs object store (S3). **THE open fork.**

| | Hosted **git** repo | **Object store** (S3) |
|---|---|---|
| History / audit | free (commits = `last_updated_by`) | build it yourself (bucket versioning helps) |
| Portability pitch | `git clone` your data and walk — strong trust story | needs an export tool |
| Concurrency | merge conflicts on same-file edits (ugly for automation) | per-object CAS (clean) |
| Ops complexity | git hosting + auth per tenant | bucket + IAM |
| "dossier's soul" | markdown stays diffable & portable | markdown becomes opaque blobs |

**Recommendation (proposed, open): git-backed — *if* we keep the write path conflict-free by construction** (each agent writes only its own claimed tasks → concurrent same-file edits don't happen → merges stay trivial). If that constraint feels fragile, fall back to **S3 + CAS**. Either sits behind the same `Store` trait, so the choice is reversible.

### 5.3 Concurrency = per-object compare-and-swap, not a global lock
The corpus is many small files (one per task/phase). A write does CAS on that one object (git: expected parent commit / S3: `If-Match` on the ETag) and retries on conflict. Two seats on different tasks never contend — **finer-grained than Terraform's single global state lock.**

### 5.4 Read path = warm local working copy synced from remote
`list` / `search` scan the corpus; scanning remote objects per query is slow and pricey. The server keeps a synced local copy and serves reads from it (mirrors Terraform's download → operate → write-back). Remote = durability + sync point; disk = query cache.

### 5.5 Transport = remote MCP (HTTP/SSE) + auth
Today's stdio transport is local-only. Cloud needs a remote MCP endpoint with a per-seat token.

### 5.6 `artifacts.jsonl` append
Object stores have no append. Either read-modify-write the file under CAS, or shard artifacts to one-object-each like every other primitive. (Git appends fine.)

## 6. API

- **MCP surface: unchanged** — that's the whole point (`project.*`, `phase.*`, `task.*`, `artifact.*`, `search`).
- **New config:** backend selection + connection, e.g. `DOSSIER_BACKEND=fs|git|s3` plus creds. Local default stays `fs`.
- **New (later):** auth — per-seat token, team scoping enforced on every call.
- **Internal `Store` trait** (the real new API): the CRUD the verbs already need — `get`/`list` + `create`/`update` per primitive, `append_artifact`, `search` — but **versioned for CAS**: reads return a version/etag; writes take the expected version and fail on mismatch.

## 7. Data model

- **On-disk corpus shape: unchanged** ([LAYOUT.md](../../../LAYOUT.md)) — `projects/<slug>/{project.md, phases/, tasks/, artifacts.jsonl}`.
- **Remote mapping:** the same tree as S3 keys (paths → keys), or the same tree inside a per-team git repo.
- **New fields for cloud:**
  - `tenant` / `team` id — one corpus per team; the isolation boundary.
  - per-object `version` / `etag` — powers CAS (likely intrinsic: a git sha or an S3 ETag, not a stored field).
  - `last_updated_by` / audit — deferred in v0 ("git history covers it"); **free if git-backed**, so it returns here.

## 8. How we decide (validation plan)

1. **This doc.** ✅
2. **Step 1 — extract the `Store` trait.** No-regret refactor PR.
3. **Step 2 — spike one remote backend.** Two clients share a corpus; *feel* it.
4. **Signal:** does shared remote memory across agents actually cut collisions / lost context? (Weak evidence already exists — multiple agents share one local `dossier-state` today.)
5. **Only then:** auth → multi-tenancy → billing.

## 9. Open questions (need an operator call)

1. **git vs S3** (§5.2) — the one decision that shapes everything downstream.
2. **Commitment level** — committed direction, or exploration? Gates how much we invest past Step 1.
3. **Which deferred non-goals return for the team tier** — conflict detection? audit? web UI? A team product likely needs some; vision cut them for solo.

## 10. Strategic note

This is not a standalone SaaS bet — it's the **team-tier of the cloud workbench already being built** (ship-cloud + huddle + the cloud-driver vision). dossier-cloud is the shared *state plane* for that multi-agent story. It likely emerges *alongside* that work rather than as a separate sprint.
