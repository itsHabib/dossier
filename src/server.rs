//! `MCP` server wrapping the `FsStore` as dossier verbs.
//!
//! Read side: `project.list` / `project.get` / `phase.list` / `task.list`
//! / `artifact.list`. Write side: project / phase / task verbs all
//! routed through the shared `write_lock`. Artifact writes and conflict
//! detection are not yet wired.

use std::sync::{Arc, Mutex};

use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{Artifact, Phase, PhaseStatus, Project, ProjectStatus, Task, TaskStatus};
use crate::store::{
    ClaimTask, CompleteTask, FsStore, LinkArtifact, NewPhase, NewProject, NewTask, UpdatePhase,
    UpdateProject, UpdateTask,
};

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

#[derive(Deserialize, JsonSchema)]
pub struct PhaseAddArgs {
    /// project slug
    pub project: String,
    /// phase slug — lowercase ASCII; must be unique within the project
    pub slug: String,
    /// human-readable phase title
    pub title: String,
    /// phase body / acceptance criteria (markdown)
    #[serde(default)]
    pub body: String,
    /// existing phase slug to insert after; omit to append to the end
    #[serde(default)]
    pub after_phase: Option<String>,
    /// who's adding the phase
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct PhaseUpdateArgs {
    /// project slug
    pub project: String,
    /// phase slug (addressing key)
    pub slug: String,
    /// new title; omit to leave unchanged
    #[serde(default)]
    pub title: Option<String>,
    /// new body; omit to leave unchanged
    #[serde(default)]
    pub body: Option<String>,
    /// new status; omit to leave unchanged
    /// (`pending` | `active` | `done` | `skipped`)
    #[serde(default)]
    pub status: Option<PhaseStatus>,
    /// who's making the update
    #[allow(dead_code)] // provenance, not persisted on the phase row in v0
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskCreateArgs {
    /// project slug
    pub project: String,
    /// phase slug — optional; omit for a project-wide task
    #[serde(default)]
    pub phase: Option<String>,
    /// task slug — lowercase ASCII; must be unique within the project
    pub slug: String,
    /// human-readable task title
    pub title: String,
    /// task body / acceptance criteria (markdown)
    #[serde(default)]
    pub body: String,
    /// who's creating the task
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskClaimArgs {
    /// task id (ULID with `tsk_` prefix)
    pub id: String,
    /// actor claiming the task (e.g. `ship`, `claude-code:michael`)
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskUpdateArgs {
    /// task id
    pub id: String,
    /// new body; omit to leave unchanged
    #[serde(default)]
    pub body: Option<String>,
    /// new status; omit to leave unchanged.
    /// `claimed` and `done` are rejected — use task.claim / task.complete.
    /// (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`)
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// optional note line appended to the task's `## Notes` log
    #[serde(default)]
    pub note: Option<String>,
    /// who's making the update
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TaskCompleteArgs {
    /// task id
    pub id: String,
    /// optional closing note line
    #[serde(default)]
    pub note: Option<String>,
    /// who's completing the task
    pub actor: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ArtifactLinkArgs {
    /// project slug
    pub project: String,
    /// task id to anchor this artifact to; omit for a project-wide artifact
    #[serde(default)]
    pub task: Option<String>,
    /// artifact kind (`commit` | `pr` | `file` | `url` | `run` | `doc` — extensible)
    pub kind: String,
    /// the artifact's pointer — a SHA, URL, file path, or run id
    #[serde(rename = "ref")]
    pub reference: String,
    /// short human-readable label, e.g. `"v0 spec"` or `"PR #7"`
    pub label: String,
    /// who's linking the artifact
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
        name = "phase.add",
        description = "Add a new phase to a project. Phase slug must be unique within the project. `after_phase` (a phase slug) inserts in order; default appends to the end."
    )]
    fn phase_add(
        &self,
        Parameters(args): Parameters<PhaseAddArgs>,
    ) -> Result<Json<Phase>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let phase = self
            .store
            .add_phase(NewPhase {
                project: args.project,
                slug: args.slug,
                title: args.title,
                body: args.body,
                after_phase: args.after_phase,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(phase))
    }

    #[tool(
        name = "phase.update",
        description = "Update mutable fields of a phase (title, body, status). (project, slug) is the addressing key. Preserves id, order, and created_at; bumps updated_at."
    )]
    fn phase_update(
        &self,
        Parameters(args): Parameters<PhaseUpdateArgs>,
    ) -> Result<Json<Phase>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let phase = self
            .store
            .update_phase(UpdatePhase {
                project: args.project,
                slug: args.slug,
                title: args.title,
                body: args.body,
                status: args.status,
            })
            .map_err(internal)?;
        Ok(Json(phase))
    }

    #[tool(
        name = "task.create",
        description = "Create a new task in a project. Slug must be unique within the project and lowercase ASCII. Optionally anchor to a phase by slug. Server stamps id, timestamps, and 'todo' status."
    )]
    fn task_create(
        &self,
        Parameters(args): Parameters<TaskCreateArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let task = self
            .store
            .create_task(NewTask {
                project: args.project,
                phase: args.phase,
                slug: args.slug,
                title: args.title,
                body: args.body,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.claim",
        description = "Claim a task for an actor. Sole entry into 'claimed' status. Same-actor re-claim on a non-terminal task is a no-op (no updated_at bump). Errors on terminal tasks or tasks held by a different actor."
    )]
    fn task_claim(
        &self,
        Parameters(args): Parameters<TaskClaimArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let task = self
            .store
            .claim_task(ClaimTask {
                id: args.id,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.update",
        description = "Update a task's body, status, and/or append a note to its progress log. Rejects status=claimed and status=done — use task.claim / task.complete instead. Terminal states reject all transitions."
    )]
    fn task_update(
        &self,
        Parameters(args): Parameters<TaskUpdateArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let task = self
            .store
            .update_task(UpdateTask {
                id: args.id,
                body: args.body,
                status: args.status,
                note: args.note,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.complete",
        description = "Mark a task done. Sole entry into 'done' status. Task must be in 'in_progress'; stamps completed_at and bumps updated_at. Optionally appends a closing note."
    )]
    fn task_complete(
        &self,
        Parameters(args): Parameters<TaskCompleteArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let task = self
            .store
            .complete_task(CompleteTask {
                id: args.id,
                note: args.note,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(task))
    }

    #[tool(
        name = "artifact.link",
        description = "Link an artifact (commit, PR, file, URL, run, doc, …) to a project, optionally anchored to a specific task. Append-only — entries are never rewritten. `ref` is the pointer (SHA, URL, path, run id); `label` is a short human-readable hint."
    )]
    fn artifact_link(
        &self,
        Parameters(args): Parameters<ArtifactLinkArgs>,
    ) -> Result<Json<Artifact>, ErrorData> {
        let _guard = self
            .write_lock
            .lock()
            .map_err(|e| internal(format!("write lock poisoned: {e}")))?;
        let artifact = self
            .store
            .link_artifact(LinkArtifact {
                project: args.project,
                task: args.task,
                kind: args.kind,
                reference: args.reference,
                label: args.label,
                actor: args.actor,
            })
            .map_err(internal)?;
        Ok(Json(artifact))
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
