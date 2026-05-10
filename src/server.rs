//! `MCP` server wrapping the `FsStore` as dossier verbs.
//!
//! v0 verbs: read side (list / get for projects, phases, tasks,
//! artifacts) plus write side for projects (create / update). Phase,
//! task, artifact writes and conflict detection land in subsequent
//! phases.

use std::sync::{Arc, Mutex};

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{Artifact, Phase, Project, ProjectStatus, Task};
use crate::store::{FsStore, NewProject, UpdateProject};

/// Implementation version of the dossier mesh. Distinct from the
/// protocol version (which lives in PROTOCOL.md, currently v0).
pub const VERSION: &str = "0.1.0";

/// Service holding a shared `FsStore` and a process-local write lock.
///
/// The `Arc<FsStore>` lets `rmcp` clone the service cheaply across
/// handler invocations; the `Mutex<()>` serializes writes inside this
/// process so concurrent tool calls don't race each other.
#[derive(Clone)]
pub struct MeshService {
    store: Arc<FsStore>,
    write_lock: Arc<Mutex<()>>,
}

impl MeshService {
    pub fn new(store: FsStore) -> Self {
        Self {
            store: Arc::new(store),
            write_lock: Arc::new(Mutex::new(())),
        }
    }
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectGetArgs {
    /// project slug, e.g. "dossier"
    pub slug: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectView {
    pub project: Project,
    pub phases: Vec<Phase>,
    pub tasks: Vec<Task>,
    pub artifacts: Vec<Artifact>,
}

#[derive(Deserialize, JsonSchema)]
pub struct PhaseListArgs {
    /// project slug
    pub project: String,
}

#[derive(Serialize, JsonSchema)]
pub struct PhaseListResult {
    pub phases: Vec<Phase>,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskListArgs {
    /// project slug
    pub project: String,
    /// if set, only tasks in this phase (matched by phase id)
    #[serde(default)]
    pub phase: String,
    /// if set, only tasks with this status (`todo`, `claimed`,
    /// `in_progress`, `blocked`, `done`, `cancelled`)
    #[serde(default)]
    pub status: String,
}

#[derive(Serialize, JsonSchema)]
pub struct TaskListResult {
    pub tasks: Vec<Task>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ArtifactListArgs {
    /// project slug
    pub project: String,
    /// if set, only artifacts linked to this task ID
    #[serde(default)]
    pub task: String,
    /// if set, only artifacts of this kind (commit, pr, file, url, run, doc)
    #[serde(default)]
    pub kind: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ArtifactListResult {
    pub artifacts: Vec<Artifact>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectCreateArgs {
    /// project slug — lowercase ASCII (a-z, 0-9, `-`, `_`); must be unique
    pub slug: String,
    /// human-readable project title
    pub title: String,
    /// project body / design doc (markdown)
    #[serde(default)]
    pub description: String,
    /// who's creating the project (e.g. `human:mh`, `claude-code:michael`)
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectUpdateArgs {
    /// project slug (addressing key — slug is immutable in v0)
    pub slug: String,
    /// new title; omit to leave unchanged
    #[serde(default)]
    pub title: Option<String>,
    /// new description body; omit to leave unchanged
    #[serde(default)]
    pub description: Option<String>,
    /// new status; omit to leave unchanged
    /// (`planning` | `active` | `paused` | `done` | `abandoned`)
    #[serde(default)]
    pub status: Option<ProjectStatus>,
    /// who's making the update (provenance only; not stored on the project itself in v0)
    #[allow(dead_code)] // surfaced in protocol, not yet persisted as a separate field
    pub actor: String,
}

#[tool_router(server_handler)]
impl MeshService {
    #[tool(
        name = "project.list",
        description = "List every project in the corpus, sorted by slug. Returns metadata only — call project.get for the full description body."
    )]
    fn project_list(&self) -> Result<Json<ProjectListResult>, ErrorData> {
        let projects = self.store.list_projects().map_err(internal)?;
        Ok(Json(ProjectListResult { projects }))
    }

    #[tool(
        name = "project.create",
        description = "Create a new project. Slug must be unique within the corpus and lowercase ASCII. Server stamps id, created_at, updated_at, and an initial 'planning' status."
    )]
    fn project_create(
        &self,
        Parameters(args): Parameters<ProjectCreateArgs>,
    ) -> Result<Json<Project>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let project = self
            .store
            .create_project(NewProject {
                slug: args.slug,
                title: args.title,
                description: args.description,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(project))
    }

    #[tool(
        name = "project.update",
        description = "Update mutable fields of a project (title, description, status). Slug is the addressing key and is immutable. Preserves id and created_at; bumps updated_at."
    )]
    fn project_update(
        &self,
        Parameters(args): Parameters<ProjectUpdateArgs>,
    ) -> Result<Json<Project>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let project = self
            .store
            .update_project(UpdateProject {
                slug: args.slug,
                title: args.title,
                description: args.description,
                status: args.status,
            })
            .map_err(internal)?;
        Ok(Json(project))
    }

    #[tool(
        name = "project.get",
        description = "Get one project by slug, including phases, tasks, artifacts, and full description body."
    )]
    fn project_get(
        &self,
        Parameters(args): Parameters<ProjectGetArgs>,
    ) -> Result<Json<ProjectView>, ErrorData> {
        let project = self.store.get_project(&args.slug).map_err(internal)?;
        let phases = self.store.list_phases(&args.slug).map_err(internal)?;
        let tasks = self.store.list_tasks(&args.slug).map_err(internal)?;
        let artifacts = self.store.list_artifacts(&args.slug).map_err(internal)?;
        Ok(Json(ProjectView {
            project,
            phases,
            tasks,
            artifacts,
        }))
    }

    #[tool(
        name = "phase.list",
        description = "List the phases of a project in order. Phase bodies are included."
    )]
    fn phase_list(
        &self,
        Parameters(args): Parameters<PhaseListArgs>,
    ) -> Result<Json<PhaseListResult>, ErrorData> {
        let phases = self.store.list_phases(&args.project).map_err(internal)?;
        Ok(Json(PhaseListResult { phases }))
    }

    #[tool(
        name = "task.list",
        description = "List the tasks of a project. Optionally filter by status or phase ID."
    )]
    fn task_list(
        &self,
        Parameters(args): Parameters<TaskListArgs>,
    ) -> Result<Json<TaskListResult>, ErrorData> {
        let all = self.store.list_tasks(&args.project).map_err(internal)?;
        let tasks = all
            .into_iter()
            .filter(|t| args.phase.is_empty() || t.phase == args.phase)
            .filter(|t| {
                args.status.is_empty()
                    || serde_json::to_value(t.status)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .as_deref()
                        == Some(&args.status)
            })
            .collect();
        Ok(Json(TaskListResult { tasks }))
    }

    #[tool(
        name = "artifact.list",
        description = "List the artifacts linked to a project (commits, PRs, files, URLs, runs, docs)."
    )]
    fn artifact_list(
        &self,
        Parameters(args): Parameters<ArtifactListArgs>,
    ) -> Result<Json<ArtifactListResult>, ErrorData> {
        let all = self.store.list_artifacts(&args.project).map_err(internal)?;
        let artifacts = all
            .into_iter()
            .filter(|a| args.task.is_empty() || a.task == args.task)
            .filter(|a| args.kind.is_empty() || a.kind == args.kind)
            .collect();
        Ok(Json(ArtifactListResult { artifacts }))
    }
}

/// Map an internal error to an MCP `ErrorData`. Generic over `ToString`
/// so callers can pass `.map_err(internal)` without an extra closure;
/// the `needless_pass_by_value` lint is allowed here because `map_err`
/// hands the error to its callback by value.
#[allow(clippy::needless_pass_by_value)]
fn internal<E: ToString>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}
