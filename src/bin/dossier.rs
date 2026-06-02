use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rmcp::{transport::stdio, ServiceExt};
use serde::Serialize;

use dossier::domain::{Task, TaskListFilter, TaskStatus};
use dossier::server::MeshService;
use dossier::store::{
    store_error_to_anyhow, CompleteTask, FsStore, LinkArtifact, Store, UpdateTask,
};
use dossier::{S3Config, S3Store};

#[derive(Parser)]
#[command(
    name = "dossier",
    about = "Project memory for the solo developer",
    after_help = "The corpus is any directory containing a .dossier/ marker. See LAYOUT.md\nfor the on-disk format."
)]
struct Cli {
    /// corpus root; for one-shot subcommands also falls back to `DOSSIER_CORPUS`
    /// or a walk-up search for `.dossier/`
    #[arg(long, global = true, env = "DOSSIER_CORPUS")]
    corpus: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio against the corpus at `--corpus`
    Serve,
    /// Mark a task done (MCP `task.complete`)
    #[command(name = "task_complete")]
    TaskComplete {
        /// task id (`tsk_` + ULID)
        #[arg(long)]
        id: String,
        /// optional closing note
        #[arg(long)]
        note: Option<String>,
        /// who's completing the task; default `cli:$USER` (or `cli`)
        #[arg(long)]
        actor: Option<String>,
    },
    /// Append a note to a task (MCP `task.update`)
    #[command(name = "task_update")]
    TaskUpdate {
        /// task id (`tsk_` + ULID)
        #[arg(long)]
        id: String,
        /// note line appended to the task's `## Notes` log
        #[arg(long)]
        note: String,
        /// who's making the update; default `cli:$USER` (or `cli`)
        #[arg(long)]
        actor: Option<String>,
    },
    /// Link an artifact to a project (MCP `artifact.link`)
    #[command(name = "artifact_link")]
    ArtifactLink {
        /// project slug
        #[arg(long)]
        project: String,
        /// artifact kind (`commit`, `pr`, `file`, `url`, `run`, `doc`, …)
        #[arg(long)]
        kind: String,
        /// artifact pointer (SHA, URL, path, run id)
        #[arg(long = "ref")]
        reference: String,
        /// task id to anchor this artifact to; omit for project-wide
        #[arg(long)]
        task: Option<String>,
        /// short human-readable label; defaults to ref
        #[arg(long)]
        label: Option<String>,
        /// who's linking the artifact; default `cli:$USER` (or `cli`)
        #[arg(long)]
        actor: Option<String>,
    },
    /// List tasks subject to filters (MCP `task.list`)
    #[command(name = "task_list")]
    TaskList {
        /// project slug; omit to list across every project
        #[arg(long)]
        project: Option<String>,
        /// phase slug; requires `--project`
        #[arg(long)]
        phase: Option<String>,
        /// task status filter (repeatable or comma-separated)
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        /// exact match against assignee frontmatter
        #[arg(long)]
        assignee: Option<String>,
        /// cap the number of returned rows
        #[arg(long)]
        limit: Option<usize>,
    },
}

struct ArtifactLinkRequest {
    project: String,
    kind: String,
    reference: String,
    task: Option<String>,
    label: Option<String>,
    actor: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let corpus = cli
                .corpus
                .context("--corpus is required (the mcp server has no cwd context)")?;
            run_serve(&corpus).await
        }
        Command::TaskComplete { id, note, actor } => {
            run_task_complete(&resolve_corpus(cli.corpus)?, id, note, actor).await
        }
        Command::TaskUpdate { id, note, actor } => {
            run_task_update(&resolve_corpus(cli.corpus)?, id, note, actor).await
        }
        Command::ArtifactLink {
            project,
            kind,
            reference,
            task,
            label,
            actor,
        } => {
            run_artifact_link(
                &resolve_corpus(cli.corpus)?,
                ArtifactLinkRequest {
                    project,
                    kind,
                    reference,
                    task,
                    label,
                    actor,
                },
            )
            .await
        }
        Command::TaskList {
            project,
            phase,
            status,
            assignee,
            limit,
        } => run_task_list(
            &resolve_corpus(cli.corpus)?,
            project,
            phase,
            &status,
            assignee,
            limit,
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeBackend {
    Fs,
    S3,
}

fn parse_serve_backend(raw: Option<&str>) -> Result<ServeBackend> {
    match raw.unwrap_or("fs").trim() {
        "fs" => Ok(ServeBackend::Fs),
        "s3" => Ok(ServeBackend::S3),
        other => bail!("unknown DOSSIER_BACKEND: {other} (expected fs or s3)"),
    }
}

fn s3_config_from_env() -> Result<S3Config> {
    let bucket = std::env::var("DOSSIER_S3_BUCKET")
        .context("DOSSIER_S3_BUCKET is required when DOSSIER_BACKEND=s3")?;
    let prefix = std::env::var("DOSSIER_S3_PREFIX").unwrap_or_default();
    let endpoint_url = std::env::var("DOSSIER_S3_ENDPOINT")
        .ok()
        .filter(|value| !value.is_empty());
    let region = std::env::var("DOSSIER_S3_REGION")
        .or_else(|_| std::env::var("AWS_REGION"))
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .unwrap_or_else(|_| "us-east-1".to_owned());
    let access_key_id = std::env::var("AWS_ACCESS_KEY_ID")
        .context("AWS_ACCESS_KEY_ID is required when DOSSIER_BACKEND=s3")?;
    let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY")
        .context("AWS_SECRET_ACCESS_KEY is required when DOSSIER_BACKEND=s3")?;
    let force_path_style = match std::env::var("DOSSIER_S3_FORCE_PATH_STYLE") {
        Ok(raw) => parse_env_bool(&raw)
            .with_context(|| format!("invalid DOSSIER_S3_FORCE_PATH_STYLE: {raw}"))?,
        Err(_) => endpoint_url.is_some(),
    };
    Ok(S3Config {
        bucket,
        prefix,
        endpoint_url,
        region,
        access_key_id,
        secret_access_key,
        force_path_style,
    })
}

fn parse_env_bool(raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        other => bail!("expected true/false, got {other}"),
    }
}

async fn open_serve_store(backend: ServeBackend, corpus: &Path) -> Result<Arc<dyn Store>> {
    match backend {
        ServeBackend::Fs => Ok(Arc::new(FsStore::open(corpus)?)),
        ServeBackend::S3 => {
            let store = S3Store::new(s3_config_from_env()?)
                .await
                .map_err(store_error_to_anyhow)?;
            Ok(Arc::new(store))
        }
    }
}

async fn run_serve(corpus: &Path) -> Result<()> {
    let backend = parse_serve_backend(std::env::var("DOSSIER_BACKEND").ok().as_deref())?;
    let service = MeshService::from_store(open_serve_store(backend, corpus).await?);
    let server = service.serve(stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn resolve_corpus(flag: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = flag {
        return Ok(path);
    }
    let mut dir = std::env::current_dir().context("resolve current directory")?;
    loop {
        if dir.join(".dossier").is_dir() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("no dossier corpus found (pass --corpus or set DOSSIER_CORPUS)");
        }
    }
}

fn default_actor(explicit: Option<String>) -> String {
    explicit.filter(|s| !s.is_empty()).unwrap_or_else(|| {
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .map_or_else(|_| "cli".to_owned(), |user| format!("cli:{user}"))
    })
}

fn emit_json<T: Serialize>(value: &T) -> Result<()> {
    serde_json::to_writer(io::stdout(), value).context("serialize json")?;
    io::stdout().write_all(b"\n").context("write stdout")?;
    Ok(())
}

fn parse_task_statuses(raw: &[String]) -> Result<Option<Vec<TaskStatus>>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(raw.len());
    for s in raw {
        out.push(parse_task_status(s)?);
    }
    Ok(Some(out))
}

fn parse_task_status(raw: &str) -> Result<TaskStatus> {
    match raw.trim() {
        "todo" => Ok(TaskStatus::Todo),
        "claimed" => Ok(TaskStatus::Claimed),
        "in_progress" => Ok(TaskStatus::InProgress),
        "blocked" => Ok(TaskStatus::Blocked),
        "done" => Ok(TaskStatus::Done),
        "cancelled" => Ok(TaskStatus::Cancelled),
        other => bail!("invalid task status: {other}"),
    }
}

async fn run_task_complete(
    corpus: &Path,
    id: String,
    note: Option<String>,
    actor: Option<String>,
) -> Result<()> {
    let store = FsStore::open(corpus)?;
    if let Some(existing) = store.get_task(&id)? {
        if existing.status == TaskStatus::Done {
            eprintln!("task {id} already complete (no-op)");
            emit_json(&existing)?;
            return Ok(());
        }
    }
    let svc = MeshService::new(store);
    let task = svc
        .complete_task(CompleteTask {
            id,
            note,
            actor: default_actor(actor),
        })
        .await
        .map_err(store_error_to_anyhow)?;
    emit_json(&task)
}

async fn run_task_update(
    corpus: &Path,
    id: String,
    note: String,
    actor: Option<String>,
) -> Result<()> {
    let svc = MeshService::new(FsStore::open(corpus)?);
    let task = svc
        .update_task(UpdateTask {
            id,
            body: None,
            status: None,
            note: Some(note),
            actor: default_actor(actor),
            depends_on: None,
        })
        .await
        .map_err(store_error_to_anyhow)?;
    emit_json(&task)
}

async fn run_artifact_link(corpus: &Path, req: ArtifactLinkRequest) -> Result<()> {
    if matches!(req.task.as_deref(), Some("")) {
        bail!("task is empty");
    }
    let store = FsStore::open(corpus)?;
    let task_key = req.task.clone().unwrap_or_default();
    let label = req.label.unwrap_or_else(|| req.reference.clone());
    if let Some(existing) =
        find_linked_artifact(&store, &req.project, &task_key, &req.kind, &req.reference)?
    {
        eprintln!(
            "artifact already linked for project {} kind {} ref {} (no-op)",
            req.project, req.kind, req.reference
        );
        emit_json(&existing)?;
        return Ok(());
    }
    let svc = MeshService::new(store);
    let artifact = svc
        .link_artifact(LinkArtifact {
            project: req.project,
            task: req.task,
            kind: req.kind,
            reference: req.reference,
            label,
            actor: default_actor(req.actor),
        })
        .await
        .map_err(store_error_to_anyhow)?;
    emit_json(&artifact)
}

fn find_linked_artifact(
    store: &FsStore,
    project: &str,
    task: &str,
    kind: &str,
    reference: &str,
) -> Result<Option<dossier::domain::Artifact>> {
    Ok(store
        .list_artifacts(project)?
        .into_iter()
        .find(|a| a.task == task && a.kind == kind && a.reference == reference))
}

fn run_task_list(
    corpus: &Path,
    project: Option<String>,
    phase: Option<String>,
    status: &[String],
    assignee: Option<String>,
    limit: Option<usize>,
) -> Result<()> {
    if phase.is_some() && project.is_none() {
        bail!("phase requires project (phase slugs are unique per project, not across the corpus)");
    }
    let filter = TaskListFilter {
        project,
        phase,
        status: parse_task_statuses(status)?,
        assignee,
        limit,
        ..TaskListFilter::default()
    };
    let store = FsStore::open(corpus)?;
    let tasks: Vec<Task> = store.list_tasks(&filter)?;
    emit_json(&tasks)
}

#[cfg(test)]
mod backend_tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        clippy::indexing_slicing,
        reason = "unit tests"
    )]

    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use dossier::store::ProjectListFilter;

    fn temp_corpus() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dossier-serve-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir_all(dir.join(".dossier")).expect("mkdir .dossier");
        dir
    }

    #[test]
    fn parse_serve_backend_defaults_to_fs() {
        assert_eq!(
            parse_serve_backend(None).expect("fs default"),
            ServeBackend::Fs
        );
        assert_eq!(
            parse_serve_backend(Some("fs")).expect("fs explicit"),
            ServeBackend::Fs
        );
    }

    #[test]
    fn parse_serve_backend_selects_s3() {
        assert_eq!(
            parse_serve_backend(Some("s3")).expect("s3"),
            ServeBackend::S3
        );
    }

    #[test]
    fn parse_serve_backend_rejects_unknown() {
        let err = parse_serve_backend(Some("git")).expect_err("unknown backend");
        assert!(err
            .to_string()
            .contains("unknown DOSSIER_BACKEND: git (expected fs or s3)"));
    }

    #[tokio::test]
    async fn open_serve_store_fs_opens_local_corpus() {
        let corpus = temp_corpus();
        let store = open_serve_store(ServeBackend::Fs, &corpus)
            .await
            .expect("fs store");
        let projects = store
            .list_projects(ProjectListFilter::default())
            .await
            .expect("list projects");
        assert!(projects.is_empty());
        let _ = fs::remove_dir_all(&corpus);
    }

    #[tokio::test]
    async fn open_serve_store_s3_from_env_when_minio_configured() {
        let endpoint = match std::env::var("DOSSIER_S3_TEST_ENDPOINT") {
            Ok(value) if !value.is_empty() => value,
            _ => return,
        };
        let access_key =
            std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_else(|_| "minioadmin".to_owned());
        let secret_key =
            std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_else(|_| "minioadmin".to_owned());
        std::env::set_var("DOSSIER_S3_BUCKET", "dossier-it");
        std::env::set_var(
            "DOSSIER_S3_PREFIX",
            format!("it/serve-{}", ulid::Ulid::new()),
        );
        std::env::set_var("DOSSIER_S3_ENDPOINT", &endpoint);
        std::env::set_var("AWS_ACCESS_KEY_ID", &access_key);
        std::env::set_var("AWS_SECRET_ACCESS_KEY", &secret_key);
        let corpus = temp_corpus();
        open_serve_store(ServeBackend::S3, &corpus)
            .await
            .expect("s3 store");
        let _ = fs::remove_dir_all(&corpus);
    }
}
