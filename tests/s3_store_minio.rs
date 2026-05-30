//! `MinIO` integration tests for `S3Store` CAS semantics.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "integration tests"
)]

use std::sync::Arc;

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use chrono::Utc;
use dossier::domain::{Artifact, Project, ProjectStatus};
use dossier::store::{Store, StoreError, Version};
use dossier::{S3Config, S3Store};
use ulid::Ulid;

fn endpoint_configured() -> Option<String> {
    std::env::var("DOSSIER_S3_TEST_ENDPOINT").ok()
}

async fn test_store() -> Option<(S3Store, S3Config)> {
    let endpoint = endpoint_configured()?;
    let access_key = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".to_owned());
    let secret_key =
        std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_owned());
    let cfg = S3Config {
        bucket: "dossier-it".to_owned(),
        prefix: format!("it/{}", Ulid::new()),
        endpoint_url: Some(endpoint),
        region: "us-east-1".to_owned(),
        access_key_id: access_key,
        secret_access_key: secret_key,
        force_path_style: true,
    };
    ensure_bucket(&cfg).await.ok()?;
    let store = S3Store::new(cfg.clone()).await.ok()?;
    Some((store, cfg))
}

async fn ensure_bucket(cfg: &S3Config) -> Result<(), StoreError> {
    let creds = Credentials::new(
        cfg.access_key_id.clone(),
        cfg.secret_access_key.clone(),
        None,
        None,
        "dossier-s3-it",
    );
    let shared = aws_config::defaults(BehaviorVersion::latest())
        .region(aws_config::Region::new(cfg.region.clone()))
        .credentials_provider(creds)
        .load()
        .await;
    let mut s3_builder = aws_sdk_s3::config::Builder::from(&shared);
    if let Some(url) = &cfg.endpoint_url {
        s3_builder = s3_builder.endpoint_url(url);
    }
    s3_builder = s3_builder.force_path_style(cfg.force_path_style);
    let client = Client::from_conf(s3_builder.build());
    match client.create_bucket().bucket(&cfg.bucket).send().await {
        Ok(_) => Ok(()),
        Err(err) => {
            let msg = format!("{err:?}");
            if msg.contains("BucketAlreadyOwnedByYou") || msg.contains("BucketAlreadyExists") {
                Ok(())
            } else {
                Err(StoreError::Unavailable)
            }
        }
    }
}

fn sample_project(slug: &str) -> Project {
    let now = Utc::now();
    Project {
        id: format!("prj_{}", Ulid::new()),
        slug: slug.to_owned(),
        title: "Integration project".to_owned(),
        description: "CAS proof".to_owned(),
        status: ProjectStatus::Active,
        created_at: now,
        updated_at: now,
        created_by: "human:test".to_owned(),
    }
}

#[tokio::test]
async fn create_only_success_and_duplicate_conflict() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let project = sample_project("create-only");
    let v0 = store.put_project(&project, None).await.expect("create");
    assert!(!v0.as_str().is_empty());
    let err = store.put_project(&project, None).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict));
}

#[tokio::test]
async fn cas_update_success_and_stale_conflict() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let mut project = sample_project("cas-update");
    let v0 = store.put_project(&project, None).await.expect("create");
    project.title = "Updated".to_owned();
    let v1 = store
        .put_project(&project, Some(v0.clone()))
        .await
        .expect("cas update");
    assert_ne!(v0.as_str(), v1.as_str());
    project.title = "Stale".to_owned();
    let err = store.put_project(&project, Some(v0)).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict));
}

#[tokio::test]
async fn get_put_roundtrip_etag_stable() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let mut project = sample_project("etag-roundtrip");
    let v0 = store.put_project(&project, None).await.expect("create");
    let got = store.get_project(&project.slug).await.expect("get");
    assert_eq!(got.version.as_str(), v0.as_str());
    assert_eq!(got.value.title, project.title);
    project.description = "mutated".to_owned();
    store
        .put_project(&project, Some(got.version))
        .await
        .expect("update with etag");
}

#[tokio::test]
async fn update_if_absent_on_missing_key_is_not_found() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let project = sample_project("never-created");
    let err = store
        .put_project(&project, Some(Version::new("stale")))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound));
}

#[tokio::test]
async fn concurrent_writer_race_exactly_one_wins() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let store = Arc::new(store);
    for i in 0..30 {
        let slug = format!("race-{i}");
        let mut project = sample_project(&slug);
        let v0 = store.put_project(&project, None).await.expect("seed");
        project.title = format!("winner-a-{i}");
        let mut project_b = project.clone();
        project_b.title = format!("winner-b-{i}");
        let store_a = Arc::clone(&store);
        let store_b = Arc::clone(&store);
        let v0_a = v0.clone();
        let v0_b = v0;
        let slug_a = slug.clone();
        let slug_b = slug;
        let handle_a = tokio::spawn(async move { store_a.put_project(&project, Some(v0_a)).await });
        let handle_b =
            tokio::spawn(async move { store_b.put_project(&project_b, Some(v0_b)).await });
        let res_a = handle_a.await.expect("join a");
        let res_b = handle_b.await.expect("join b");
        let ok_count = usize::from(res_a.is_ok()) + usize::from(res_b.is_ok());
        let conflict_count = usize::from(matches!(res_a, Err(StoreError::Conflict)))
            + usize::from(matches!(res_b, Err(StoreError::Conflict)));
        assert_eq!(ok_count, 1, "iteration {i}: slug {slug_a}/{slug_b}");
        assert_eq!(conflict_count, 1, "iteration {i}");
    }
}

#[tokio::test]
async fn artifact_roundtrip() {
    let Some((store, _cfg)) = test_store().await else {
        return;
    };
    let project = sample_project("artifacts");
    store
        .put_project(&project, None)
        .await
        .expect("create project");
    let artifact = Artifact {
        id: format!("art_{}", Ulid::new()),
        project: project.id.clone(),
        task: String::new(),
        kind: "commit".to_owned(),
        reference: "abc123".to_owned(),
        label: "test".to_owned(),
        linked_at: Utc::now(),
        actor: "human:test".to_owned(),
    };
    store.put_artifact(&artifact).await.expect("put artifact");
    let listed = store
        .list_artifacts(dossier::store::ArtifactListFilter {
            project: project.slug.clone(),
        })
        .await
        .expect("list artifacts");
    assert!(listed.iter().any(|a| a.id == artifact.id));
}
