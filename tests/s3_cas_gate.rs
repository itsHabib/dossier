//! CAS lost-update validation gate (cloud-backend Phase 1 GO/NO-GO).
//!
//! Two sub-tests against a live MinIO, GREEN iff both pass:
//!   A — faithful TDD §11.4 scenario ×100 (claim + link + claim-race verbs).
//!   B — high-contention single-object read-modify-write counting oracle.
//! Plus a raw-client conditional-write conformance precheck, and a negative
//! control (ignored by default) that disables CAS and must lose an update.
//!
//! All gate tests self-skip when `DOSSIER_S3_TEST_ENDPOINT` is unset, so the
//! default `make check` (no MinIO) stays green. Design + acceptance:
//! docs/features/cloud-backend/phase-1/cas-lost-update-validation-gate.md.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::too_many_lines,
    reason = "integration tests"
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use tokio::sync::Barrier;
use ulid::Ulid;

use dossier::domain::{ClaimTask, LinkArtifact, NewProject, NewTask, Task, TaskStatus, UpdateTask};
use dossier::server::MeshService;
use dossier::store::{ArtifactListFilter, Store, StoreError};
use dossier::{S3Config, S3Store};

const S11_4_ITERS: usize = 100;
const RMW_ITERS: usize = 30;
const RMW_WRITERS: usize = 16;
const K_LINKS: usize = 4;

// --- MinIO harness (self-contained; mirrors tests/s3_store_minio.rs) ---

fn endpoint_configured() -> Option<String> {
    std::env::var("DOSSIER_S3_TEST_ENDPOINT")
        .ok()
        .filter(|s| !s.is_empty())
}

async fn build_client(cfg: &S3Config) -> Client {
    let creds = Credentials::new(
        cfg.access_key_id.clone(),
        cfg.secret_access_key.clone(),
        None,
        None,
        "dossier-cas-gate",
    );
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(cfg.region.clone()))
        .credentials_provider(creds)
        .load()
        .await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared);
    if let Some(url) = &cfg.endpoint_url {
        builder = builder.endpoint_url(url);
    }
    builder = builder.force_path_style(cfg.force_path_style);
    Client::from_conf(builder.build())
}

async fn ensure_bucket(client: &Client, bucket: &str) {
    if let Err(err) = client.create_bucket().bucket(bucket).send().await {
        let msg = format!("{err:?}");
        assert!(
            msg.contains("BucketAlreadyOwnedByYou") || msg.contains("BucketAlreadyExists"),
            "create bucket failed: {msg}"
        );
    }
}

/// Returns `(store, raw client, config)` or `None` when MinIO isn't configured.
async fn test_store() -> Option<(Arc<S3Store>, Client, S3Config)> {
    let endpoint = endpoint_configured()?;
    let access = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".to_owned());
    let secret = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_owned());
    let cfg = S3Config {
        bucket: "dossier-it".to_owned(),
        prefix: format!("cas-gate/{}", Ulid::new()),
        endpoint_url: Some(endpoint),
        region: "us-east-1".to_owned(),
        access_key_id: access,
        secret_access_key: secret,
        force_path_style: true,
        test_list_call_counter: None,
        test_get_call_counter: None,
    };
    let client = build_client(&cfg).await;
    ensure_bucket(&client, &cfg.bucket).await;
    let store = S3Store::new(cfg.clone()).await.ok()?;
    Some((Arc::new(store), client, cfg))
}

async fn seed_project(svc: &MeshService, slug: &str) {
    svc.create_project(NewProject {
        slug: slug.to_owned(),
        title: "gate".to_owned(),
        description: String::new(),
        actor: "stress".to_owned(),
    })
    .await
    .expect("seed project");
}

async fn seed_task(svc: &MeshService, project: &str, slug: &str) -> String {
    svc.create_task(NewTask {
        project: project.to_owned(),
        phase: None,
        slug: slug.to_owned(),
        title: "t".to_owned(),
        body: String::new(),
        actor: "stress".to_owned(),
        depends_on: Vec::new(),
    })
    .await
    .expect("seed task")
    .id
}

fn note(id: &str, marker: &str, actor: &str) -> UpdateTask {
    UpdateTask {
        id: id.to_owned(),
        note: Some(marker.to_owned()),
        actor: actor.to_owned(),
        ..Default::default()
    }
}

fn markers(task: &Task, prefix: &str) -> Vec<String> {
    task.notes
        .iter()
        .map(|n| n.body.clone())
        .filter(|b| b.starts_with(prefix))
        .collect()
}

// --- Sub-test B: high-contention single-object RMW counting oracle ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_high_contention_rmw_oracle() {
    let Some((store, _client, _cfg)) = test_store().await else {
        return;
    };
    let svc = Arc::new(MeshService::from_store(store.clone()));

    for run in 0..RMW_ITERS {
        let project = format!("rmwgate-{run}");
        seed_project(&svc, &project).await;
        let task_id = seed_task(&svc, &project, "t").await;
        svc.claim_task(&ClaimTask {
            id: task_id.clone(),
            actor: "stress".to_owned(),
        })
        .await
        .expect("claim seed task");
        svc.update_task(UpdateTask {
            id: task_id.clone(),
            status: Some(TaskStatus::InProgress),
            actor: "stress".to_owned(),
            ..Default::default()
        })
        .await
        .expect("move seed task to in_progress");

        let barrier = Arc::new(Barrier::new(RMW_WRITERS));
        let mut handles = Vec::with_capacity(RMW_WRITERS);
        for i in 0..RMW_WRITERS {
            let svc = Arc::clone(&svc);
            let id = task_id.clone();
            let bar = Arc::clone(&barrier);
            let marker = format!("rmw-{run}-{i:04}");
            handles.push(tokio::spawn(async move {
                bar.wait().await;
                let res = svc.update_task(note(&id, &marker, "stress")).await;
                (marker, res)
            }));
        }

        let mut committed: BTreeSet<String> = BTreeSet::new();
        let mut n_conflict = 0usize;
        for handle in handles {
            let (marker, res) = handle.await.expect("writer task panicked");
            match res {
                Ok(_) => {
                    committed.insert(marker);
                }
                Err(StoreError::Conflict) => n_conflict += 1,
                Err(other) => panic!("run {run}: unexpected store error for {marker}: {other:?}"),
            }
        }

        // Liveness floor: high contention must not livelock everyone out.
        assert!(
            !committed.is_empty(),
            "run {run}: no writer committed (livelock?)"
        );

        let task = store.get_task(&task_id).await.expect("readback").value;
        let prefix = format!("rmw-{run}-");
        let persisted_vec = markers(&task, &prefix);
        let persisted: BTreeSet<String> = persisted_vec.iter().cloned().collect();

        // I3: exactly-once (no double-apply).
        assert_eq!(
            persisted_vec.len(),
            persisted.len(),
            "run {run}: duplicate marker persisted (double-apply)"
        );
        // I1 ∧ I2: the set that survives == the set the store said it committed.
        // A lost update is exactly a committed marker missing here.
        assert_eq!(
            persisted,
            committed,
            "run {run}: persisted != committed — missing(lost)={:?} extra(phantom)={:?}",
            committed.difference(&persisted).collect::<Vec<_>>(),
            persisted.difference(&committed).collect::<Vec<_>>(),
        );
        // I4: every writer accounted for (no swallowed error / silent drop).
        assert_eq!(
            committed.len() + n_conflict,
            RMW_WRITERS,
            "run {run}: writers unaccounted (ok={} conflict={})",
            committed.len(),
            n_conflict
        );
    }
}

// --- Sub-test A: faithful §11.4 mixed-workload scenario ×100 ---

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gate_scenario_11_4_mixed_workload() {
    let Some((store, _client, _cfg)) = test_store().await else {
        return;
    };
    let svc = Arc::new(MeshService::from_store(store.clone()));

    for run in 0..S11_4_ITERS {
        let project = format!("s114-{run}");
        seed_project(&svc, &project).await;
        let tx = seed_task(&svc, &project, "tx").await;
        let ty = seed_task(&svc, &project, "ty").await;
        let trace = seed_task(&svc, &project, "trace").await;

        // 4 task-actors (a, b, racer-1, racer-2) + K concurrent link actors, all
        // released together so the K links contend on the shared artifacts.jsonl CAS.
        let barrier = Arc::new(Barrier::new(4 + K_LINKS));

        let h_a = {
            let (svc, id, bar) = (Arc::clone(&svc), tx.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                bar.wait().await;
                svc.claim_task(&ClaimTask {
                    id,
                    actor: "agent-a".to_owned(),
                })
                .await
            })
        };
        let h_b = {
            let (svc, id, bar) = (Arc::clone(&svc), ty.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                bar.wait().await;
                svc.claim_task(&ClaimTask {
                    id,
                    actor: "agent-b".to_owned(),
                })
                .await
            })
        };
        let mut h_links = Vec::with_capacity(K_LINKS);
        for k in 0..K_LINKS {
            let (svc, proj, bar) = (Arc::clone(&svc), project.clone(), Arc::clone(&barrier));
            h_links.push(tokio::spawn(async move {
                bar.wait().await;
                svc.link_artifact(LinkArtifact {
                    project: proj,
                    task: None,
                    kind: "commit".to_owned(),
                    reference: format!("artref-{run}-{k}"),
                    label: format!("artref-{run}-{k}"),
                    actor: "linker".to_owned(),
                })
                .await
                .is_ok()
            }));
        }
        let h_r1 = {
            let (svc, id, bar) = (Arc::clone(&svc), trace.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                bar.wait().await;
                svc.claim_task(&ClaimTask {
                    id,
                    actor: "racer-1".to_owned(),
                })
                .await
            })
        };
        let h_r2 = {
            let (svc, id, bar) = (Arc::clone(&svc), trace.clone(), Arc::clone(&barrier));
            tokio::spawn(async move {
                bar.wait().await;
                svc.claim_task(&ClaimTask {
                    id,
                    actor: "racer-2".to_owned(),
                })
                .await
            })
        };

        let r_a = h_a.await.expect("join a");
        let r_b = h_b.await.expect("join b");
        let mut link_oks = 0usize;
        for h in h_links {
            if h.await.expect("join link") {
                link_oks += 1;
            }
        }
        let r1 = h_r1.await.expect("join r1");
        let r2 = h_r2.await.expect("join r2");

        // Invariant 1: both disjoint-task claims landed and persisted.
        assert!(r_a.is_ok(), "run {run}: agent-a claim failed: {r_a:?}");
        assert!(r_b.is_ok(), "run {run}: agent-b claim failed: {r_b:?}");
        let task_x = store.get_task(&tx).await.expect("readback tx").value;
        let task_y = store.get_task(&ty).await.expect("readback ty").value;
        assert_eq!(task_x.assignee, "agent-a", "run {run}: tx assignee");
        assert_eq!(task_x.status, TaskStatus::Claimed, "run {run}: tx status");
        assert_eq!(task_y.assignee, "agent-b", "run {run}: ty assignee");
        assert_eq!(task_y.status, TaskStatus::Claimed, "run {run}: ty status");

        // Invariant 2: the race yields exactly one Ok winner; the loser is a
        // terminal "already claimed" Invalid (not a Conflict, not a panic).
        let race_oks = usize::from(r1.is_ok()) + usize::from(r2.is_ok());
        assert_eq!(
            race_oks, 1,
            "run {run}: race winners != 1 (r1={r1:?} r2={r2:?})"
        );
        let loser = if r1.is_ok() { &r2 } else { &r1 };
        match loser {
            Err(StoreError::Invalid(msg)) => assert!(
                msg.contains("already claimed"),
                "run {run}: race loser Invalid but not 'already claimed': {msg}"
            ),
            other => panic!("run {run}: race loser not terminal Invalid: {other:?}"),
        }
        let task_race = store.get_task(&trace).await.expect("readback trace").value;
        let winner = if r1.is_ok() { "racer-1" } else { "racer-2" };
        assert_eq!(
            task_race.assignee, winner,
            "run {run}: trace assignee != race winner"
        );

        // Invariant 3: no artifact lost — every Ok link present exactly once.
        assert_eq!(link_oks, K_LINKS, "run {run}: not all links returned Ok");
        let arts = store
            .list_artifacts(ArtifactListFilter {
                project: project.clone(),
            })
            .await
            .expect("list artifacts");
        let ref_prefix = format!("artref-{run}-");
        let refs_vec: Vec<String> = arts
            .iter()
            .map(|a| a.reference.clone())
            .filter(|r| r.starts_with(&ref_prefix))
            .collect();
        let refs: BTreeSet<String> = refs_vec.iter().cloned().collect();
        assert_eq!(
            refs_vec.len(),
            refs.len(),
            "run {run}: duplicate artifact reference"
        );
        assert_eq!(
            refs.len(),
            K_LINKS,
            "run {run}: artifact count != K (lost link?)"
        );
    }
}

// --- Conformance precheck: raw client, bypassing cas_put, asserts HTTP 412 ---

#[tokio::test]
async fn gate_conditional_write_conformance() {
    let Some((_store, client, _cfg)) = test_store().await else {
        return;
    };
    let bucket = "dossier-it";
    let key = format!("cas-gate-precheck/{}", Ulid::new());

    // 1. create-only on a fresh key → ok.
    let created = client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from(b"v1".to_vec()))
        .if_none_match("*")
        .send()
        .await
        .expect("create-only on fresh key should succeed");
    let etag = created.e_tag().expect("etag on create").to_owned();

    // 2. create-only again → 412 (exists).
    let err = client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from(b"v2".to_vec()))
        .if_none_match("*")
        .send()
        .await
        .expect_err("create-only on existing key must fail");
    assert_eq!(
        err.raw_response().map(|r| r.status().as_u16()),
        Some(412),
        "If-None-Match on existing key must be 412 (backend not conditional?)"
    );

    // 3. if-match with a stale/bogus etag → 412.
    let err = client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from(b"v3".to_vec()))
        .if_match("\"00000000000000000000000000000000\"")
        .send()
        .await
        .expect_err("If-Match with stale etag must fail");
    assert_eq!(
        err.raw_response().map(|r| r.status().as_u16()),
        Some(412),
        "If-Match stale must be 412 (backend not conditional?)"
    );

    // 4. if-match with the current etag → ok.
    let updated = client
        .put_object()
        .bucket(bucket)
        .key(&key)
        .body(ByteStream::from(b"v4".to_vec()))
        .if_match(&etag)
        .send()
        .await
        .expect("If-Match with current etag should succeed");
    let etag2 = updated.e_tag().expect("etag on update").to_owned();

    // 5. ETag tracks content: changed bytes → different etag.
    assert_ne!(etag, etag2, "etag must change when content changes");
}

// --- Negative control: disable CAS, prove the oracle detects the loss ---
//
// Ignored by default; run during sign-off with BOTH the endpoint and
// DOSSIER_S3_DISABLE_CAS set, in its own process:
//   DOSSIER_S3_TEST_ENDPOINT=... DOSSIER_S3_DISABLE_CAS=1 \
//     cargo test --test s3_cas_gate -- --ignored gate_negative_control
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "negative control: requires DOSSIER_S3_DISABLE_CAS=1; proves the oracle has teeth"]
async fn gate_negative_control_loses_updates() {
    assert!(
        std::env::var_os("DOSSIER_S3_DISABLE_CAS").is_some(),
        "negative control requires DOSSIER_S3_DISABLE_CAS=1"
    );
    let Some((store, _client, _cfg)) = test_store().await else {
        return;
    };
    let svc = Arc::new(MeshService::from_store(store.clone()));

    let project = "negctl".to_owned();
    seed_project(&svc, &project).await;
    let task_id = seed_task(&svc, &project, "t").await;
    svc.claim_task(&ClaimTask {
        id: task_id.clone(),
        actor: "stress".to_owned(),
    })
    .await
    .expect("claim");
    svc.update_task(UpdateTask {
        id: task_id.clone(),
        status: Some(TaskStatus::InProgress),
        actor: "stress".to_owned(),
        ..Default::default()
    })
    .await
    .expect("in_progress");

    let barrier = Arc::new(Barrier::new(RMW_WRITERS));
    let mut handles = Vec::with_capacity(RMW_WRITERS);
    for i in 0..RMW_WRITERS {
        let svc = Arc::clone(&svc);
        let id = task_id.clone();
        let bar = Arc::clone(&barrier);
        let marker = format!("neg-{i:04}");
        handles.push(tokio::spawn(async move {
            bar.wait().await;
            let res = svc.update_task(note(&id, &marker, "stress")).await;
            (marker, res)
        }));
    }
    let mut committed: BTreeSet<String> = BTreeSet::new();
    for handle in handles {
        let (marker, res) = handle.await.expect("writer panicked");
        if res.is_ok() {
            committed.insert(marker);
        }
    }
    let task = store.get_task(&task_id).await.expect("readback").value;
    let persisted: BTreeSet<String> = markers(&task, "neg-").into_iter().collect();
    assert!(
        committed.difference(&persisted).next().is_some(),
        "negative control lost no update — the oracle has no teeth (committed={}, persisted={})",
        committed.len(),
        persisted.len()
    );
}
