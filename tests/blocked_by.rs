//! Focused acceptance controls for first-class external task blockers.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    reason = "integration tests"
)]

mod common;

use common::{block_on, create_project, fresh_service};
use dossier::domain::{NewProject, NewTask, TaskListFilter, UpdateTask};
use dossier::store::{FsStore, Store, StoreError};

fn create_task(
    service: &dossier::server::MeshService,
    project: &str,
    slug: &str,
    blocked_by: Vec<String>,
) -> dossier::domain::Task {
    block_on(service.create_task(NewTask {
        project: project.to_owned(),
        phase: None,
        slug: slug.to_owned(),
        title: format!("Task {slug}"),
        body: String::new(),
        actor: "human:test".to_owned(),
        depends_on: vec!["tsk_existing_dependency".to_owned()],
        blocked_by,
    }))
    .expect("create task")
}

fn seed_project(service: &dossier::server::MeshService, slug: &str) {
    create_project(
        service,
        NewProject {
            slug: slug.to_owned(),
            title: format!("Project {slug}"),
            description: String::new(),
            actor: "human:test".to_owned(),
        },
    );
}

#[test]
fn create_update_and_clear_round_trip_without_changing_depends_on() {
    let (tmp, service) = fresh_service();
    seed_project(&service, "alpha");
    let created = create_task(
        &service,
        "alpha",
        "external",
        vec![
            " pr:itsHabib/ship#203 ".to_owned(),
            "url:https://example.com/build/42".to_owned(),
        ],
    );
    assert_eq!(
        created.blocked_by,
        vec![
            "pr:itsHabib/ship#203".to_owned(),
            "url:https://example.com/build/42".to_owned(),
        ]
    );

    let unchanged = block_on(service.update_task(UpdateTask {
        id: created.id.clone(),
        note: Some("still waiting".to_owned()),
        actor: "human:test".to_owned(),
        ..Default::default()
    }))
    .expect("omit blockers");
    assert_eq!(unchanged.blocked_by, created.blocked_by);

    let updated = block_on(service.update_task(UpdateTask {
        id: created.id.clone(),
        actor: "human:test".to_owned(),
        blocked_by: Some(vec!["pr:owner/repo#9".to_owned()]),
        ..Default::default()
    }))
    .expect("replace blockers");
    assert_eq!(updated.blocked_by, vec!["pr:owner/repo#9".to_owned()]);
    assert_eq!(
        updated.depends_on,
        vec!["tsk_existing_dependency".to_owned()]
    );

    let cleared = block_on(service.update_task(UpdateTask {
        id: created.id,
        actor: "human:test".to_owned(),
        blocked_by: Some(Vec::new()),
        ..Default::default()
    }))
    .expect("clear blockers");
    assert!(cleared.blocked_by.is_empty());
    assert_eq!(
        cleared.depends_on,
        vec!["tsk_existing_dependency".to_owned()]
    );

    let task_file = std::fs::read_dir(tmp.path().join("projects/alpha/tasks"))
        .expect("task dir")
        .next()
        .expect("task entry")
        .expect("read task entry")
        .path();
    let raw = std::fs::read_to_string(task_file).expect("read task file");
    assert!(!raw.contains("blocked_by:"), "empty field must be omitted");
}

#[test]
fn list_filter_is_exact_and_composes_with_project() {
    let (tmp, service) = fresh_service();
    seed_project(&service, "alpha");
    seed_project(&service, "beta");
    let wanted = create_task(
        &service,
        "alpha",
        "wanted",
        vec!["pr:owner/repo#20".to_owned()],
    );
    let _prefix = create_task(
        &service,
        "alpha",
        "prefix",
        vec!["pr:owner/repo#200".to_owned()],
    );
    let _other_project = create_task(
        &service,
        "beta",
        "other",
        vec!["pr:owner/repo#20".to_owned()],
    );

    let store = FsStore::open(tmp.path()).expect("reopen store");
    let tasks = store
        .list_tasks(&TaskListFilter {
            project: Some("alpha".to_owned()),
            blocked_by: Some("pr:owner/repo#20".to_owned()),
            ..Default::default()
        })
        .expect("list filtered tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, wanted.id);
}

#[test]
fn storage_boundary_rejects_invalid_and_trims_valid_refs() {
    let (tmp, service) = fresh_service();
    seed_project(&service, "alpha");
    let task = create_task(&service, "alpha", "stored", Vec::new());
    let store = FsStore::open(tmp.path()).expect("reopen store");
    let versioned = block_on(Store::get_task(&store, &task.id)).expect("get versioned task");

    let mut invalid = versioned.value.clone();
    invalid.blocked_by = vec!["tsk_not_an_external_ref".to_owned()];
    let err = block_on(Store::put_task(
        &store,
        &invalid,
        Some(versioned.version.clone()),
    ))
    .expect_err("store rejects invalid blocker");
    assert!(
        matches!(err, StoreError::Invalid(message) if message.contains("invalid blocked_by reference"))
    );

    let mut valid = versioned.value;
    valid.blocked_by = vec!["  pr:owner/repo#7  ".to_owned()];
    block_on(Store::put_task(&store, &valid, Some(versioned.version)))
        .expect("store accepts valid blocker");
    let reread = block_on(Store::get_task(&store, &task.id)).expect("reread task");
    assert_eq!(reread.value.blocked_by, vec!["pr:owner/repo#7"]);
}
