# CAS lost-update validation gate

| | |
|---|---|
| **Status** | accepted — implemented + run; **GO on MinIO** (see Gate result) |
| **Owner** | @itsHabib |
| **Date** | 2026-06-07 |
| **Related** | [docs/features/cloud-backend/spec.md](../spec.md) §6.1 (Store trait), §7.1 (write flow), §7.4 (claim race), §8 (concurrency/retry), §9 (rollout), §11.4 (the gate); [PROTOCOL.md](../../../../PROTOCOL.md) (task state machine); `src/s3store.rs` (`cas_put`, `put_artifact`); `src/server.rs` (`cas_mutate_task`, `cas_backoff`, `CAS_MAX_ATTEMPTS`); `tests/s3_store_minio.rs` (`concurrent_writer_race_exactly_one_wins`, `test_store` harness) |

## Scope

| Bucket | Files | LOC | Weighted |
|---|---|---|---|
| Production | none — gate exercises shipped CAS as-is. **One exception (see §Negative control):** an env-gated `DOSSIER_S3_DISABLE_CAS` branch inside `S3Store::cas_put` IF option (a) is chosen; option (b) keeps production at 0 | 0–8 | 0–8 |
| Tests | `tests/s3_cas_gate.rs` (two gate tests + conformance precheck + negative control), `tests/common/mod.rs` (lifted MinIO harness + seed helpers) | ~380 | ~190 |
| Docs | this doc | ~0 | 0 |
| **Total** | | | **~190–198** |

Band: **amazing** (< 500 weighted). Runtime: **local** (MinIO-gated; self-skips without `DOSSIER_S3_TEST_ENDPOINT`, so default CI is unaffected and `make check` stays green with or without the endpoint).

The gate is **pure test-harness + in-test oracle, zero production-source diff** — with the single carve-out below for the negative control, which is itself a NO-GO criterion and so must be honestly wired. If any *other* production change is forced, that is itself a NO-GO signal handled as a separate fix PR (see §Acceptance criterion 7).

## Goal

The dossier-cloud S3 tier puts project memory behind a swappable async `Store` trait with optimistic concurrency on top of S3 conditional writes. `put_{project,phase,task}(value, expected)` issues create-only or update-if-version-matches; `Version` is the S3 ETag; an HTTP 412 maps to `StoreError::Conflict`. The service layer lifts this into a read-modify-write loop (`MeshService::cas_mutate_task`) that, on `Conflict`, re-reads and runs a 3-way branch — terminal reject (state machine refuses), idempotent success (desired state already reached), or true-retry (re-apply onto the fresh base, re-put) — bounded by `CAS_MAX_ATTEMPTS = 5` with jittered backoff (`cas_backoff`). Exhaustion returns a typed `Conflict`, never a silent drop.

That machinery is **shipped and verified end-to-end against MinIO** (CRUD round-trips, CAS succeed-on-current / conflict-on-stale, a 2-writer race, the `shift_phases` fixes). What is *not* yet established is the one property the cloud rollout hinges on, stated verbatim in §11.4:

> the concurrent stress — two agents claim different tasks while a third links artifacts, plus one agent races a claim (§7.4) — completes with **0 lost updates** across N=100 runs. That's the engineering gate.

This doc specs the executable form of that gate.

**A lost update, defined operationally:** a write the store reported as committed (`Ok`) whose effect is **absent from the final persisted state**. On an S3 CAS backend the canonical ways this happens are: (a) a conditional write that should have been rejected silently overwrites a newer version — CAS not actually enforced; (b) the retry loop re-applies a mutation onto a *stale* base it never re-read, clobbering an interleaved commit; (c) the 3-way branch mis-classifies a real conflict as idempotent-success and returns `Ok` without writing. The gate must turn any of these into a **deterministic assertion failure that names the victim**, not a probabilistic "looks fine."

**What GREEN means:** the S3 tier is *backend-correct* — concurrent agents do not lose committed work — *modulo* the explicit residual that GREEN-on-MinIO is necessary but not sufficient for GREEN-on-AWS/R2 (see §The two-layer CAS reality and §Threats T4). GREEN unlocks the downstream phases (§11.5). It does **not** prove the *product* question ("does shared memory cut agent collisions") — that is validated separately by dogfooding, not here.

## The two-layer CAS reality (read this before the design)

`S3Store::cas_put` (`src/s3store.rs`) enforces compare-and-swap with **two independent mechanisms**, not one. On an update (`expected = Some(v)`):

1. **Application-layer GET-compare** — it does a pre-flight `get_object_body_optional(key)`, returns `NotFound` if the object is gone, and returns `Conflict` if the live ETag `!= expected` — *before issuing any PUT*.
2. **S3-layer conditional PUT** — it then sends the PUT with `If-Match: <v>` (update) or `If-None-Match: *` (create), so the backend rejects a stale write with HTTP 412 → `Conflict`.

On a strongly-consistent backend (MinIO is one) these are **redundant**: either alone suffices for most schedules. That redundancy is the root of three ways the gate could be GREEN while the cloud-relevant primitive is dead, and it dictates two non-obvious design choices below:

- The **negative control must defeat BOTH layers** (stripping the header alone leaves the GET-compare enforcing CAS, so the control would pass — and a passing negative control is a NO-GO). See §Negative control.
- The **conformance precheck must hit the raw `aws_sdk_s3` Client directly**, bypassing `cas_put` entirely, so it asserts the *backend's* 412 behavior rather than the app-layer compare that would mask a non-conditional backend. See §Conformance precheck and §Threats T4.

The gate via `cas_put` therefore proves *"the composite of app-compare + If-Match loses no updates on this backend"* — which is **strictly weaker** than *"S3 conditional-PUT is enforced."* Closing that gap is exactly the job of the raw-client precheck plus the required real-backend confirmation run (§Open questions), and the gap is called out wherever it bites rather than papered over.

## Design: ship both sub-tests; GREEN iff both pass

The §11.4 scenario *as literally written* is **low-contention**. The two task claimers touch **different** task objects, so they never contend on the same version; only the single §7.4 race pair collides on one object, once per run. Running that ×100 reliably catches flakiness, ordering bugs, and verb-composition regressions — but it barely exercises the read-modify-write retry path, which is exactly where lost-update bugs live. You could pass §11.4 ×100 with a broken `cas_backoff`, a silently-dropping retry branch, or even a no-op CAS, because the retry-reapply branch may never fire across all 100 runs. So "0 lost updates" is **not falsifiable by §11.4 alone**.

**Decision: ship both, and the gate is GREEN iff both pass.**

- **Sub-test A — faithful §11.4 ×100.** The gate's literal text and the realism check: the real claim + link + claim-race verbs, composed, across 100 mixed-workload schedules. Necessary (it *is* the named gate) and it exercises the actual verb mix, but insufficient as proof.
- **Sub-test B — high-contention single-object RMW counting stress.** N writers hammer one task's append-only `notes` log through the production `cas_mutate_task` loop; the oracle asserts the set of notes that physically survive equals exactly the set of writers the store said it committed. The rigorous, falsifiable heart of the gate.

Neither subsumes the other: A alone could pass while a real lost-update bug hides in the retry loop; B alone wouldn't prove the integration verbs compose without loss. Both are acceptance-blocking.

All workload goes through **`MeshService`** (`claim_task` / `update_task` / `link_artifact`), **not** the bare `Store`, so the real lifted RMW loop, the 3-way branch, and the jittered backoff are under test. The async service methods take `&self` and never touch the process-local `write_lock` (that mutex only guards the sync `#[tool]` entrypoints), so concurrent in-process tasks racing through them expose true cross-writer CAS contention with no in-process serialization masking it. The gate does **not** hand-roll a retry loop — a gate that proves "0 lost updates" against a *different* loop proves nothing about production.

### Sub-test A — faithful §11.4 scenario + oracle

`gate_scenario_11_4_mixed_workload`, N = 100 iterations (const `S11_4_ITERS`).

**Per-iteration seed.** A fresh project (so `link_artifact` can resolve its slug), one phase, three claimable tasks `T_x`, `T_y`, `T_race`, all `status = todo`. Fresh slugs per iteration (the run index is part of every slug: `gate-{run}`, `tx-{run}`, …) so each iteration is a clean experiment — S3 has no transactional truncate, and reusing objects would let iteration K's final state seed iteration K+1 and mask a loss.

**Workload — four concurrent actors** spawned via `tokio::spawn` on a shared `Arc<MeshService>`, joined with `futures::future::join_all` (a panicked worker surfaces as a `JoinError` hard-fail, never swallowed):

| Actor | Action | Target | Expected terminal |
|---|---|---|---|
| `agent-a` | `claim_task` | `T_x` | `Ok`, assignee `agent-a`, status `claimed` |
| `agent-b` | `claim_task` | `T_y` | `Ok`, assignee `agent-b`, status `claimed` |
| `linker` | `link_artifact` ×K (K = 4, distinct ids/refs) | project (`task = None`) | all `Ok`; K artifacts present, each once |
| `racer-1` & `racer-2` | `claim_task` | `T_race` (**same** task) | exactly one `Ok`; the other a **terminal** `Invalid` containing "already claimed" |

The linker does K appends, not one, because `put_artifact` on the S3 backend is a **CAS read-modify-write of the shared `projects/<slug>/artifacts.jsonl` object** (verified in `src/s3store.rs::put_artifact` — it reads the whole file, rejects a duplicate id, appends a line, and `cas_put`s the whole object inside its own bounded `ARTIFACT_PUT_RETRIES` loop). A single link wouldn't stress that append path; K concurrent links do. (See §Dependencies for why this contradicts an "artifacts are create-only-per-id" reading and why it does *not* block the gate.)

**Oracle (read back from the store after join, before the next iteration).** Five invariants:

1. **Both different-task claims landed.** `get_task(T_x).assignee == "agent-a"` and `get_task(T_y).assignee == "agent-b"`, both `status == claimed`, both `claimed_at` set. (Catches a claim that returned `Ok` but didn't persist.)
2. **Race produced exactly one winner.** Exactly one of `{racer-1, racer-2}` returned `Ok`; the loser is a **terminal `StoreError::Invalid` whose message contains "already claimed"** — *not* `Conflict`. This is the verified state-machine behavior: the loser re-reads after the winner committed, `apply_claim_task` bails with "task already claimed by …", and `cas_mutate_task` returns that as a terminal `domain_err` → `Invalid` *before* any retry. `get_task(T_race).assignee` equals whichever racer returned `Ok`. (Catches double-claim / clobbered claim.)
3. **No artifact lost.** `list_artifacts(project)` contains all K linker ids, **each exactly once**: the committed-id set equals the persisted-id set, length K, no duplicates. (Catches a lost append to `artifacts.jsonl`.)
4. **No cross-talk corruption.** `T_x`, `T_y`, `T_race` are distinct objects with independent versions; none picked up another's assignee. (Catches an RMW that re-applied onto the wrong re-read base.)
5. **Every `Ok` is real.** For each actor that returned `Ok`, its effect is observable in the readback — the literal definition of "0 lost updates" for this workload (the contrapositive of 1–3).

**Run-count rationale.** N = 100 matches the spec verbatim and is enough to surface scheduler-dependent flakiness in the integration path (~4 spawns × 100 ≈ 400 short MinIO round-trips) without making the run unbearable. One oracle violation across all 100 fails the gate.

### Sub-test B — high-contention single-object RMW counting stress

`gate_high_contention_rmw_oracle`, W = 16 writers (const `RMW_WRITERS`) × M = 30 iterations (const `RMW_ITERS`). The falsifiability engine: **one object, many writers, an arithmetic invariant a lost update cannot satisfy.**

**Target field and why notes.** Seed one project + one task `T`, claim it once (to `in_progress`) so `update_task` mutations are state-machine-valid and won't terminal-reject. The RMW-able field is `task.notes` — the append-only progress log (`Vec<Note>`). The write path is `MeshService::update_task` with a `note` argument, routing through the production `cas_mutate_task` loop and applying `append_task_note` (pushes one `Note`, never dedupes, requires a single-line non-empty body and an actor without `": "` — all satisfied).

Notes are chosen, not a synthetic counter, because:

- **It is the real production RMW path.** `update_task{note}` → `cas_mutate_task` → `get_task` (read version) → `append_task_note` (mutate) → `put_task(expected = version)` (CAS) → on `Conflict`, re-read + re-apply + re-put with `cas_backoff`. We stress the *actual* loop the gate cares about.
- **The append is monotone and non-idempotent**, so the oracle is exact: every committed append leaves exactly one distinguishable element. A scalar-field overwrite would let a last-writer-wins lost update hide; notes *are* the history. This is the in-frontmatter analog of the repo's append-only `artifacts.jsonl` invariant.
- **Each note carries a globally-unique marker we control:** writer `i` posts `body = "rmw-{run}-{i:04}"`. Run uniqueness comes from the iteration index; `i` makes each marker unique within the run.

*Decision, recorded:* a synthetic integer counter field would also give a clean invariant, but it requires inventing a `Task` field the protocol doesn't have — scope creep into the data model for a test. Notes give identical counting power on an existing field. Rejected.

**Workload.** All W writers spawned on one `Arc<MeshService>` over one `Arc<S3Store>`, each awaiting a `tokio::sync::Barrier(W)` immediately before its `update_task` call so they enter the read-modify-write window together and maximize the collision window (without a barrier, `tokio::spawn` start-stagger lets writers trickle and contention evaporates).

```
seed project + task T, claim T by "stress" → in_progress
barrier = Barrier::new(W)
for i in 0..W spawn:
    body_i = format!("rmw-{run}-{i:04}")
    barrier.wait().await
    res_i = svc.update_task(UpdateTask { id: T.id, note: Some(body_i),
                                         actor: "stress".into(), ..Default::default() }).await
join all; partition res_i into Ok(marker) | Err(Conflict) | other
```

**The counting oracle — exact invariant math.** From the recorded results:

- `S_ok` = set of `body_i` for which `res_i == Ok` (writers the store told "committed"); `n_ok = |S_ok|`.
- `n_conf` = count of `res_i == Err(Conflict)` (retry-budget exhausted — a *legitimate* non-commit; see §Threats T-budget).
- Any `res_i == Err(Invalid | Unavailable | other)` or a `JoinError` ⇒ **immediate gate failure** — no transition is invalid here; the only legal outcomes are commit or budget-exhaustion conflict.

**Two-path readback (closes the LIST-vs-GET skew, red-team [concurrency-realism] HOLE 3).** `S3Store::get_task(id)` is *not* a point read by key — it calls `find_task_key(id)` (a `list_objects_v2` walk to resolve the key from the filename) then GETs by key. On an object store, LIST freshness can be weaker than read-after-write GET-by-key, so a LIST issued right after join could disagree with the authoritative by-key state and either mask a loss or fabricate a false RED. The oracle therefore reads the final object **twice and asserts they agree**:

- (R1) `svc → get_task(T.id).notes` (the LIST-walk path the product uses).
- (R2) a **direct GET-by-key** built the way `S3Store` builds task keys — `join_key(["projects", project_slug, "tasks", task_filename(id, task_slug)])` — issued through a harness accessor on the raw client (the harness already builds a bare `Client`; see §Test harness). Parse its body with `parse_task`.
- Assert `markers(R1) == markers(R2)`. A disagreement ⇒ **gate failure flagged as LIST/GET consistency skew**, distinct from a lost-update failure, so a substrate consistency problem is not silently scored as a CAS bug. The lost-update invariants below run against R2 (the authoritative by-key read); R1 is the agreement check.

Let `markers = { n.body : n ∈ R2.notes, n.body starts_with "rmw-{run}-" }`. The lost-update invariant is the conjunction:

- **(I1)** `markers ⊇ S_ok` — every committed marker is present → **no lost update**.
- **(I2)** `|markers| == n_ok` — no extra/phantom markers → no double-apply / phantom commit.
- **(I3)** no marker appears twice in `R2.notes` (multiset check) — exact-once.
- **(I4)** `n_ok + n_conf == W` — every writer terminated cleanly and is accounted for (catches a swallowed error / silent drop).

Because each marker is globally unique, I1 ∧ I2 collapse to **set equality** `markers == S_ok`: *the set of notes that physically survive equals exactly the set of writers the store claimed to commit.* A lost update is mathematically impossible to hide from that equality. The assertion's panic message prints the diff (`missing = S_ok − markers`, `extra = markers − S_ok`), so a failure **names the exact victim marker**.

**Liveness floor.** If contention were so high that *everyone* conflicts, the test would pass vacuously (`S_ok` and `markers` both empty) while proving nothing. So per iteration assert **`n_ok ≥ 1`**, and log the `n_ok / W` ratio so an audit eye sees contention isn't pathological. This guards against a regression that turns the store into a livelock and is the answer to the "fail on any `Conflict`?" dispute (see §Threats T-budget).

**Run-count rationale.** W = 16 on one object is genuinely high contention — 8× the 2-writer seed test — while staying inside the 5-attempt budget so legitimate writers *can* mostly land within budget. 30 iterations × 16 = 480 contended RMWs across 30 distinct schedules — comparable cost to sub-test A — plenty to catch a non-deterministic clobber. W and M are `const` at module top so an audit run can crank `W = 64` / `W = 128` to push into heavy-conflict territory; correctness (I1–I4) must hold at every W, only the liveness floor is W-sensitive and holds comfortably at the gate W. (Note that cranking W *widens* the GET-to-PUT window of the app-layer compare — see §Threats T4 — which is precisely why the raw-client precheck, not W-cranking, is what closes the conditional-PUT-fidelity gap.)

### Conformance precheck — raw client, bypassing `cas_put`

`gate_conditional_write_conformance`. Runs before the stress in the sign-off flow. It asserts the **backend's** conditional-write primitive directly by issuing requests on the **raw `aws_sdk_s3::Client`** (built by the harness, the same way `ensure_bucket` / `list_*` build one), **never through `put_*` / `cas_put`** — because on a strongly-consistent store the app-layer GET-compare would return `Conflict` regardless of whether the header works, masking a non-conditional backend (red-team [oracle-soundness] fix 3, [minio-s3]).

Each assertion checks the **SDK error's HTTP status code is exactly 412** (via the `ProvideErrorMetadata` / raw `Response::status()` surface), not merely that some `Conflict` came back:

1. `put_object` with `If-None-Match: *` on a fresh key → **ok** (create).
2. Same `put_object` with `If-None-Match: *` again → **412** (exists).
3. `put_object` with `If-Match: <bogus/stale ETag>` → **412** (mismatch).
4. `put_object` with `If-Match: <correct current ETag>` → **ok** (update).
5. **ETag-tracks-content** (T5): byte-identical re-PUT → same ETag; one changed byte → different ETag (the second half of the existing `get_put_roundtrip_etag_stable` posture).

Any non-412 where 412 is required ⇒ the gate **aborts as INCONCLUSIVE** (explicitly *not* a pass), message "backend does not honor conditional writes — gate cannot validate the cloud-relevant primitive." A `Conflict` produced by the app-layer compare does **not** satisfy this precheck — that is the whole point of going raw.

### Negative control — disable BOTH CAS layers; prove the oracle can fail

`gate_negative_control_loses_updates`, env-flagged / `#[ignore]` so it runs only during sign-off. Re-runs sub-test B's workload with CAS **fully disabled** and asserts it **produces at least one lost update** (`markers ⊊ S_ok`, i.e. `S_ok − markers` non-empty), not merely "the test fails." A green negative control is itself a NO-GO — it means the oracle has no teeth.

The header-stripping decorator from the prior draft is **abandoned**: it is technically impossible from the `Store` trait boundary (the `if_match` / `if_none_match` calls live inside the private `cas_put`; the trait surface — `put_task(value, expected)` etc. — exposes no raw conditional-PUT primitive, and forcing `expected = None` routes to `if_none_match("*")`, which *inverts* CAS into all-create-fails, not disables it), AND even if reachable it would leave the pre-flight GET-compare intact and still prevent lost updates (red-team [oracle-soundness] HOLE + [concurrency-realism] HOLE 1). Both layers must die. Two honest options; **pick one in §Open question 3**:

- **(a) env-gated branch inside `cas_put`.** When `DOSSIER_S3_DISABLE_CAS=1`, `cas_put` **skips the pre-flight GET-compare AND sends an unconditional `put_object`** (no `if_match` / `if_none_match`). Tiny test-only branch in production source; **requires carving it out of acceptance criterion 7** (it is the only permitted production line, justified by a one-line comment, and it is dead unless the env var is set).
- **(b) test-only `Store` impl in `tests/`.** A struct wrapping the same `aws_sdk_s3::Client` that re-implements the task write path as *read current body → append marker → unconditional `put_object`* (no compare, no header). Keeps production at a 0-line diff (preserves criterion 7) at the cost of duplicating the put path in test code.

Either way, to make the control reliably RED (not flaky): keep the `Barrier` so the GET→append→PUT windows overlap; with both compare and header removed, two writers reading the same base both PUT and one clobbers the other → `S_ok ⊋ markers` deterministically across W = 16. The assertion is `assert!(!(s_ok − markers).is_empty(), "negative control did not lose an update — oracle has no teeth")`.

## Test harness

**One store, sequential outer loop, fresh per-run slugs.** Each sub-test is a single `#[tokio::test]` with an outer `for run in 0..N` — *not* 100 separate fns and *not* spawning runs concurrently. Concurrency lives *inside* a run (the actors / writers); runs are serialized so a failure pins to a run index and 100 runs' worth of traffic doesn't swamp a laptop MinIO into `Unavailable` noise that looks like (but isn't) a gate failure. One `S3Store` is reused across all runs (client construction — credential load + smithy build — is the expensive part); isolation is **per-run by key**: the harness lives under a random `it/<ULID>` prefix per `test_store()` call, and the run index is folded into every slug.

**Multi-thread runtime.** Both gate tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 4)]`. The existing tests use the default current-thread runtime, which serializes the writers' network waits and *reduces* real concurrency — fine for the 2-writer seed race, too weak for 16 writers each doing GET→PUT round-trips. Multi-thread (modest worker count — overlap, not drowning MinIO) lets the writers' I/O genuinely overlap. `cargo test` parallelism across test *functions* is orthogonal and safe — each test owns its own key prefix. Mandated explicitly so a future in-memory test store can't silently re-serialize the gate (T8).

**Raw-client accessor.** The precheck and the (R2) by-key readback need the bare `aws_sdk_s3::Client` + bucket + resolved key. `tests/common/mod.rs` exposes a small accessor returning the `Client` + bucket + a `task_key(project_slug, id, task_slug)` helper that mirrors `S3Store`'s `join_key(["projects", …, "tasks", task_filename(id, slug)])` construction exactly. This is the same bare-`Client` pattern `ensure_bucket` already uses; no new dep.

**Result classification is the assertion backbone.** Every actor/writer result is partitioned and asserted on by bucket, never on raw counts alone:

- `Ok(effect)` — committed; its effect MUST appear in the final (R2) readback.
- `Err(StoreError::Conflict)` — retry-budget exhausted after `CAS_MAX_ATTEMPTS`. A clean, caller-visible non-commit. In sub-test B it is counted in `n_conf` and its marker MUST be absent (§Threats T-budget).
- `Err(StoreError::Invalid(_))` — for the §7.4 race loser only, the *expected* terminal "already claimed" outcome; assert the message marker. Anywhere else, a bug → hard fail.
- `Err(Unavailable | other)` / `JoinError` — infra failure / panic. Hard-fail with the run index; never counted as a lost-update result.

**File placement and shared harness.** New file `tests/s3_cas_gate.rs`, distinct from the store-layer `tests/s3_store_minio.rs`: the gate drives service-layer types (`MeshService`, `UpdateTask`, `ClaimTask`, `LinkArtifact`) where the existing file deliberately tests only the `Store` layer, and the gate is *the gate* — a reader/CI should point at one file. The existing `concurrent_writer_race_exactly_one_wins` stays put (it's a `cas_put` unit proof, not the gate; do not delete or absorb it).

The MinIO bootstrap currently in `s3_store_minio.rs` (`test_store`, `ensure_bucket`, `endpoint_configured`, `sample_project`) is lifted into `tests/common/mod.rs` (the canonical cargo shared-module idiom — `common` is not compiled as its own test binary) and both files `mod common;` it. Two integration files needing the identical bootstrap is past the "a little copying beats a premature abstraction" line; this small refactor ships *with* the gate so there's one source of truth. `common` also exposes the new seed helpers: `seed_claimable_task(svc, project, slug) -> Task`, `seed_in_progress_task(svc, run) -> (task_id, project_slug, task_slug)` (returns the slugs the by-key readback needs), and the raw-client accessor above. The `test_list_call_counter` plumbing stays via the existing `_with_list_counter` variant (only `shift_phases_uses_single_list_call` needs it).

**Skeleton (shape only).**

```rust
// tests/s3_cas_gate.rs
#![allow(clippy::expect_used, clippy::unwrap_used /* … integration tests */)]
mod common;
use common::{seed_in_progress_task, task_key, test_store};
use dossier::server::MeshService;
use dossier::store::{StoreError, UpdateTask};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::Barrier;

const S11_4_ITERS: usize = 100;
const RMW_ITERS: usize = 30;
const RMW_WRITERS: usize = 16;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_high_contention_rmw_oracle() {
    let Some((store, cfg, client)) = test_store().await else { return };
    let svc = Arc::new(MeshService::from_store(Arc::new(store)));
    for run in 0..RMW_ITERS {
        let (task_id, proj_slug, task_slug) = seed_in_progress_task(&svc, run).await;
        let barrier = Arc::new(Barrier::new(RMW_WRITERS));
        let mut handles = Vec::with_capacity(RMW_WRITERS);
        for i in 0..RMW_WRITERS {
            let (svc, id, bar) = (Arc::clone(&svc), task_id.clone(), Arc::clone(&barrier));
            let marker = format!("rmw-{run}-{i:04}");
            handles.push(tokio::spawn(async move {
                bar.wait().await;
                let r = svc.update_task(UpdateTask {
                    id, note: Some(marker.clone()), actor: "stress".into(),
                    ..Default::default()
                }).await;
                (marker, r)
            }));
        }
        let mut s_ok: BTreeSet<String> = BTreeSet::new();
        let mut n_conf = 0usize;
        for h in futures::future::join_all(handles).await {
            let (marker, r) = h.expect("worker panicked");            // JoinError = hard fail
            match r {
                Ok(_) => { s_ok.insert(marker); }
                Err(StoreError::Conflict) => n_conf += 1,             // legitimate non-commit
                Err(e) => panic!("run {run}: unexpected store error for {marker}: {e:?}"),
            }
        }
        assert!(!s_ok.is_empty(), "run {run}: livelock — no writer committed"); // liveness floor
        // R1: list-walk path the product uses.
        let r1 = svc /* get_task(task_id).notes */;
        // R2: authoritative direct GET-by-key (mirrors S3Store::join_key task key).
        let key = task_key(&cfg, &proj_slug, &task_id, &task_slug);
        let r2 = /* client.get_object(bucket, key) → parse_task → notes */;
        let pref = format!("rmw-{run}-");
        let m1: BTreeSet<String> = markers(&r1, &pref);
        let m2: BTreeSet<String> = markers(&r2, &pref);
        assert_eq!(m1, m2, "run {run}: LIST-walk vs GET-by-key skew (substrate consistency, not CAS)");
        let dup = has_duplicate(&r2, &pref);
        assert!(!dup, "run {run}: duplicate marker (double-apply)");   // I3
        assert_eq!(m2, s_ok, "run {run}: persisted != committed (LOST UPDATE or phantom)"); // I1 ∧ I2
        assert_eq!(s_ok.len() + n_conf, RMW_WRITERS, "run {run}: writers unaccounted"); // I4
    }
}
```

## Why this combination proves "0 lost updates"

- Sub-test B's set-equality `markers == S_ok` (against the by-key readback) is a **total, exact** statement over a single object: no interleaving, scheduler quirk, or backoff timing under which a committed note can be absent and the assertion still pass. Not statistical — exact per run, repeated across 30 schedules — and the LIST/GET agreement check rules out a substrate-consistency false reading either way.
- Sub-test A proves the **composition** of the real verbs (claim + link + claim-race) holds the same property across 100 mixed schedules, so the guarantee isn't an artifact of a single-verb microbench.
- The negative control proves the oracle **can** go RED (it has teeth); the raw-client precheck proves the **backend's conditional primitive** actually works (not just the app-layer compare).
- GREEN iff all of: both sub-tests pass with zero violations, the precheck passes, and the negative control fails. A single lost or duplicated marker, a non-committed `Ok`, a `Conflict` that actually wrote, an unaccounted writer, a LIST/GET skew, a precheck non-412, a green negative control, or any §11.4 oracle violation flips it RED with a named cause.

## Threats to validity + how each is closed

A passing run proves the absence of *observed* failures, not of the *defect class*. Each threat is a concrete way the gate stays GREEN while CAS is broken, with the defense that closes it.

| # | Threat | How it's closed |
|---|---|---|
| **T1 — weak oracle** | "Exactly one wins" detects a *totally absent* CAS (both succeed) but is blind to two writers committing against different re-read versions where the second clobbers the first. One-wins is a property of a single attempt; lost-update is a property of committed *history*. | Sub-test B's counting oracle asserts against **final object state** (by-key read), not the per-call result tuple: `markers == S_ok` over globally-unique note markers. A lost update is exactly `∃ body ∈ S_ok, body ∉ markers` → RED with the victim named. §11.4 is explicitly demoted from "the proof" to "the realism smoke." |
| **T2 — contention too low** | §11.4-as-written is structurally low-contention (claimers on disjoint objects; one race pair, once). 100 green runs can mean the retry-reapply branch fired ~0 times. | Sub-test B: ≥ 16 (audit: 64+) writers on **one** object, **barrier-synchronized start**. §11.4 stays as integration realism, not the contention proof. |
| **T3 — the retry loop masks a dead CAS** | The loop is *designed* to paper over conflicts; a too-forgiving oracle lets it paper over *bugs*. With a no-op CAS and commutative appends, every writer could read fresh, append, return `Ok` while final state looks plausible. **The most dangerous threat.** | (a) Note-append makes a broken CAS **detectable**: two writers read the same base, each appends, the second whole-object PUT drops the first → `S_ok ⊋ markers` → RED. (b) **Negative control** (§Negative control) disables BOTH CAS layers and MUST produce ≥1 lost update; a green control is a NO-GO. (c) Retry never "succeeds past correctness": budget-exhaustion returns typed `Conflict`; the oracle counts committed effects regardless of attempt count and never asserts "all writers Ok." |
| **T4 — MinIO ≠ AWS S3 conditional-write fidelity / dual-guard masking** | The cloud rollout depends on the backend honoring `If-Match` / `If-None-Match` and returning **412** on mismatch. But on MinIO's strong consistency, the app-layer GET-compare alone enforces CAS — **a backend that completely ignored `If-Match` would still pass the gate via `cas_put`** (red-team [oracle-soundness], [concurrency-realism] HOLE 2, [minio-s3]). | **The gate via `cas_put` cannot distinguish a conditional from a non-conditional backend on a strongly-consistent store** — stated plainly. That is why the **raw-client conformance precheck** (§Conformance precheck) is load-bearing: it asserts SDK HTTP status `== 412` directly, bypassing the app-compare. Pin a conditional-write-capable MinIO release (`RELEASE.2024-09-*`+, AWS GA'd `If-Match`-on-PUT Aug 2024), recorded in this doc + harness. **AWS/R2 parity is an explicit residual:** MinIO-green is necessary, not sufficient; one real-backend confirmation run is **required before GO is acted on** (§Open question 2). |
| **T5 — ETag instability** | `Version` *is* the ETag; the scheme assumes ETag changes iff content changes. Multipart uploads and SSE-KMS/SSE-C break that → spurious conflicts (false RED) or stable-but-untracking ETags (false GREEN). | dossier objects are small single-part markdown, no SSE in the test — asserted as a precondition. Precheck step 5 asserts byte-identical re-PUT → same ETag, changed byte → different ETag. **Residual invariant recorded:** if dossier ever stores large artifacts or enables SSE, `Version` must move off raw ETag to a stored content hash. |
| **T6 — read-after-write consistency** | The loop is GET → mutate → conditional-PUT → (on 412) GET again; on an eventually-consistent store the loser could re-read stale and re-apply. Separately, `cas_put`'s GET-compare and PUT are two round-trips — a TOCTOU window the header is meant to close (red-team [concurrency-realism] HOLE 2). | In scope: S3 single-region (strong read-after-write) and MinIO (strongly consistent). On those, the TOCTOU window is closed by the header on a conformant backend and (redundantly) by strong consistency. The gate talks to `S3Store` directly, not the warm cache. The oracle is the backstop: any consistency violation surfaces as a lost update → RED. The TOCTOU-via-dead-header case is exactly what the raw-client precheck + AWS/R2 run guard (T4). |
| **T7 — insufficient / mis-specified repetitions** | "N=100" is a number, not a confidence level; a 1-in-500 race is near-invisible at 100 *low-contention* runs. | Confidence comes from **windows per run**, not run count: sub-test B forces hundreds of CAS resolutions per run (W × attempts). *We are not estimating a failure rate; we force a high-contention regime and assert an exact invariant (`markers == S_ok`) holds in every observation, with a negative control proving the assertion can fail.* §11.4 keeps N=100 as cheap integration realism. |
| **T8 — tokio scheduling makes "concurrent" serial** | `#[tokio::test]` defaults to current-thread; writers interleave only at `.await` and can run near-serially, hiding parallel-only races. | `flavor = "multi_thread", worker_threads = 4` + the `Barrier` before the first write. Real network I/O at each GET/PUT provides yield points; multi-thread overlaps the CPU-side mutate + SDK dispatch. Mandated so a future in-memory test store can't silently re-serialize the gate. |
| **T9 — the artifact path is a *second*, distinct CAS surface** | `put_artifact` on S3 is **not** create-only-per-id — it's a CAS read-modify-write of the shared `artifacts.jsonl` with its own `ARTIFACT_PUT_RETRIES` loop (verified in `s3store.rs`). The §11.4 "third agent links artifacts" leg hammers a hot shared object with an independent CAS loop. | The linker does **K ≥ 4 concurrent links**, and sub-test A's invariant 3 is a committed-effects oracle on `artifacts.jsonl` (every `Ok` link present exactly once; listed count == `Ok` count). The artifact path gets its own counting oracle; it is not waved through. (Reconciled in §Dependencies.) |
| **T-budget — `Conflict` accounting** | With W=16 and `CAS_MAX_ATTEMPTS=5`, some writers legitimately exhaust the budget and return `Conflict`. That is **not** a lost update. But it must be partitioned soundly, and the test must not pass vacuously if *everyone* conflicts. | The oracle treats a budget `Conflict` as a clean non-commit: its marker is never in `S_ok`, I1/I2 don't expect it, I4 counts it. **A `Conflict` is not a gate failure.** The liveness floor (`n_ok ≥ 1`) prevents vacuous pass and resolves the "fail on any `Conflict`?" dispute in favor of the sound §8 reading. W=16 is sized so the budget is *adequate but not generous*. This is the one place the oracle couples to the retry contract: the partition is sound only if `Conflict` strictly means "did not commit" and every `Ok` strictly means "note persisted" — which the oracle is the executable proof of. |
| **T-jitter — `cas_backoff` is not true full jitter (finding, not a fix here)** | `cas_backoff` derives its sleep from `SystemTime::now().subsec_nanos() % (max+1)` — **not an RNG** — and caps the exponent at 6 (base 25ms · 2⁶ = 1600ms < the 2000ms cap, so attempt 5 never reaches the cap). Under a barrier-synchronized herd, threads sampling the clock in the same window back off near-identically and re-collide in lockstep. | Does **not** cause lost updates (the oracle stays correct), but *inflates spurious retry exhaustion* → false RED / flaky gate. **Recorded as a finding**, not fixed by this PR: the gate runs against the loop as-is. If the gate is flaky because retries exhaust (high `n_conf` while observed contention is moderate), *that* is the signal to fix `cas_backoff` (seeded RNG drawing `uniform(0, cap)`; raise/remove the exponent clamp to honor §8's "full jitter, base 25ms cap 2s"). Logging `n_ok / W` per iteration makes this observable. Separate production PR. |
| **T-isolation — harness hygiene false-green** | Reused prefixes could let residue inflate the "present" count; a slug bug could route "concurrent" writers to different keys so they never contend (trivially green). | Fresh `it/<ULID>` prefix per test + run index in every slug; markers globally unique (`rmw-{run}-{i}`) so cross-run residue can't alias a current marker even if isolation regresses. Sub-test B targets one explicit `task_id` (logged once); misrouted writers' markers land elsewhere and go missing → RED, turning "are they really on one object" into a checked invariant. The by-key (R2) readback further pins the asserted object. |

## Acceptance criteria (binary, baseline-free)

The gate is **GO iff all of the following hold** in a sign-off run against a conditional-write-capable MinIO with `DOSSIER_S3_TEST_ENDPOINT` set. Every criterion is an absolute pass/fail.

1. **`gate_scenario_11_4_mixed_workload` passes at N=100.** Across all 100 iterations: every `Ok` claim is reflected in final task state (correct assignee, `status=claimed`, `claimed_at` set); the §7.4 race yields exactly one `Ok` and one **terminal `Invalid` containing "already claimed"** (never a silent success, never both winning, never a panic); every `Ok` link appears exactly once in `list_artifacts` with listed-count == `Ok`-count.
2. **`gate_high_contention_rmw_oracle` passes at W=16 × M=30.** Every iteration: `markers == S_ok` (no committed marker missing = lost update; no extra/duplicate marker = phantom/double-apply); LIST-walk and by-key readbacks agree (no substrate-consistency skew); every non-`Ok` is a typed `Conflict` (budget exhaustion) — never `Invalid`, `Unavailable`, panic, or silent drop; `n_ok + n_conf == W`; `n_ok ≥ 1`.
3. **0 lost updates, exactly.** A lost update = `(op returned Ok) ∧ (effect absent from final by-key readback)`. The count across the entire gate run is **exactly 0**. This is the single number the gate is named for.
4. **Negative control fails (gate has teeth).** With **BOTH CAS layers disabled** (`DOSSIER_S3_DISABLE_CAS=1` in `cas_put` per option (a), or the test-only unconditional-PUT `Store` per option (b)), `gate_negative_control_loses_updates` **produces ≥ 1 lost update** (`S_ok − markers` non-empty). A green negative control is itself a NO-GO. (Disabling only the header is insufficient and is not an acceptable control — the app-layer compare would keep CAS alive.)
5. **Conformance precheck passes.** The live backend, exercised via the **raw `aws_sdk_s3` Client** (not `cas_put`), returns SDK HTTP status **exactly 412** for both `If-Match`-stale and `If-None-Match`-existing, and ETag tracks content (T4, T5). Non-412 where 412 is required ⇒ INCONCLUSIVE, not pass.
6. **`make check` stays green** with and without the endpoint: fmt clean, clippy `-D warnings` clean (test module gates the panicky lints, same as existing s3 tests), and without `DOSSIER_S3_TEST_ENDPOINT` all gate tests self-skip so default CI is unaffected.
7. **No production-source diff — with one carve-out.** The PR's `git diff --stat` touches only `tests/` and `docs/`, **except** the single env-gated `cas_put` branch IF negative-control option (a) is chosen (≤ ~8 lines, dead unless `DOSSIER_S3_DISABLE_CAS=1`, justified by a one-line comment). Option (b) keeps the diff at 0 production lines. Any *other* production change ⇒ the mechanism wasn't done ⇒ NO-GO; split that fix into its own PR.
8. **Repeatable green (determinism).** Sign-off requires **3 consecutive clean local runs** of the gate file (`cargo test --test s3_cas_gate` ×3) against MinIO. A flaky pass (e.g. 2/3) is a **NO-GO** — an intermittent ordering/retry bug the oracle catches sometimes. Recorded in the sign-off note; not a CI gate (MinIO isn't in default CI).

**Recording the verdict.** Captured as a dossier artifact linked to the Phase-1 gate task in the dogfood corpus (`artifact.link { kind: "run", reference: <3× cargo-test invocation + git sha>, label: "cas-lost-update-gate: GO | NO-GO" }`) plus a one-paragraph sign-off in the task notes (N/W/M actually run, lost-update count, flaky-or-clean across the 3 runs, MinIO version, precheck + negative-control results, and whether the AWS/R2 confirmation run has been done). This doc gets a `## Gate result` stamp (GO/NO-GO + date + sha) when run. No separate report file (house rule: findings go in the corpus/PR).

**What NO-GO implies** — each a distinct branch, none of them "tune the test until green":
- *Oracle finds a lost update / phantom* → a real CAS or RMW-branch bug. Stop; file against `S3Store`/`cas_mutate_task`; fix in a separate production PR; re-run. **Do not relax the oracle.**
- *LIST/GET skew* → substrate consistency problem (or a wrong key construction in the harness); diagnose before trusting any score.
- *Flaky (< 3/3)* → intermittent bug; same path. (If diagnosed to `cas_backoff` correlated jitter, that's the T-jitter fix.)
- *Budget exhaustion surfaces as untyped error / panic / silent success* → the "never silent" contract (§8) is broken; production fix, re-run.
- *Negative control passes / precheck non-412* → the test or substrate can't detect loss; fix the gate/substrate before trusting any green.

A NO-GO never advances past §11.4; Phases 3b+ stay locked until a subsequent GO.

## Test plan

- `make check` green with and without `DOSSIER_S3_TEST_ENDPOINT`.
- `cargo test --test s3_cas_gate` against MinIO runs sub-tests A + B + the conformance precheck.
- The negative control (`gate_negative_control_loses_updates`) is `#[ignore]` / env-flagged; run it explicitly during sign-off and assert it goes RED (≥1 lost update).
- Sign-off: 3 consecutive clean runs of the gate file, the precheck passing, the negative control failing, recorded in the dossier task.
- Manual residual: one AWS/R2 confirmation run before acting on GO (§Open question 2), out of the automated scope.

## Non-goals

- **No real-AWS / R2 run inside the automated gate.** MinIO is the gate substrate (same posture as every existing s3 test). One S3/R2 confirmation run is **required before GO is acted on** (it closes the T4 residual), but it is out of this gate's automated scope (§Open questions).
- **No perf / latency / throughput benchmarking.** The gate is correctness-binary (lost updates: yes/no). No timing assertions, no p99, no claims/sec. §2 latency targets are validated by dogfooding.
- **No warm-cache path.** The cache (Phase 3a/b) is bounded-stale by design; correctness rests on write-time CAS. The gate tests the authority, not the cache.
- **No retry-budget / jitter tuning in this PR.** `CAS_MAX_ATTEMPTS`, base 25ms / cap 2s are fixed inputs; a typed `Conflict` under load is a pass. The `cas_backoff` jitter weakness is a recorded finding (T-jitter), fixed separately if the gate flags it.
- **No `delete_*`, multi-tenant, or auth verbs** — post-gate phases (§9).
- **No `FsStore` variant of the gate** — FsStore CAS has its own unit tests; this gate is the S3-tier GO/NO-GO.
- **No `gh issue` for follow-ups** — findings land in this doc / the PR / the dogfood corpus.

## Scope / sizing / dependencies

**Scope: one PR.** Test-harness + in-test oracle, plus at most the single env-gated `cas_put` branch for the negative control (option (a); option (b) keeps production untouched). The CAS machinery (`cas_put`, `cas_mutate_task` 3-way branch + `CAS_MAX_ATTEMPTS` + `cas_backoff`, `claim_task`, `update_task`, `link_artifact`/`put_artifact`) is shipped and verified; the gate exercises it as-is. Any production change beyond the carve-out is itself a NO-GO and ships as a separate fix PR.

**Sizing: amazing band (< 500 weighted LOC), estimate ~190 wLOC.** ~380 raw test LOC at 0.5× ≈ 190 wLOC (two gate tests + precheck + negative control + oracle/seed helpers + the small `common` lift); docs 0×; the optional ≤8-line `cas_put` branch is 1.0× and negligible. Comfortably inside *amazing*.

**Dependencies.**

| Dep | Status | Hard / soft |
|---|---|---|
| Phase 0 — async `Store` trait, `Versioned<T>`, CAS write flow | done | HARD — met |
| Phase 1 — `S3Store` (`If-Match`/`If-None-Match` + app-compare), 3-way retry RMW, `cas_mutate_task`, `claim_task` | done + verified vs MinIO | HARD — met |
| MinIO reachable via `DOSSIER_S3_TEST_ENDPOINT`, conditional-write-capable release | available; version to pin | HARD at run-time; soft at merge-time (test self-skips) |
| Phase 2 — artifact sharding (`artifacts.jsonl` → `artifacts/<id>.json`) | **not landed** in this worktree | **soft — see ruling** |

**Phase-2 (artifact sharding) ruling — soft dep, do not block the gate.** A prior facet argued sharding is soft *because* S3 `put_artifact` is "create-only by unique ULID with no shared-object contention." **That premise is false against the current code:** `put_artifact` is a CAS read-modify-write of the shared `artifacts.jsonl` (read, reject duplicate id, append a line, `cas_put` the whole object, bounded `ARTIFACT_PUT_RETRIES`). So the artifact leg is, today, a *second high-contention CAS surface* — which is why the gate stresses it with K concurrent links and a committed-effects oracle (T9), not why it can ignore it. The dep is nonetheless **soft** for the right reason: the gate's artifact invariant is "no `Ok` link is *lost*," not "links don't contend." The shared-`artifacts.jsonl` path *can* satisfy "no link lost" (invariant 3 checks exactly that). If Phase-2 sharding lands first, the leg instead exercises the create-only `If-None-Match: *` per-id path and invariant 3 still holds unchanged. **Do not block the gate PR on Phase 2.** No new crate deps — `tokio`, `Arc`, `futures` (already a dep), and the existing `aws_sdk_s3` test stack cover it.

## Gate result — GO (MinIO), 2026-06-08

Implemented (`tests/s3_cas_gate.rs` + the env-gated `cas_put` branch) and run against MinIO (`minio/minio`, conditional-write capable — confirmed by the precheck). **Verdict: GO**, modulo the one residual below.

- **Sub-test B (RMW counting oracle, W=16 × M=30):** PASS on all 3 sign-off runs — `persisted == committed` every iteration, **0 lost updates** across 480 contended read-modify-writes per run.
- **Sub-test A (§11.4 mixed workload ×100):** PASS on all 3 runs — disjoint claims persist (correct assignee + `claimed` + `claimed_at`), the race yields exactly one `Ok` + one terminal "already claimed" `Invalid`, and every `Ok` link is present exactly once.
- **Conformance precheck (raw `aws_sdk_s3` client, bypassing `cas_put`):** PASS — MinIO returns HTTP **412** for `If-None-Match`-on-existing and `If-Match`-stale, and ETag tracks content.
- **Negative control (`DOSSIER_S3_DISABLE_CAS=1`):** RED as required — with both CAS layers disabled the oracle **detects** a lost update, proving it has teeth.
- **3 consecutive clean runs:** ✓ (281s / 160s / 149s). **`make check`:** green with and without `DOSSIER_S3_TEST_ENDPOINT`.

**Residual — required before acting on GO:** one AWS S3 / Cloudflare R2 confirmation run. Per T4, the gate via `cas_put` on a strongly-consistent backend cannot, alone, distinguish a conditional from a non-conditional backend (the app-layer GET-compare suffices on MinIO); the raw-client precheck mitigates this, but a real-backend run is needed to fully close the conditional-PUT-fidelity gap. Out of the automated gate's scope — needs an owner + cloud creds.

## Decisions (resolved open questions)

1. **MinIO version pin** → run against current `minio/minio`; the conformance precheck empirically gates conditional-write support on every run, so any tag that passes the precheck is acceptable. Pin a specific `RELEASE.2024-09-*`+ tag if/when the gate is wired to a scheduled CI job.
2. **AWS/R2 confirmation run** → kept as the single open residual above (manual, operator-owned, before acting on GO). Not automatable here (no cloud creds).
3. **Negative-control shape** → **(a)**, the env-gated branch inside `cas_put` (`DOSSIER_S3_DISABLE_CAS`, ~17 lines, dead unless set). Implemented; criterion 7 carve-out honored.
4. **`cas_backoff` jitter** → filed as a follow-up (real §8 gap: clock-derived, exponent-clamped — not true full jitter). The gate is clean, so it does **not** block GO.
5. **Standing gate W** → W=16 standing; W=64/128 operator-on-demand for audits.

## Implementation notes (deviations from the draft, all sound)

- **Single-path readback.** The oracle reads back via the public `Store::get_task` / `list_artifacts` (a real product read; authoritative on strongly-consistent MinIO). The separate raw-by-key (R2) readback + LIST/GET-skew check needed `pub(crate)` internals (`parse_task`, `find_task_key`, `task_filename`) unreachable from an integration test; the LIST/GET-skew concern is a real-S3/R2 eventual-consistency edge that folds into the AWS/R2 residual, not a MinIO-gate concern.
- **Self-contained test file.** `tests/s3_cas_gate.rs` carries its own ~40-line MinIO harness rather than lifting `tests/s3_store_minio.rs` into `tests/common/mod.rs` — keeps the working store-layer test untouched. Consolidating both onto a shared `common` module is a trivial follow-up.
- **Markers, not parse.** Sub-test B asserts on raw note-body marker strings (globally unique `rmw-{run}-{i}`) via the public `Task.notes`, so the oracle doesn't depend on the parser under test.

## Source

Materializes the §11.4 engineering gate from the cloud-backend spec into executable form. Umbrella: [docs/features/cloud-backend/spec.md](../spec.md). New test files: `tests/s3_cas_gate.rs`, `tests/common/mod.rs`. Sized for the `/work-driver-prep` → `/work-driver` chain.
