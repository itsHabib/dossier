//! `MCP` server wrapping a [`Store`] backend as dossier verbs.
//!
//! Read side: `project.list` / `project.get` / `phase.list` / `task.list`
//! / `task.get` / `artifact.list`. Write side: all verbs route through
//! [`MeshService`] policy over `Arc<dyn Store>`; task verbs use optimistic
//! CAS loops on the store primitives.

use std::sync::Arc;

use anyhow::Error as AnyhowError;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use ulid::Ulid;

use crate::domain::{
    Artifact, Phase, PhaseListFilter, Project, ProjectListFilter, ProjectOverview, SearchArgs,
    Task, TaskGetArgs, TaskListFilter,
};
use crate::store::{
    ArtifactListFilter, ClaimTask, CompleteTask, FsStore, LinkArtifact, NewPhase, NewProject,
    NewTask, Store, StoreError, UpdatePhase, UpdateProject, UpdateTask,
};

pub mod artifact;
pub mod phase;
pub mod project;
pub mod search;
pub mod task;
#[cfg(test)]
pub mod testutil;

use artifact::{ArtifactLinkArgs, ArtifactListArgs, ArtifactListResult};
use phase::{PhaseAddArgs, PhaseListArgs, PhaseListResult, PhaseUpdateArgs};
use project::{
    build_overview, ProjectCreateArgs, ProjectGetArgs, ProjectListArgs, ProjectListResult,
    ProjectOverviewArgs, ProjectUpdateArgs, ProjectView,
};
use search::SearchResult;
use task::{
    TaskClaimArgs, TaskCompleteArgs, TaskCreateArgs, TaskListArgs, TaskListResult, TaskUpdateArgs,
};

/// Implementation version of the dossier mesh. Distinct from the
/// protocol version (which lives in PROTOCOL.md, currently v0).
pub const VERSION: &str = "0.1.0";

/// Service holding a shared [`Store`] backend and a process-local write lock.
///
/// The `Arc<dyn Store>` lets `rmcp` clone the service cheaply across
/// handler invocations. The async `tokio::sync::Mutex<()>` serializes the
/// check-then-write verbs that have no store-level CAS guard (project
/// create/update, phase add/update, artifact.link); the task verbs skip it
/// and rely on the store's CAS loops instead. The guard is held across the
/// awaited store op, so the handlers stay `async` — no nested runtime (a
/// sync `block_on` here panics under the real stdio transport).
#[derive(Clone)]
pub struct MeshService {
    store: Arc<dyn Store>,
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl MeshService {
    /// Wrap a shared [`Store`] backend with a process-local write lock.
    pub fn from_store(store: Arc<dyn Store>) -> Self {
        Self {
            store,
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Wrap an opened [`FsStore`] with shared handles and a process-local write lock.
    pub fn new(store: FsStore) -> Self {
        Self::from_store(Arc::new(store))
    }
}

#[tool_router(server_handler)]
impl MeshService {
    #[tool(
        name = "project.list",
        description = "List projects subject to a predicate filter. Filters AND-together.\n\nLIVE BY DEFAULT: with no `status` filter, returns only live (non-terminal) projects — `done` and `abandoned` are hidden. Pass `include_terminal: true` to get every status (the old default), or an explicit `status` (incl. terminal values like `done`) which always wins verbatim and ignores `include_terminal`. Default sort is `created_at` ASC.\n\nFilters: `status` is a list of statuses (`planning` | `active` | `paused` | `done` | `abandoned`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the project's description body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` (default `created_at`); `desc: true` reverses (default ascending). `limit` caps the rows.\n\nReturns metadata only — call `project.get` for the full description body (corpus-wide; for one project's state, use project.overview)."
    )]
    async fn project_list(
        &self,
        Parameters(args): Parameters<ProjectListArgs>,
    ) -> Result<Json<ProjectListResult>, ErrorData> {
        let projects = self
            .store
            .list_projects(ProjectListFilter::from(args))
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        Ok(Json(ProjectListResult { projects }))
    }

    #[tool(
        name = "project.create",
        description = "Create a new project. Slug must be unique within the corpus and lowercase ASCII. Server stamps id, created_at, updated_at, and an initial 'planning' status."
    )]
    async fn project_create(
        &self,
        Parameters(args): Parameters<ProjectCreateArgs>,
    ) -> Result<Json<Project>, ErrorData> {
        let _guard = self.write_lock.lock().await;
        let project = self
            .create_project(NewProject {
                slug: args.slug,
                title: args.title,
                description: args.description,
                actor: args.actor,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(project))
    }

    #[tool(
        name = "project.update",
        description = "Update mutable fields of a project (title, description, status). Slug is the addressing key and is immutable. Preserves id and created_at; bumps updated_at."
    )]
    async fn project_update(
        &self,
        Parameters(args): Parameters<ProjectUpdateArgs>,
    ) -> Result<Json<Project>, ErrorData> {
        if matches!(args.actor.as_deref(), Some("")) {
            return Err(ErrorData::invalid_params("actor must not be empty", None));
        }
        let _guard = self.write_lock.lock().await;
        let project = self
            .update_project(UpdateProject {
                slug: args.slug,
                title: args.title,
                description: args.description,
                status: args.status,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(project))
    }

    #[tool(
        name = "project.get",
        description = "Full hydrate: one project with every phase, task, and artifact body inline. Heavy on mature projects — can exceed the result size cap. To orient, call project.overview; to read a specific part, use phase.list / task.get / task.list."
    )]
    async fn project_get(
        &self,
        Parameters(args): Parameters<ProjectGetArgs>,
    ) -> Result<Json<ProjectView>, ErrorData> {
        let project = match self.store.get_project(&args.slug).await {
            Err(StoreError::NotFound) => {
                return Err(ErrorData::invalid_params(
                    format!("project not found: {}", args.slug),
                    None,
                ));
            }
            Err(err) => return Err(store_err(err)),
            Ok(versioned) => versioned.value,
        };
        let phase_filter = PhaseListFilter {
            project: Some(args.slug.clone()),
            ..Default::default()
        };
        let task_filter = TaskListFilter {
            project: Some(args.slug.clone()),
            ..Default::default()
        };
        let phases = self
            .store
            .list_phases(phase_filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        let tasks = self
            .store
            .list_tasks(task_filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        let artifacts = self
            .store
            .list_artifacts(ArtifactListFilter {
                project: args.slug.clone(),
            })
            .await
            .map_err(store_err)?;
        Ok(Json(ProjectView {
            project,
            phases,
            tasks,
            artifacts,
        }))
    }

    #[tool(
        name = "project.overview",
        description = "Orient in a project — the bounded \"what's the state of `<project>`?\" read, and the one to call FIRST. Returns project meta + a (truncated) description + an ordered phase index where each phase carries task-status COUNTS (todo|claimed|in_progress|blocked|done|cancelled, plus total) instead of bodies, an `unphased` bucket for tasks not anchored to a live phase (empty *or* dangling phase id), and project-level rollups (phase + task counts, artifact count). Stays a few KB no matter how much work has accumulated. To read a specific design doc or task body, follow up with phase.list / task.get / task.list. project.get is the full unbounded hydrate — heavy on mature projects."
    )]
    async fn project_overview(
        &self,
        Parameters(args): Parameters<ProjectOverviewArgs>,
    ) -> Result<Json<ProjectOverview>, ErrorData> {
        let project = match self.store.get_project(&args.slug).await {
            Err(StoreError::NotFound) => {
                return Err(ErrorData::invalid_params(
                    format!("project not found: {}", args.slug),
                    None,
                ));
            }
            Err(err) => return Err(store_err(err)),
            Ok(versioned) => versioned.value,
        };
        let phases: Vec<Phase> = self
            .store
            .list_phases(PhaseListFilter {
                project: Some(args.slug.clone()),
                ..Default::default()
            })
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        let tasks: Vec<Task> = self
            .store
            .list_tasks(TaskListFilter {
                project: Some(args.slug.clone()),
                ..Default::default()
            })
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        let artifact_count = self
            .store
            .list_artifacts(ArtifactListFilter {
                project: args.slug.clone(),
            })
            .await
            .map_err(store_err)?
            .len();
        Ok(Json(build_overview(
            &project,
            phases,
            &tasks,
            artifact_count,
        )))
    }

    #[tool(
        name = "phase.add",
        description = "Add a new phase to a project. Phase slug must be unique within the project. `owner` is required (current responsible party, actor-string shape — `human:<name>` / `team:<slug>` / `agent:<name>`; distinct from `created_by`, which records the origin actor immutably). `after_phase` (a phase slug) inserts in order; default appends to the end."
    )]
    async fn phase_add(
        &self,
        Parameters(args): Parameters<PhaseAddArgs>,
    ) -> Result<Json<Phase>, ErrorData> {
        let _guard = self.write_lock.lock().await;
        let phase = self
            .add_phase(&NewPhase {
                project: args.project,
                slug: args.slug,
                title: args.title,
                body: args.body,
                after_phase: args.after_phase,
                actor: args.actor,
                owner: args.owner,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(phase))
    }

    #[tool(
        name = "phase.update",
        description = "Update mutable fields of a phase (title, body, status, owner). (project, slug) is the addressing key. `owner` replaces the current value when `Some` (rejects an empty string); omit to leave unchanged. Preserves id, order, and created_at; bumps updated_at."
    )]
    async fn phase_update(
        &self,
        Parameters(args): Parameters<PhaseUpdateArgs>,
    ) -> Result<Json<Phase>, ErrorData> {
        let _guard = self.write_lock.lock().await;
        let phase = self
            .update_phase(UpdatePhase {
                project: args.project,
                slug: args.slug,
                title: args.title,
                body: args.body,
                status: args.status,
                owner: args.owner,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(phase))
    }

    #[tool(
        name = "task.create",
        description = "Create a new task in a project. Slug must be unique within the project and lowercase ASCII. Optionally anchor to a phase by slug. Server stamps id, timestamps, and 'todo' status."
    )]
    async fn task_create(
        &self,
        Parameters(args): Parameters<TaskCreateArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let task = self
            .create_task(NewTask {
                project: args.project,
                phase: args.phase,
                slug: args.slug,
                title: args.title,
                body: args.body,
                actor: args.actor,
                depends_on: args.depends_on,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.claim",
        description = "Claim a task for an actor. Sole entry into 'claimed' status. Same-actor re-claim on a non-terminal task is a no-op (no updated_at bump). Errors on terminal tasks or tasks held by a different actor."
    )]
    async fn task_claim(
        &self,
        Parameters(args): Parameters<TaskClaimArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let task = self
            .claim_task(&ClaimTask {
                id: args.id,
                actor: args.actor,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.update",
        description = "Update a task's body, status, and/or append a note to its progress log. Rejects status=claimed and status=done — use task.claim / task.complete instead. Terminal states reject all transitions."
    )]
    async fn task_update(
        &self,
        Parameters(args): Parameters<TaskUpdateArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let task = self
            .update_task(UpdateTask {
                id: args.id,
                body: args.body,
                status: args.status,
                note: args.note,
                actor: args.actor,
                depends_on: args.depends_on,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.complete",
        description = "Mark a task done. Sole entry into 'done' status. From 'in_progress' completes directly; from 'todo' or 'claimed' (same actor) implicitly claims and advances through in_progress first. Cross-actor 'claimed' rejects. Stamps completed_at and bumps updated_at. Optionally appends a closing note."
    )]
    async fn task_complete(
        &self,
        Parameters(args): Parameters<TaskCompleteArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        let task = self
            .complete_task(CompleteTask {
                id: args.id,
                note: args.note,
                actor: args.actor,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(task))
    }

    #[tool(
        name = "task.get",
        description = "Fetch a single task by id (tsk_ + ULID). Walks the whole corpus — no project slug required. Rejects malformed ids before scanning; returns not-found when the id is well-formed but absent."
    )]
    async fn task_get(
        &self,
        Parameters(args): Parameters<TaskGetArgs>,
    ) -> Result<Json<Task>, ErrorData> {
        if !is_well_formed_task_id(&args.id) {
            return Err(ErrorData::invalid_params("invalid id format", None));
        }
        let id = &args.id;
        let task = match self.store.get_task(id).await {
            Ok(versioned) => versioned.value,
            Err(StoreError::NotFound) => {
                return Err(ErrorData::invalid_params(
                    format!("task not found: {id}"),
                    None,
                ));
            }
            Err(err) => return Err(store_err(err)),
        };
        Ok(Json(task))
    }

    #[tool(
        name = "artifact.link",
        description = "Link an artifact (commit, PR, file, URL, run, doc, …) to a project, optionally anchored to a specific task. Append-only — entries are never rewritten. `ref` is the pointer (SHA, URL, path, run id); `label` is a short human-readable hint."
    )]
    async fn artifact_link(
        &self,
        Parameters(args): Parameters<ArtifactLinkArgs>,
    ) -> Result<Json<Artifact>, ErrorData> {
        let _guard = self.write_lock.lock().await;
        let artifact = self
            .link_artifact(LinkArtifact {
                project: args.project,
                task: args.task,
                kind: args.kind,
                reference: args.reference,
                label: args.label,
                actor: args.actor,
            })
            .await
            .map_err(store_err_to_invalid)?;
        Ok(Json(artifact))
    }

    #[tool(
        name = "phase.list",
        description = "List phases subject to a predicate filter. Phase bodies are included.\n\nLIVE BY DEFAULT: with no `status` filter, returns only live (non-terminal) phases — `done` and `skipped` are hidden. Pass `include_terminal: true` to get every status (the old default), or an explicit `status` (incl. terminal values) which always wins verbatim and ignores `include_terminal`.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. A cross-project listing groups by project, then by `order` within each project, so the linear-position ordering stays meaningful.\n\nFilters: `status` is a list (`pending` | `active` | `done` | `skipped`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the phase body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `order` (default `order` — the linear-position frontmatter field). `desc: true` reverses (default ascending). `limit` caps the rows. Filters AND-together. Pass bodies:false to strip the phase body markdown; all frontmatter (id, slug, title, status, order, owner, timestamps) is still returned."
    )]
    async fn phase_list(
        &self,
        Parameters(args): Parameters<PhaseListArgs>,
    ) -> Result<Json<PhaseListResult>, ErrorData> {
        let with_bodies = args.bodies.unwrap_or(true);
        let filter = PhaseListFilter::from(args);
        let mut phases: Vec<Phase> = self
            .store
            .list_phases(filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        if !with_bodies {
            for p in &mut phases {
                p.body.clear();
            }
        }
        Ok(Json(PhaseListResult { phases }))
    }

    #[tool(
        name = "task.list",
        description = "List tasks subject to a predicate filter. Filters AND-together.\n\nLIVE BY DEFAULT: with no `status` filter, returns only live (non-terminal) tasks — `done` and `cancelled` are hidden. Pass `include_terminal: true` to get every status (the old default), or an explicit `status` (incl. terminal values like `done`) which always wins verbatim and ignores `include_terminal`. Default sort is `created_at` ASC.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. `phase` is a phase slug and REQUIRES `project` (validation error otherwise — phase slugs are unique per project, not globally).\n\nFilters: `status` is a list (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`) — OR-of-statuses. `assignee` is an exact match against the task's `assignee` frontmatter (e.g. `human:michael`, `ship`). `body_contains` is a case-insensitive literal substring against the task body. The four date-range pairs — `created`, `updated`, `completed`, `claimed` — each take `_after` (inclusive, >=) and `_before` (exclusive, <) RFC 3339 timestamps. Filtering on `completed_*` or `claimed_*` drops rows where that timestamp is null. Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `completed_at` | `claimed_at` (default `created_at`); sorting by a nullable field (`completed_at`, `claimed_at`) drops rows where that field is null. `desc: true` reverses (default ascending). `limit` caps the rows. Pass bodies:false to omit task bodies + notes (frontmatter only) — use it when drilling down from project.overview."
    )]
    async fn task_list(
        &self,
        Parameters(args): Parameters<TaskListArgs>,
    ) -> Result<Json<TaskListResult>, ErrorData> {
        if args.phase.is_some() && args.project.is_none() {
            return Err(ErrorData::invalid_params(
                "phase requires project (phase slugs are unique per project, not across the corpus)",
                None,
            ));
        }
        let with_bodies = args.bodies.unwrap_or(true);
        let filter = TaskListFilter::from(args);
        let mut tasks: Vec<Task> = self
            .store
            .list_tasks(filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        if !with_bodies {
            for t in &mut tasks {
                t.body.clear();
                t.notes.clear();
            }
        }
        Ok(Json(TaskListResult { tasks }))
    }

    #[tool(
        name = "artifact.list",
        description = "List the artifacts linked to a project (commits, PRs, files, URLs, runs, docs)."
    )]
    async fn artifact_list(
        &self,
        Parameters(args): Parameters<ArtifactListArgs>,
    ) -> Result<Json<ArtifactListResult>, ErrorData> {
        let all = self
            .store
            .list_artifacts(ArtifactListFilter {
                project: args.project.clone(),
            })
            .await
            .map_err(store_err)?;
        let artifacts = all
            .into_iter()
            .filter(|a| args.task.is_empty() || a.task == args.task)
            .filter(|a| args.kind.is_empty() || a.kind == args.kind)
            .collect();
        Ok(Json(ArtifactListResult { artifacts }))
    }

    #[tool(
        name = "search",
        description = "Unified case-insensitive literal substring search across project titles + description bodies, phase titles + bodies, and task titles + spec bodies (not notes, assignee, or other frontmatter). One call returns a single ranked list so the model can pick rows to open — use this instead of three `body_contains` list round-trips when you don't already know the primitive kind. Each hit includes `score` overlapping literal match count in title+newline+body (higher is stronger), `snippet` (~80 characters centered on the first match, no markdown awareness), and rows are ordered by `score` descending then `updated_at` descending; `limit` (default 50) applies after sorting. `kinds` filters to one or more of `project` | `phase` | `task` (default: all). `project` restricts to one project slug; omit or null for the whole corpus. Unlike the list verbs, search INCLUDES terminal (`done` / `cancelled` / `skipped` / `abandoned`) items BY DEFAULT — finding completed work is a core use; pass `include_terminal: false` to scope to live items only. Empty `query` is rejected; no matches returns an empty list. Prefer list verbs with `body_contains` when you already know you're only looking for tasks (or phases, projects)."
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<Json<SearchResult>, ErrorData> {
        if args.query.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "query must be a non-empty literal substring",
                None,
            ));
        }
        let hits = self.search_corpus(&args).await?;
        Ok(Json(SearchResult { hits }))
    }
}

// CAS retry budget — cloud spec §8 (single budget for every lifted CAS loop).
const CAS_RETRY_BASE_MS: u64 = 25;
const CAS_RETRY_CAP_MS: u64 = 2000;
const CAS_MAX_ATTEMPTS: u32 = 5;

async fn cas_backoff(attempt: u32) {
    let exponent = attempt.min(6);
    let max_ms = CAS_RETRY_BASE_MS
        .saturating_mul(1_u64 << exponent)
        .min(CAS_RETRY_CAP_MS);
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()) % (max_ms + 1));
    if jitter > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(jitter)).await;
    }
}

fn invalid_msg(msg: impl std::fmt::Display) -> StoreError {
    StoreError::Invalid(msg.to_string())
}

fn domain_err(err: &AnyhowError) -> StoreError {
    StoreError::Invalid(err.to_string())
}

fn store_err_to_invalid(err: StoreError) -> ErrorData {
    match err {
        StoreError::NotFound => ErrorData::invalid_params("not found", None),
        StoreError::Conflict => ErrorData::invalid_request("conflict", None),
        StoreError::Unavailable => ErrorData::internal_error("unavailable", None),
        StoreError::Invalid(msg) => {
            let lower = msg.to_lowercase();
            if is_user_domain_error(&lower) {
                ErrorData::invalid_params(msg, None)
            } else {
                ErrorData::internal_error(msg, None)
            }
        }
        StoreError::Io(e) => internal(e),
    }
}

/// Substrings matching `bail!` / validation messages in `store.rs`, lowercased
/// for [`is_user_domain_error`]. Tightly coupled to the wording of those
/// `bail!` bodies — when a new store verb is added or an existing message is
/// reworded, mirror the change here or the new user error silently surfaces
/// as `internal_error`. Markers that describe corpus corruption or server
/// misconfiguration are deliberately absent so they fall through to
/// `internal_error`.
const USER_ERROR_MARKERS: &[&str] = &[
    "not found",
    "slug is required",
    "slug must be lowercase ascii",
    "project slug must be lowercase ascii",
    "phase slug must be lowercase ascii",
    "project slug already exists",
    "project is required",
    "phase slug already exists in project",
    "project and slug are required",
    "actor is required",
    "owner is required",
    "owner must not be empty",
    "phase is required (omit the field entirely for a project-wide task)",
    "task slug already exists in project",
    "cannot claim task in terminal state",
    "task already claimed by",
    "task must be in_progress to complete",
    "kind is required",
    "ref is required",
    "label is required",
    "must be single-line (no newline or carriage return)",
    "task is empty (omit the field entirely for a project-wide artifact)",
    "use task.claim to transition into claimed",
    "use task.complete to transition into done",
    "task is in a terminal state",
    "invalid task transition",
    "task body must not contain",
    "note must be single-line",
    "actor must not contain `: `",
    "note body must not be empty",
    "search query must be non-empty",
    "search: project filter must be non-empty",
];

/// Map an internal error to an MCP `ErrorData`. Generic over `ToString`
/// so callers can pass `.map_err(internal)` without an extra closure;
/// the `needless_pass_by_value` lint is allowed here because `map_err`
/// hands the error to its callback by value.
#[allow(clippy::needless_pass_by_value)]
fn internal<E: ToString>(e: E) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Map a [`StoreError`] onto MCP `ErrorData`. `NotFound` and validation-style
/// `Invalid` values become request errors; `Conflict` is the CAS mismatch path.
fn store_err(err: StoreError) -> ErrorData {
    match err {
        StoreError::NotFound => ErrorData::invalid_params("not found", None),
        StoreError::Conflict => ErrorData::invalid_request("conflict", None),
        StoreError::Unavailable => ErrorData::internal_error("unavailable", None),
        StoreError::Invalid(msg) => {
            let lower = msg.to_lowercase();
            if is_user_domain_error(&lower) {
                ErrorData::invalid_params(msg, None)
            } else {
                ErrorData::internal_error(msg, None)
            }
        }
        StoreError::Io(e) => internal(e),
    }
}

/// Classify `anyhow` errors from store-layer failures into MCP request validation vs
/// server faults by matching substrings from the error chain.
#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
fn internal_or_invalid(err: AnyhowError) -> ErrorData {
    let msg = err.to_string();
    let chain = err
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    if is_user_domain_error(&chain.to_lowercase()) {
        ErrorData::invalid_params(msg, None)
    } else {
        ErrorData::internal_error(msg, None)
    }
}

fn is_user_domain_error(chain: &str) -> bool {
    let lower = chain.to_lowercase();
    USER_ERROR_MARKERS.iter().any(|m| lower.contains(m))
}

/// `tsk_` plus a 26-character Crockford ULID payload (same rule as new ids).
fn is_well_formed_task_id(id: &str) -> bool {
    let Some(ulid_part) = id.strip_prefix("tsk_") else {
        return false;
    };
    ulid_part.len() == 26 && Ulid::from_string(ulid_part).is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::cast_possible_truncation
    )]

    use super::*;
    use rmcp::model::ErrorCode;

    #[test]
    fn internal_or_invalid_unknown_message_is_internal_error() {
        let err = internal_or_invalid(anyhow::anyhow!(
            "simulated failure without user-facing store markers"
        ));
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
    }

    #[test]
    fn internal_or_invalid_corrupt_state_stays_internal_error() {
        for msg in [
            "task in todo has assignee, corrupt state",
            "task in state in_progress has no assignee, corrupt state",
        ] {
            let err = internal_or_invalid(anyhow::anyhow!(msg.to_owned()));
            assert_eq!(
                err.code,
                ErrorCode::INTERNAL_ERROR,
                "corpus-corruption error must not surface as invalid_params: {msg}"
            );
        }
    }

    #[test]
    fn internal_or_invalid_missing_corpus_marker_is_internal_error() {
        let err = internal_or_invalid(anyhow::anyhow!(
            "not a dossier corpus: .dossier marker missing"
        ));
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
    }
}
