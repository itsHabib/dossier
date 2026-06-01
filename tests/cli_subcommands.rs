//! Integration tests for one-shot CLI subcommands.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "integration tests"
)]

mod common;

use std::process::Command;

use dossier::domain::{Task, TaskListFilter, TaskStatus};
use dossier::store::{ClaimTask, CompleteTask, FsStore, NewProject, NewTask, UpdateTask};
use serde::de::DeserializeOwned;
use serde_json::Value;

fn dossier_bin() -> &'static str {
    env!("CARGO_BIN_EXE_dossier")
}

fn run_cli(corpus: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(dossier_bin())
        .arg("--corpus")
        .arg(corpus)
        .args(args)
        .output()
        .expect("spawn dossier");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    (code, stdout, stderr)
}

fn parse_json<T: DeserializeOwned>(raw: &str) -> T {
    serde_json::from_str(raw.trim()).expect("parse json")
}

fn seed_project(store: &FsStore, slug: &str) {
    store
        .create_project(NewProject {
            slug: slug.to_owned(),
            title: format!("Project {slug}"),
            description: String::new(),
            actor: "human:test".to_owned(),
        })
        .expect("create project");
}

fn seed_task(store: &FsStore, project: &str, slug: &str) -> Task {
    store
        .create_task(NewTask {
            project: project.to_owned(),
            phase: None,
            slug: slug.to_owned(),
            title: format!("Task {slug}"),
            body: "spec body".to_owned(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
        })
        .expect("create task")
}

fn advance_to_in_progress(store: &FsStore, id: &str) {
    store
        .claim_task(&ClaimTask {
            id: id.to_owned(),
            actor: "human:test".to_owned(),
        })
        .expect("claim");
    store
        .update_task(UpdateTask {
            id: id.to_owned(),
            status: Some(TaskStatus::InProgress),
            actor: "human:test".to_owned(),
            ..Default::default()
        })
        .expect("advance");
}

#[test]
fn cli_task_complete_transitions_in_progress_task_to_done() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");
    let task = seed_task(&store, "alpha", "ship-it");
    advance_to_in_progress(&store, &task.id);

    let (code, stdout, stderr) = run_cli(
        tmp.path(),
        &[
            "task_complete",
            "--id",
            &task.id,
            "--note",
            "shipped via cli",
            "--actor",
            "cli:test",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr}");

    let done: Task = parse_json(&stdout);
    assert_eq!(done.status, TaskStatus::Done);
    assert!(done.completed_at.is_some());
}

#[test]
fn cli_task_complete_is_idempotent_when_already_done() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");
    let task = seed_task(&store, "alpha", "already-done");
    advance_to_in_progress(&store, &task.id);
    store
        .complete_task(CompleteTask {
            id: task.id.clone(),
            note: None,
            actor: "human:test".to_owned(),
        })
        .expect("complete");

    let (code, stdout, stderr) = run_cli(
        tmp.path(),
        &["task_complete", "--id", &task.id, "--actor", "cli:test"],
    );
    assert_eq!(code, 0);
    assert!(stderr.contains("already complete (no-op)"));

    let again: Task = parse_json(&stdout);
    assert_eq!(again.status, TaskStatus::Done);
}

#[test]
fn cli_task_update_appends_duplicate_notes() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");
    let task = seed_task(&store, "alpha", "note-me");

    for note in ["first", "first"] {
        let (code, stdout, stderr) = run_cli(
            tmp.path(),
            &[
                "task_update",
                "--id",
                &task.id,
                "--note",
                note,
                "--actor",
                "cli:test",
            ],
        );
        assert_eq!(code, 0, "stderr: {stderr}");
        let _: Task = parse_json(&stdout);
    }

    let raw = std::fs::read_to_string(
        store
            .root()
            .join("projects/alpha/tasks")
            .read_dir()
            .expect("tasks dir")
            .find_map(|e| {
                let e = e.ok()?;
                let path = e.path();
                if path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| stem.starts_with(&task.id))
                {
                    Some(path)
                } else {
                    None
                }
            })
            .expect("task file"),
    )
    .expect("read task");
    assert_eq!(raw.matches("first").count(), 2);
}

#[test]
fn cli_artifact_link_dedupes_same_tuple() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");

    let args = [
        "artifact_link",
        "--project",
        "alpha",
        "--kind",
        "pr",
        "--ref",
        "https://example/pr/1",
        "--label",
        "PR #1",
        "--actor",
        "cli:test",
    ];
    let (code, stdout, stderr) = run_cli(tmp.path(), &args);
    assert_eq!(code, 0, "stderr: {stderr}");
    let first: Value = parse_json(&stdout);

    let (code, stdout, stderr) = run_cli(tmp.path(), &args);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stderr.contains("already linked"));
    let second: Value = parse_json(&stdout);
    assert_eq!(first["id"], second["id"]);
    assert_eq!(store.list_artifacts("alpha").expect("list").len(), 1);
}

#[test]
fn cli_artifact_link_appends_different_tuple() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");

    for reference in ["https://example/pr/1", "https://example/pr/2"] {
        let (code, _, stderr) = run_cli(
            tmp.path(),
            &[
                "artifact_link",
                "--project",
                "alpha",
                "--kind",
                "pr",
                "--ref",
                reference,
                "--label",
                "label",
                "--actor",
                "cli:test",
            ],
        );
        assert_eq!(code, 0, "stderr: {stderr}");
    }

    assert_eq!(store.list_artifacts("alpha").expect("list").len(), 2);
}

#[test]
fn cli_artifact_link_rejects_explicit_empty_task() {
    // Regression for codex PR #38 P2: ensure --task "" is rejected before the
    // existing-artifact dedupe short-circuit (which would otherwise return Ok
    // when a project-wide artifact already exists).
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");

    // Pre-seed a project-wide artifact so the dedupe path is reachable.
    let (code, _, stderr) = run_cli(
        tmp.path(),
        &[
            "artifact_link",
            "--project",
            "alpha",
            "--kind",
            "pr",
            "--ref",
            "https://example/pr/1",
            "--label",
            "PR #1",
            "--actor",
            "cli:test",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");

    // Now call with --task "" + the same tuple. Without the guard this would
    // hit the dedupe short-circuit and return success.
    let (code, _, stderr) = run_cli(
        tmp.path(),
        &[
            "artifact_link",
            "--project",
            "alpha",
            "--task",
            "",
            "--kind",
            "pr",
            "--ref",
            "https://example/pr/1",
            "--label",
            "PR #1",
            "--actor",
            "cli:test",
        ],
    );
    assert_ne!(code, 0, "expected non-zero exit for empty --task");
    assert!(stderr.contains("task is empty"), "stderr: {stderr}");
}

#[test]
fn cli_task_list_filters_match_store() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");
    let todo = seed_task(&store, "alpha", "todo-one");
    let active = seed_task(&store, "alpha", "active-one");
    advance_to_in_progress(&store, &active.id);

    let filter = TaskListFilter {
        project: Some("alpha".to_owned()),
        status: Some(vec![TaskStatus::Todo, TaskStatus::InProgress]),
        ..TaskListFilter::default()
    };

    let (code, stdout, stderr) = run_cli(
        tmp.path(),
        &[
            "task_list",
            "--project",
            "alpha",
            "--status",
            "todo,in_progress",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let cli_tasks: Vec<Task> = parse_json(&stdout);
    let store_tasks = store.list_tasks(&filter).expect("store list");

    assert_eq!(cli_tasks.len(), 2);
    assert_eq!(cli_tasks.len(), store_tasks.len());
    let cli_json = serde_json::to_value(&cli_tasks).expect("cli json");
    let store_json = serde_json::to_value(&store_tasks).expect("store json");
    assert_eq!(cli_json, store_json);
    let cli_ids: Vec<_> = cli_tasks.iter().map(|t| t.id.as_str()).collect();
    assert!(cli_ids.contains(&todo.id.as_str()));
    assert!(cli_ids.contains(&active.id.as_str()));
}

#[test]
fn cli_json_matches_store_for_write_verbs() {
    let (tmp, store) = common::fresh_corpus();
    seed_project(&store, "alpha");
    let task = seed_task(&store, "alpha", "parity");
    advance_to_in_progress(&store, &task.id);
    let corpus = tmp.path();

    let (code, stdout, stderr) = run_cli(
        corpus,
        &[
            "task_update",
            "--id",
            &task.id,
            "--note",
            "progress",
            "--actor",
            "cli:test",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let cli_update: Value = parse_json(&stdout);
    assert_eq!(cli_update["id"], Value::String(task.id.clone()));
    assert_eq!(
        cli_update["status"],
        Value::String("in_progress".to_owned())
    );
    assert_eq!(
        cli_update["notes"][0]["body"],
        Value::String("progress".to_owned())
    );

    let (code, stdout, stderr) = run_cli(
        corpus,
        &[
            "task_complete",
            "--id",
            &task.id,
            "--note",
            "done",
            "--actor",
            "cli:test",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let cli_done: Value = parse_json(&stdout);
    assert_eq!(cli_done["status"], Value::String("done".to_owned()));
    assert!(cli_done["completed_at"].is_string());

    let (code, stdout, stderr) = run_cli(
        corpus,
        &[
            "artifact_link",
            "--project",
            "alpha",
            "--kind",
            "pr",
            "--ref",
            "https://example/pr/9",
            "--label",
            "PR #9",
            "--actor",
            "cli:test",
        ],
    );
    assert_eq!(code, 0, "stderr: {stderr}");
    let cli_art: Value = parse_json(&stdout);
    assert_eq!(cli_art["kind"], Value::String("pr".to_owned()));
    assert_eq!(
        cli_art["ref"],
        Value::String("https://example/pr/9".to_owned())
    );
    assert_eq!(store.list_artifacts("alpha").expect("list").len(), 1);
}

#[test]
fn cli_task_list_rejects_phase_without_project() {
    let (tmp, _store) = common::fresh_corpus();
    let (code, _, stderr) = run_cli(tmp.path(), &["task_list", "--phase", "integration-layer"]);
    assert_ne!(code, 0);
    assert!(stderr.contains("phase requires project"));
}

#[test]
fn cli_serve_requires_explicit_corpus() {
    let (tmp, _store) = common::fresh_corpus();
    // Scrub DOSSIER_CORPUS from the child env. Clap's `--corpus` arg uses
    // `env = "DOSSIER_CORPUS"`, so an operator with the var exported in
    // their shell would otherwise see clap auto-fill it and skip the
    // "--corpus is required" branch this test asserts on.
    let output = Command::new(dossier_bin())
        .current_dir(tmp.path())
        .env_remove("DOSSIER_CORPUS")
        .arg("serve")
        .output()
        .expect("spawn serve");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("--corpus is required"));
}
