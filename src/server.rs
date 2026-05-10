//! `MCP` server wrapping the `FsStore` as Agent Project Protocol verbs.
//!
//! v0 covers the read side only: list projects/phases/tasks/artifacts
//! and fetch a hydrated project view. Write verbs land in a later phase.

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{Artifact, Phase, Project, Task};
use crate::store::FsStore;

/// Implementation version of the dossier mesh. Distinct from the
/// protocol version (which lives in PROTOCOL.md, currently v0).
pub const VERSION: &str = "0.1.0";

/// Stateless service: holds a shared `FsStore` behind an `Arc` so the
/// `rmcp` runtime can clone it across handler invocations without cost.
#[derive(Clone)]
pub struct MeshService {
    store: Arc<FsStore>,
}

impl MeshService {
    pub fn new(store: FsStore) -> Self {
        Self {
            store: Arc::new(store),
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
