//! Shared test harness for the server test modules.
//!
//! Test-only seams over [`MeshService`]; the panicky lints are gated the
//! same way as every `mod tests` block.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::cast_possible_truncation
)]

use std::path::{Path, PathBuf};

use crate::domain::{Project, Task};
use crate::store::{FsStore, NewProject, NewTask, StoreError};

use super::MeshService;

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub fn seed_store(tmp: &Path) -> FsStore {
    FsStore::open(tmp).expect("open seed store")
}

pub fn seed_project(svc: &MeshService, slug: &str) -> Project {
    block_on(svc.create_project(NewProject {
        slug: slug.to_owned(),
        title: format!("Project {slug}"),
        description: String::new(),
        actor: "human:test".to_owned(),
    }))
    .expect("seed project")
}

pub fn seed_task(svc: &MeshService, project: &str, slug: &str) -> Task {
    block_on(svc.create_task(NewTask {
        project: project.to_owned(),
        phase: None,
        slug: slug.to_owned(),
        title: slug.to_owned(),
        body: String::new(),
        actor: "human:test".to_owned(),
        depends_on: Vec::new(),
        blocked_by: Vec::new(),
    }))
    .expect("seed task")
}

pub fn task_file_path(corpus: &Path, task: &Task) -> PathBuf {
    corpus
        .join("projects")
        .join(&task.project_slug)
        .join("tasks")
        .join(format!("{}-{}.md", task.id, task.slug))
}

pub fn set_task_field(corpus: &Path, task: &Task, field: &str, value: &str) {
    use std::fmt::Write as _;
    let path = task_file_path(corpus, task);
    let raw = std::fs::read_to_string(&path).unwrap();
    let needle = format!("{field}: ");
    let parts: Vec<&str> = raw.splitn(3, "---").collect();
    assert_eq!(parts.len(), 3, "task file missing frontmatter delimiters");
    let front = parts[1];
    let mut new_front = String::new();
    let mut replaced = false;
    for line in front.lines() {
        if line.starts_with(&needle) {
            let _ = writeln!(new_front, "{field}: {value}");
            replaced = true;
        } else {
            new_front.push_str(line);
            new_front.push('\n');
        }
    }
    if !replaced {
        let _ = writeln!(new_front, "{field}: {value}");
    }
    let body = parts[2].trim_start_matches('\n');
    let new_raw = format!("---{new_front}---\n{body}");
    std::fs::write(&path, new_raw).unwrap();
}

pub fn set_task_body(corpus: &Path, task: &Task, body: &str) {
    let path = task_file_path(corpus, task);
    let raw = std::fs::read_to_string(&path).unwrap();
    let parts: Vec<&str> = raw.splitn(3, "---").collect();
    assert_eq!(parts.len(), 3, "task file missing frontmatter delimiters");
    let after_front = parts[2];
    let notes = after_front
        .find("## Notes")
        .map_or("", |i| &after_front[i..]);
    let front = parts[1];
    let new_raw = if notes.is_empty() {
        format!("---{front}---\n\n{body}\n")
    } else {
        format!("---{front}---\n\n{body}\n\n{notes}")
    };
    std::fs::write(&path, new_raw).unwrap();
}

pub fn fresh_service() -> (tempfile::TempDir, MeshService) {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
    let store = FsStore::open(tmp.path()).expect("open fresh corpus");
    let service = MeshService::new(store);
    (tmp, service)
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(future)
}

pub const INVALID_PROJECT_SLUG: &str = "Bad-Slug";

pub fn assert_rejects_invalid_project_slug(err: StoreError, slug: &str) {
    let StoreError::Invalid(msg) = err else {
        panic!("expected invalid error, got {err:?}");
    };
    assert!(
        msg.contains("slug must be lowercase ascii"),
        "unexpected message: {msg}"
    );
    assert!(msg.contains(slug), "message should include slug: {msg}");
}
