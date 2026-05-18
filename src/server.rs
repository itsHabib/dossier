//! `MCP` server wrapping the `FsStore` as dossier verbs.
//!
//! Read side: `project.list` / `project.get` / `phase.list` / `task.list`
//! / `artifact.list`. Write side: project / phase / task verbs all
//! routed through the shared `write_lock`. Artifact writes and conflict
//! detection are not yet wired.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{
    Artifact, Phase, PhaseListFilter, PhaseOrderField, PhaseStatus, Project, ProjectListFilter,
    ProjectOrderField, ProjectStatus, SearchArgs, SearchHit, Task, TaskListFilter, TaskOrderField,
    TaskStatus,
};
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

/// Predicate-shaped arguments for `project.list`. Every field is
/// optional; an empty arg set returns every project in the corpus
/// sorted by `created_at` ASC.
#[derive(Deserialize, JsonSchema, Default)]
pub struct ProjectListArgs {
    /// if set, only projects whose status is in this list
    /// (`planning` | `active` | `paused` | `done` | `abandoned`).
    /// An empty list is treated as no filter — omit the field instead.
    #[serde(default)]
    pub status: Option<Vec<ProjectStatus>>,
    /// case-insensitive literal substring matched against the project's
    /// description body
    #[serde(default)]
    pub body_contains: Option<String>,
    /// RFC 3339 timestamp; matches rows with `created_at >= this`
    #[serde(default)]
    pub created_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `created_at < this`
    #[serde(default)]
    pub created_before: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at >= this`
    #[serde(default)]
    pub updated_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at < this`
    #[serde(default)]
    pub updated_before: Option<DateTime<Utc>>,
    /// sort key (`created_at` | `updated_at`); default `created_at`
    #[serde(default)]
    pub order_by: Option<ProjectOrderField>,
    /// reverse the sort (descending); default `false` (ascending)
    #[serde(default)]
    pub desc: Option<bool>,
    /// cap the number of returned rows
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Predicate-shaped arguments for `phase.list`. `project = None`
/// (omitted or explicit `null`) walks every project in the corpus.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PhaseListArgs {
    /// project slug; omit or pass `null` to list phases across every project
    #[serde(default)]
    pub project: Option<String>,
    /// if set, only phases whose status is in this list
    /// (`pending` | `active` | `done` | `skipped`).
    /// An empty list is treated as no filter — omit the field instead.
    #[serde(default)]
    pub status: Option<Vec<PhaseStatus>>,
    /// case-insensitive literal substring matched against the phase body
    #[serde(default)]
    pub body_contains: Option<String>,
    /// RFC 3339 timestamp; matches rows with `created_at >= this`
    #[serde(default)]
    pub created_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `created_at < this`
    #[serde(default)]
    pub created_before: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at >= this`
    #[serde(default)]
    pub updated_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at < this`
    #[serde(default)]
    pub updated_before: Option<DateTime<Utc>>,
    /// sort key (`created_at` | `updated_at` | `order`); default `order`
    /// (linear position within a project, the existing on-disk ordering)
    #[serde(default)]
    pub order_by: Option<PhaseOrderField>,
    /// reverse the sort (descending); default `false` (ascending)
    #[serde(default)]
    pub desc: Option<bool>,
    /// cap the number of returned rows
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct PhaseListResult {
    pub phases: Vec<Phase>,
}

/// Predicate-shaped arguments for `task.list`.
///
/// `project = None` (omitted or explicit `null`) walks every project in
/// the corpus. `phase` requires `project` (validation error otherwise)
/// because phase slugs are unique within a project, not across the corpus.
#[derive(Deserialize, JsonSchema, Default)]
pub struct TaskListArgs {
    /// project slug; omit or pass `null` to list tasks across every project
    #[serde(default)]
    pub project: Option<String>,
    /// phase slug; requires `project` (validation error otherwise)
    #[serde(default)]
    pub phase: Option<String>,
    /// if set, only tasks whose status is in this list
    /// (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`).
    /// An empty list is treated as no filter — omit the field instead.
    #[serde(default)]
    pub status: Option<Vec<TaskStatus>>,
    /// exact match against the task's `assignee` frontmatter field
    #[serde(default)]
    pub assignee: Option<String>,
    /// case-insensitive literal substring matched against the task spec
    /// body only — the appended `## Notes` section is not searched
    #[serde(default)]
    pub body_contains: Option<String>,
    /// RFC 3339 timestamp; matches rows with `created_at >= this`
    #[serde(default)]
    pub created_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `created_at < this`
    #[serde(default)]
    pub created_before: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at >= this`
    #[serde(default)]
    pub updated_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `updated_at < this`
    #[serde(default)]
    pub updated_before: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `completed_at >= this`
    /// (drops rows where `completed_at` is null)
    #[serde(default)]
    pub completed_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `completed_at < this`
    /// (drops rows where `completed_at` is null)
    #[serde(default)]
    pub completed_before: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `claimed_at >= this`
    /// (drops rows where `claimed_at` is null)
    #[serde(default)]
    pub claimed_after: Option<DateTime<Utc>>,
    /// RFC 3339 timestamp; matches rows with `claimed_at < this`
    /// (drops rows where `claimed_at` is null)
    #[serde(default)]
    pub claimed_before: Option<DateTime<Utc>>,
    /// sort key (`created_at` | `updated_at` | `completed_at` |
    /// `claimed_at`); default `created_at`. Sorting by a nullable field
    /// (`completed_at`, `claimed_at`) drops rows where that field is null.
    #[serde(default)]
    pub order_by: Option<TaskOrderField>,
    /// reverse the sort (descending); default `false` (ascending)
    #[serde(default)]
    pub desc: Option<bool>,
    /// cap the number of returned rows
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Serialize, JsonSchema)]
pub struct TaskListResult {
    pub tasks: Vec<Task>,
}

#[derive(Serialize, JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
}

impl From<ProjectListArgs> for ProjectListFilter {
    fn from(a: ProjectListArgs) -> Self {
        Self {
            status: a.status,
            body_contains: a.body_contains,
            created_after: a.created_after,
            created_before: a.created_before,
            updated_after: a.updated_after,
            updated_before: a.updated_before,
            order_by: a.order_by,
            desc: a.desc,
            limit: a.limit,
        }
    }
}

impl From<PhaseListArgs> for PhaseListFilter {
    fn from(a: PhaseListArgs) -> Self {
        Self {
            project: a.project,
            status: a.status,
            body_contains: a.body_contains,
            created_after: a.created_after,
            created_before: a.created_before,
            updated_after: a.updated_after,
            updated_before: a.updated_before,
            order_by: a.order_by,
            desc: a.desc,
            limit: a.limit,
        }
    }
}

impl From<TaskListArgs> for TaskListFilter {
    fn from(a: TaskListArgs) -> Self {
        Self {
            project: a.project,
            phase: a.phase,
            status: a.status,
            assignee: a.assignee,
            body_contains: a.body_contains,
            created_after: a.created_after,
            created_before: a.created_before,
            updated_after: a.updated_after,
            updated_before: a.updated_before,
            completed_after: a.completed_after,
            completed_before: a.completed_before,
            claimed_after: a.claimed_after,
            claimed_before: a.claimed_before,
            order_by: a.order_by,
            desc: a.desc,
            limit: a.limit,
        }
    }
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
        description = "List projects subject to a predicate filter. Every argument is optional; an empty arg set returns every project in the corpus, sorted by `created_at` ASC. Filters AND-together.\n\nFilters: `status` is a list of statuses (`planning` | `active` | `paused` | `done` | `abandoned`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the project's description body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` (default `created_at`); `desc: true` reverses (default ascending). `limit` caps the rows.\n\nReturns metadata only — call `project.get` for the full description body."
    )]
    fn project_list(
        &self,
        Parameters(args): Parameters<ProjectListArgs>,
    ) -> Result<Json<ProjectListResult>, ErrorData> {
        let projects = self
            .store
            .list_projects(&ProjectListFilter::from(args))
            .map_err(internal)?;
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
        let phase_filter = PhaseListFilter {
            project: Some(args.slug.clone()),
            ..Default::default()
        };
        let task_filter = TaskListFilter {
            project: Some(args.slug.clone()),
            ..Default::default()
        };
        let phases = self.store.list_phases(&phase_filter).map_err(internal)?;
        let tasks = self.store.list_tasks(&task_filter).map_err(internal)?;
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
        description = "List phases subject to a predicate filter. Phase bodies are included.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. A cross-project listing groups by project, then by `order` within each project, so the linear-position ordering stays meaningful.\n\nFilters: `status` is a list (`pending` | `active` | `done` | `skipped`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the phase body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `order` (default `order` — the linear-position frontmatter field). `desc: true` reverses (default ascending). `limit` caps the rows. Filters AND-together."
    )]
    fn phase_list(
        &self,
        Parameters(args): Parameters<PhaseListArgs>,
    ) -> Result<Json<PhaseListResult>, ErrorData> {
        let filter = PhaseListFilter::from(args);
        let phases = self.store.list_phases(&filter).map_err(internal)?;
        Ok(Json(PhaseListResult { phases }))
    }

    #[tool(
        name = "task.list",
        description = "List tasks subject to a predicate filter. Every argument is optional; an empty arg set returns every task in the corpus, sorted by `created_at` ASC. Filters AND-together.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. `phase` is a phase slug and REQUIRES `project` (validation error otherwise — phase slugs are unique per project, not globally).\n\nFilters: `status` is a list (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`) — OR-of-statuses. `assignee` is an exact match against the task's `assignee` frontmatter (e.g. `human:michael`, `ship`). `body_contains` is a case-insensitive literal substring against the task body. The four date-range pairs — `created`, `updated`, `completed`, `claimed` — each take `_after` (inclusive, >=) and `_before` (exclusive, <) RFC 3339 timestamps. Filtering on `completed_*` or `claimed_*` drops rows where that timestamp is null. Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `completed_at` | `claimed_at` (default `created_at`); sorting by a nullable field (`completed_at`, `claimed_at`) drops rows where that field is null. `desc: true` reverses (default ascending). `limit` caps the rows."
    )]
    fn task_list(
        &self,
        Parameters(args): Parameters<TaskListArgs>,
    ) -> Result<Json<TaskListResult>, ErrorData> {
        if args.phase.is_some() && args.project.is_none() {
            return Err(ErrorData::invalid_params(
                "phase requires project (phase slugs are unique per project, not across the corpus)",
                None,
            ));
        }
        let filter = TaskListFilter::from(args);
        let tasks = self.store.list_tasks(&filter).map_err(internal)?;
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

    #[tool(
        name = "search",
        description = "Unified case-insensitive literal substring search across project titles + description bodies, phase titles + bodies, and task titles + spec bodies (not notes, assignee, or other frontmatter). One call returns a single ranked list so the model can pick rows to open — use this instead of three `body_contains` list round-trips when you don't already know the primitive kind. Each hit includes `score` overlapping literal match count in title+newline+body (higher is stronger), `snippet` (~80 characters centered on the first match, no markdown awareness), and rows are ordered by `score` descending then `updated_at` descending; `limit` (default 50) applies after sorting. `kinds` filters to one or more of `project` | `phase` | `task` (default: all). `project` restricts to one project slug; omit or null for the whole corpus. Empty `query` is rejected; no matches returns an empty list. Prefer list verbs with `body_contains` when you already know you're only looking for tasks (or phases, projects)."
    )]
    fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        if args.query.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "query must be a non-empty literal substring",
                None,
            ));
        }
        let hits = self.store.search(&args).map_err(internal)?;
        Ok(Json(SearchResult { hits }))
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

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;
    use crate::domain::SearchArgs;

    fn fresh_service() -> (tempfile::TempDir, MeshService) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let store = FsStore::open(tmp.path()).expect("open fresh corpus");
        let service = MeshService::new(store);
        (tmp, service)
    }

    #[test]
    fn task_list_rejects_phase_without_project() {
        let (_tmp, svc) = fresh_service();
        let args = TaskListArgs {
            phase: Some("spec".to_owned()),
            ..Default::default()
        };
        match svc.task_list(Parameters(args)) {
            Err(err) => assert!(
                err.message.contains("phase requires project"),
                "unexpected error message: {}",
                err.message
            ),
            Ok(_) => panic!("phase without project must be rejected"),
        }
    }

    #[test]
    fn task_list_accepts_phase_with_project() {
        // Confirms the server-layer "phase requires project" guard passes
        // the call through to the store when both are present. The store
        // surfaces an unresolvable phase slug as a typed error (rather
        // than silently returning every task), which is what we expect
        // here — the project doesn't exist, so neither does the phase.
        let (_tmp, svc) = fresh_service();
        let args = TaskListArgs {
            project: Some("nonexistent".to_owned()),
            phase: Some("spec".to_owned()),
            ..Default::default()
        };
        match svc.task_list(Parameters(args)) {
            Err(err) => {
                assert!(
                    !err.message.contains("phase requires project"),
                    "server-layer validation should not reject phase+project; got: {}",
                    err.message
                );
                assert!(
                    err.message.contains("phase not found"),
                    "store should report the unresolvable slug; got: {}",
                    err.message
                );
            }
            Ok(Json(out)) => panic!(
                "expected typed error for unresolvable slug, got {} tasks",
                out.tasks.len()
            ),
        }
    }

    #[test]
    fn task_list_rejects_malformed_date() {
        // Malformed RFC 3339 strings fail at serde deserialization, which
        // is the spec's documented "typed validation error rather than
        // silent coercion" behavior. We don't pin a particular chrono
        // error message — only that the input is rejected, since the
        // message wording is upstream's choice and may change.
        let raw = r#"{
            "project": "alpha",
            "created_after": "not-a-real-date"
        }"#;
        assert!(
            serde_json::from_str::<TaskListArgs>(raw).is_err(),
            "malformed date must be rejected at deserialization"
        );

        // A well-formed RFC 3339 timestamp must deserialize cleanly.
        let ok_raw = r#"{
            "project": "alpha",
            "created_after": "2026-05-01T00:00:00Z"
        }"#;
        let args: TaskListArgs = match serde_json::from_str(ok_raw) {
            Ok(a) => a,
            Err(e) => panic!("well-formed timestamp must deserialize: {e}"),
        };
        assert!(args.created_after.is_some());
    }

    #[test]
    fn task_list_rejects_unknown_order_by() {
        let raw = r#"{
            "project": "alpha",
            "order_by": "title"
        }"#;
        let Err(err) = serde_json::from_str::<TaskListArgs>(raw) else {
            panic!("unknown order_by must be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("order_by") || msg.contains("variant"),
            "expected mention of order_by or variant; got: {msg}"
        );
    }

    #[test]
    fn phase_list_rejects_unknown_order_by() {
        let raw = r#"{
            "project": "alpha",
            "order_by": "title"
        }"#;
        let Err(err) = serde_json::from_str::<PhaseListArgs>(raw) else {
            panic!("unknown order_by must be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("order_by") || msg.contains("variant"),
            "expected mention of order_by or variant; got: {msg}"
        );
    }

    #[test]
    fn project_list_rejects_unknown_order_by() {
        let raw = r#"{
            "order_by": "completed_at"
        }"#;
        let Err(err) = serde_json::from_str::<ProjectListArgs>(raw) else {
            panic!("project.list has no completed_at order_by");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("order_by") || msg.contains("variant"),
            "expected mention of order_by or variant; got: {msg}"
        );
    }

    #[test]
    fn task_list_status_accepts_multiple_values() {
        let raw = r#"{
            "project": "alpha",
            "status": ["claimed", "in_progress"]
        }"#;
        let args: TaskListArgs = match serde_json::from_str(raw) {
            Ok(a) => a,
            Err(e) => panic!("multi-status must deserialize: {e}"),
        };
        let statuses = args.status.unwrap_or_default();
        assert_eq!(statuses.len(), 2);
        assert!(statuses.contains(&TaskStatus::Claimed));
        assert!(statuses.contains(&TaskStatus::InProgress));
    }

    #[test]
    fn task_list_accepts_explicit_null_project() {
        let raw = r#"{
            "project": null,
            "status": ["in_progress"]
        }"#;
        let args: TaskListArgs = match serde_json::from_str(raw) {
            Ok(a) => a,
            Err(e) => panic!("explicit null project must deserialize: {e}"),
        };
        assert!(args.project.is_none());
    }

    #[test]
    fn search_rejects_whitespace_only_query() {
        let (_tmp, svc) = fresh_service();
        let args = SearchArgs {
            query: "   ".to_owned(),
            ..Default::default()
        };
        match svc.search(Parameters(args)) {
            Err(e) => assert!(
                e.message.to_lowercase().contains("query") || e.message.contains("non-empty"),
                "{}",
                e.message
            ),
            Ok(_) => panic!("whitespace-only query must be rejected"),
        }
    }

    #[test]
    fn search_args_rejects_bad_kind() {
        let raw = r#"{"query":"x", "kinds": ["wat"]}"#;
        assert!(
            serde_json::from_str::<SearchArgs>(raw).is_err(),
            "unknown kind must fail deserialization"
        );
    }

    #[test]
    fn project_list_accepts_empty_args() {
        // Smoke: a completely empty arg set is a valid call (list everything).
        let (_tmp, svc) = fresh_service();
        let out = match svc.project_list(Parameters(ProjectListArgs::default())) {
            Ok(Json(out)) => out,
            Err(err) => panic!("empty args must succeed: {}", err.message),
        };
        assert!(out.projects.is_empty());
    }

    #[test]
    fn project_update_args_deserialize_without_actor() {
        let raw = r#"{"slug": "alpha", "title": "Renamed"}"#;
        let args: ProjectUpdateArgs = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(args.slug, "alpha");
        assert_eq!(args.title.as_deref(), Some("Renamed"));
        assert!(args.description.is_none());
        assert!(args.status.is_none());
    }

    #[test]
    fn phase_update_args_deserialize_without_actor() {
        let raw = r#"{"project": "alpha", "slug": "spec", "body": "done"}"#;
        let args: PhaseUpdateArgs = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(args.project, "alpha");
        assert_eq!(args.slug, "spec");
        assert_eq!(args.body.as_deref(), Some("done"));
        assert!(args.title.is_none());
        assert!(args.status.is_none());
    }

    #[test]
    fn project_update_args_ignores_actor_from_old_clients() {
        let raw = r#"{"slug": "alpha", "actor": "human:legacy"}"#;
        let args: ProjectUpdateArgs = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(args.slug, "alpha");
        assert!(args.title.is_none());
        assert!(args.description.is_none());
        assert!(args.status.is_none());
    }

    #[test]
    fn phase_update_args_ignores_actor_from_old_clients() {
        let raw = r#"{"project": "alpha", "slug": "spec", "actor": "human:legacy"}"#;
        let args: PhaseUpdateArgs = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(args.project, "alpha");
        assert_eq!(args.slug, "spec");
        assert!(args.title.is_none());
        assert!(args.body.is_none());
        assert!(args.status.is_none());
    }

    #[test]
    fn project_update_and_phase_update_succeed_without_actor() {
        let (_tmp, svc) = fresh_service();

        svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("project.create");

        svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "spec".to_owned(),
            title: "Spec".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
        }))
        .expect("phase.add");

        let Json(project) = svc
            .project_update(Parameters(ProjectUpdateArgs {
                slug: "alpha".to_owned(),
                title: Some("Alpha2".to_owned()),
                description: None,
                status: Some(ProjectStatus::Active),
            }))
            .expect("project.update");
        assert_eq!(project.title, "Alpha2");
        assert_eq!(project.status, ProjectStatus::Active);

        let Json(phase) = svc
            .phase_update(Parameters(PhaseUpdateArgs {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: Some("Spec2".to_owned()),
                body: None,
                status: Some(PhaseStatus::Done),
            }))
            .expect("phase.update");
        assert_eq!(phase.title, "Spec2");
        assert_eq!(phase.status, PhaseStatus::Done);
    }

    #[test]
    fn project_get_scopes_phases_and_tasks_to_requested_project() {
        let (_tmp, svc) = fresh_service();

        svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("create alpha");

        svc.project_create(Parameters(ProjectCreateArgs {
            slug: "beta".to_owned(),
            title: "Beta".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("create beta");

        svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "alpha-phase".to_owned(),
            title: "Alpha phase".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
        }))
        .expect("alpha phase");

        svc.phase_add(Parameters(PhaseAddArgs {
            project: "beta".to_owned(),
            slug: "beta-phase".to_owned(),
            title: "Beta phase".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
        }))
        .expect("beta phase");

        svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "alpha-task".to_owned(),
            title: "Alpha task".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("alpha task");

        svc.task_create(Parameters(TaskCreateArgs {
            project: "beta".to_owned(),
            phase: None,
            slug: "beta-task".to_owned(),
            title: "Beta task".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("beta task");

        let Json(view) = svc
            .project_get(Parameters(ProjectGetArgs {
                slug: "alpha".to_owned(),
            }))
            .expect("project.get alpha");

        assert_eq!(view.phases.len(), 1, "scoped to alpha project");
        assert_eq!(view.phases[0].slug, "alpha-phase");

        assert_eq!(view.tasks.len(), 1, "scoped to alpha project");
        assert_eq!(view.tasks[0].slug, "alpha-task");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn artifact_list_filters_combine_task_and_kind_predicates() {
        let (_tmp, svc) = fresh_service();

        svc.project_create(Parameters(ProjectCreateArgs {
            slug: "corp".to_owned(),
            title: "Corp".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("create project");

        let Json(t1) = svc
            .task_create(Parameters(TaskCreateArgs {
                project: "corp".to_owned(),
                phase: None,
                slug: "t1".to_owned(),
                title: "T1".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
            }))
            .expect("t1");

        let Json(t2) = svc
            .task_create(Parameters(TaskCreateArgs {
                project: "corp".to_owned(),
                phase: None,
                slug: "t2".to_owned(),
                title: "T2".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
            }))
            .expect("t2");

        let tid1 = t1.id;
        let tid2 = t2.id;

        let Json(Artifact { id: id_a1, .. }) = svc
            .artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid1.clone()),
                kind: "pr".to_owned(),
                reference: "https://example/pr/1".to_owned(),
                label: "art1".to_owned(),
                actor: "human:test".to_owned(),
            }))
            .expect("art1");

        let Json(Artifact { id: id_a2, .. }) = svc
            .artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid2),
                kind: "pr".to_owned(),
                reference: "https://example/pr/2".to_owned(),
                label: "art2".to_owned(),
                actor: "human:test".to_owned(),
            }))
            .expect("art2");

        let Json(Artifact { id: id_a3, .. }) = svc
            .artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid1.clone()),
                kind: "commit".to_owned(),
                reference: "deadbeef".to_owned(),
                label: "art3".to_owned(),
                actor: "human:test".to_owned(),
            }))
            .expect("art3");

        macro_rules! ids {
            ($args:expr) => {{
                let Json(out) = svc.artifact_list(Parameters($args)).expect("artifact_list");
                out.artifacts.into_iter().map(|a| a.id).collect::<Vec<_>>()
            }};
        }

        let mut all = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: String::new(),
        });
        all.sort_unstable();
        assert_eq!(all.len(), 3);

        let mut hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: tid1.clone(),
            kind: String::new(),
        });
        hit.sort_unstable();
        assert_eq!(
            hit,
            {
                let mut v = vec![id_a1.clone(), id_a3.clone()];
                v.sort_unstable();
                v
            },
            "task filter narrows correctly"
        );

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: "pr".to_owned(),
        });
        hit.sort_unstable();
        assert_eq!(hit, {
            let mut v = vec![id_a1, id_a2];
            v.sort_unstable();
            v
        },);

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: tid1,
            kind: "commit".to_owned(),
        });
        assert_eq!(hit, vec![id_a3]);

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: "tsk_does_not_exist".to_owned(),
            kind: String::new(),
        });
        assert!(hit.is_empty());
    }

    #[test]
    fn project_list_filter_round_trips_through_from_impl() {
        let created_after = DateTime::parse_from_rfc3339("2026-01-01T08:15:30Z")
            .unwrap()
            .with_timezone(&Utc);
        let created_before = DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_after = DateTime::parse_from_rfc3339("2026-02-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_before = DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let f = ProjectListFilter::from(ProjectListArgs {
            status: Some(vec![ProjectStatus::Active]),
            body_contains: Some("needle".to_owned()),
            created_after: Some(created_after),
            created_before: Some(created_before),
            updated_after: Some(updated_after),
            updated_before: Some(updated_before),
            order_by: Some(ProjectOrderField::UpdatedAt),
            desc: Some(true),
            limit: Some(7),
        });

        assert_eq!(f.status, Some(vec![ProjectStatus::Active]));
        assert_eq!(f.body_contains.as_deref(), Some("needle"));
        assert_eq!(f.created_after, Some(created_after));
        assert_eq!(f.created_before, Some(created_before));
        assert_eq!(f.updated_after, Some(updated_after));
        assert_eq!(f.updated_before, Some(updated_before));
        assert_eq!(f.order_by, Some(ProjectOrderField::UpdatedAt));
        assert_eq!(f.desc, Some(true));
        assert_eq!(f.limit, Some(7));
    }

    #[test]
    fn phase_list_filter_round_trips_through_from_impl() {
        let created_after = DateTime::parse_from_rfc3339("2026-03-03T03:03:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let created_before = DateTime::parse_from_rfc3339("2026-03-04T03:03:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_after = DateTime::parse_from_rfc3339("2026-03-05T03:03:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_before = DateTime::parse_from_rfc3339("2026-03-06T03:03:03Z")
            .unwrap()
            .with_timezone(&Utc);

        let f = PhaseListFilter::from(PhaseListArgs {
            project: Some("omega".to_owned()),
            status: Some(vec![PhaseStatus::Skipped]),
            body_contains: Some("phase-body".to_owned()),
            created_after: Some(created_after),
            created_before: Some(created_before),
            updated_after: Some(updated_after),
            updated_before: Some(updated_before),
            order_by: Some(PhaseOrderField::CreatedAt),
            desc: Some(true),
            limit: Some(42),
        });

        assert_eq!(f.project.as_deref(), Some("omega"));
        assert_eq!(f.status, Some(vec![PhaseStatus::Skipped]));
        assert_eq!(f.body_contains.as_deref(), Some("phase-body"));
        assert_eq!(f.created_after, Some(created_after));
        assert_eq!(f.created_before, Some(created_before));
        assert_eq!(f.updated_after, Some(updated_after));
        assert_eq!(f.updated_before, Some(updated_before));
        assert_eq!(f.order_by, Some(PhaseOrderField::CreatedAt));
        assert_eq!(f.desc, Some(true));
        assert_eq!(f.limit, Some(42));
    }

    #[test]
    fn task_list_filter_round_trips_through_from_impl() {
        let created_after = DateTime::parse_from_rfc3339("2026-04-04T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let created_before = DateTime::parse_from_rfc3339("2026-04-05T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_after = DateTime::parse_from_rfc3339("2026-04-06T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let updated_before = DateTime::parse_from_rfc3339("2026-04-07T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let completed_after = DateTime::parse_from_rfc3339("2026-04-08T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let completed_before = DateTime::parse_from_rfc3339("2026-04-09T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let claimed_after = DateTime::parse_from_rfc3339("2026-04-10T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);
        let claimed_before = DateTime::parse_from_rfc3339("2026-04-11T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);

        let f = TaskListFilter::from(TaskListArgs {
            project: Some("rho".to_owned()),
            phase: Some("implement".to_owned()),
            status: Some(vec![TaskStatus::Todo, TaskStatus::Blocked]),
            assignee: Some("ship".to_owned()),
            body_contains: Some("fixture".to_owned()),
            created_after: Some(created_after),
            created_before: Some(created_before),
            updated_after: Some(updated_after),
            updated_before: Some(updated_before),
            completed_after: Some(completed_after),
            completed_before: Some(completed_before),
            claimed_after: Some(claimed_after),
            claimed_before: Some(claimed_before),
            order_by: Some(TaskOrderField::ClaimedAt),
            desc: Some(true),
            limit: Some(99),
        });

        assert_eq!(f.project.as_deref(), Some("rho"));
        assert_eq!(f.phase.as_deref(), Some("implement"));
        assert_eq!(f.status, Some(vec![TaskStatus::Todo, TaskStatus::Blocked]));
        assert_eq!(f.assignee.as_deref(), Some("ship"));
        assert_eq!(f.body_contains.as_deref(), Some("fixture"));
        assert_eq!(f.created_after, Some(created_after));
        assert_eq!(f.created_before, Some(created_before));
        assert_eq!(f.updated_after, Some(updated_after));
        assert_eq!(f.updated_before, Some(updated_before));
        assert_eq!(f.completed_after, Some(completed_after));
        assert_eq!(f.completed_before, Some(completed_before));
        assert_eq!(f.claimed_after, Some(claimed_after));
        assert_eq!(f.claimed_before, Some(claimed_before));
        assert_eq!(f.order_by, Some(TaskOrderField::ClaimedAt));
        assert_eq!(f.desc, Some(true));
        assert_eq!(f.limit, Some(99));
    }
}
