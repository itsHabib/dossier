//! Task verbs: DTOs and service-layer policy for `task.*`.
//!
//! The `#[tool]` wrappers stay in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the argument/result DTOs, the CAS write
//! policy, and the task tests.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use anyhow::Error as AnyhowError;

use crate::domain::{
    append_task_note, apply_claim_task, apply_task_body_update, apply_task_complete,
    apply_task_status_update, is_valid_slug, new_id, normalize_blocked_by, resolve_status,
    validate_task_body, PhaseListFilter, Task, TaskListFilter, TaskOrderField, TaskStatus,
};
use crate::store::{now_utc, ClaimTask, CompleteTask, NewTask, StoreError, UpdateTask, Versioned};

use super::{cas_backoff, domain_err, invalid_msg, MeshService, CAS_MAX_ATTEMPTS};

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
    /// Omit `status` for the live-only default (non-terminal rows); pass
    /// `include_terminal: true` to include terminal rows. An explicit list
    /// selects exact statuses; an explicit empty `[]` is "no filter" (all
    /// statuses) — distinct from omitting.
    #[serde(default)]
    pub status: Option<Vec<TaskStatus>>,
    /// when `status` is omitted, default to live (non-terminal) tasks only.
    /// Set `true` to include terminal (`done`, `cancelled`) rows; ignored when
    /// an explicit `status` is given (explicit always wins).
    #[serde(default)]
    pub include_terminal: Option<bool>,
    /// exact match against the task's `assignee` frontmatter field
    #[serde(default)]
    pub assignee: Option<String>,
    /// exact match against one canonical external blocker reference
    #[serde(default)]
    pub blocked_by: Option<String>,
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
    /// include task bodies + notes (default `true`); pass `false` to omit
    /// them (frontmatter only) for a bounded drill-down read from project.overview
    #[serde(default)]
    pub bodies: Option<bool>,
}

/// Response envelope for `task.list`.
#[derive(Serialize, JsonSchema)]
pub struct TaskListResult {
    pub tasks: Vec<Task>,
}

impl From<TaskListArgs> for TaskListFilter {
    fn from(a: TaskListArgs) -> Self {
        Self {
            project: a.project,
            phase: a.phase,
            status: resolve_status(a.status, a.include_terminal, TaskStatus::live_statuses),
            assignee: a.assignee,
            blocked_by: a.blocked_by,
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

/// Arguments for `task.create`. Task slug must be unique within the project;
/// optional `phase` slug anchors the task to that phase.
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
    /// task IDs or slugs this task depends on; omit for none
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// canonical external blocker refs (`pr:owner/repo#N` or `url:https://...`)
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

/// Arguments for `task.claim`. Same-actor re-claim on a non-terminal task is a no-op.
#[derive(Deserialize, JsonSchema)]
pub struct TaskClaimArgs {
    /// task id (ULID with `tsk_` prefix)
    pub id: String,
    /// actor claiming the task (e.g. `ship`, `claude-code:michael`)
    pub actor: String,
}

/// Arguments for `task.update`.
///
/// `status=claimed` and `status=done` are rejected — use `task.claim` /
/// `task.complete`. Terminal states reject all status transitions; remaining
/// targets are state-machine guarded.
#[derive(Deserialize, JsonSchema)]
pub struct TaskUpdateArgs {
    /// task id
    pub id: String,
    /// new body; omit to leave unchanged
    #[serde(default)]
    pub body: Option<String>,
    /// new status; omit to leave unchanged. Accepted values:
    /// (`todo` | `in_progress` | `blocked` | `cancelled`).
    /// `claimed` and `done` are rejected — use task.claim / task.complete.
    #[serde(default)]
    pub status: Option<TaskStatus>,
    /// optional note line appended to the task's `## Notes` log
    #[serde(default)]
    pub note: Option<String>,
    /// replace dependency list; omit to leave unchanged; pass `[]` to clear
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// replace external blockers; omit to leave unchanged; pass `[]` to clear
    #[serde(default)]
    pub blocked_by: Option<Vec<String>>,
    /// who's making the update
    pub actor: String,
}

/// Arguments for `task.complete`. Completes from `in_progress`, or walks
/// `todo` / `claimed` (same actor) through claim → `in_progress` → done.
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

fn task_logical_eq(a: &Task, b: &Task) -> bool {
    a.id == b.id
        && a.project == b.project
        && a.project_slug == b.project_slug
        && a.phase == b.phase
        && a.slug == b.slug
        && a.title == b.title
        && a.body == b.body
        && a.status == b.status
        && a.assignee == b.assignee
        && a.claimed_at == b.claimed_at
        && a.completed_at == b.completed_at
        && a.notes == b.notes
        && a.depends_on == b.depends_on
        && a.blocked_by == b.blocked_by
}

impl MeshService {
    /// Service-layer `task.create` — project-scoped slug uniqueness via `project.md` CAS gate.
    #[allow(
        clippy::too_many_lines,
        reason = "project-CAS gate + create-only put share one retry loop"
    )]
    pub async fn create_task(&self, args: NewTask) -> Result<Task, StoreError> {
        if args.actor.is_empty() {
            return Err(invalid_msg("actor is required to create a task"));
        }
        if args.project.is_empty() {
            return Err(invalid_msg("project is required"));
        }
        if args.slug.is_empty() {
            return Err(invalid_msg("slug is required"));
        }
        if let Some(phase) = &args.phase {
            if phase.is_empty() {
                return Err(invalid_msg(
                    "phase is required (omit the field entirely for a project-wide task)",
                ));
            }
            if !is_valid_slug(phase) {
                return Err(invalid_msg(format!(
                    "phase slug must be lowercase ascii (a-z, 0-9, -, _): {phase}"
                )));
            }
        }
        if !is_valid_slug(&args.slug) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            )));
        }
        validate_task_body(&args.body).map_err(|e| domain_err(&e))?;
        let blocked_by = normalize_blocked_by(&args.blocked_by).map_err(|e| domain_err(&e))?;

        for attempt in 0..CAS_MAX_ATTEMPTS {
            let Versioned {
                value: project,
                version: project_version,
            } = match self.store.get_project(&args.project).await {
                Ok(v) => v,
                Err(StoreError::NotFound) => {
                    return Err(invalid_msg(format!("project not found: {}", args.project)));
                }
                Err(e) => return Err(e),
            };

            let phase_id = match &args.phase {
                Some(phase_slug) => {
                    let phases = self
                        .store
                        .list_phases(PhaseListFilter {
                            project: Some(args.project.clone()),
                            ..Default::default()
                        })
                        .await?;
                    let phase = phases
                        .iter()
                        .find(|p| p.value.slug == *phase_slug)
                        .ok_or_else(|| invalid_msg(format!("phase not found: {phase_slug}")))?;
                    phase.value.id.clone()
                }
                None => String::new(),
            };

            let tasks = self
                .store
                .list_tasks(TaskListFilter {
                    project: Some(args.project.clone()),
                    ..Default::default()
                })
                .await?;
            if tasks.iter().any(|t| t.value.slug == args.slug) {
                return Err(invalid_msg(format!(
                    "task slug already exists in project: {}",
                    args.slug
                )));
            }

            let mut project_gate = project.clone();
            project_gate.updated_at = now_utc();
            match self
                .store
                .put_project(&project_gate, Some(project_version))
                .await
            {
                Ok(_) => {}
                Err(StoreError::Conflict) => {
                    let tasks = self
                        .store
                        .list_tasks(TaskListFilter {
                            project: Some(args.project.clone()),
                            ..Default::default()
                        })
                        .await?;
                    if tasks.iter().any(|t| t.value.slug == args.slug) {
                        return Err(invalid_msg(format!(
                            "task slug already exists in project: {}",
                            args.slug
                        )));
                    }
                    if attempt + 1 >= CAS_MAX_ATTEMPTS {
                        return Err(StoreError::Conflict);
                    }
                    cas_backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(e),
            }

            let tasks = self
                .store
                .list_tasks(TaskListFilter {
                    project: Some(args.project.clone()),
                    ..Default::default()
                })
                .await?;
            if tasks.iter().any(|t| t.value.slug == args.slug) {
                return Err(invalid_msg(format!(
                    "task slug already exists in project: {}",
                    args.slug
                )));
            }

            let now = now_utc();
            let id = new_id("tsk");
            let task = Task {
                id: id.clone(),
                project: project.id,
                project_slug: args.project.clone(),
                phase: phase_id,
                slug: args.slug.clone(),
                title: args.title.clone(),
                body: args.body.clone(),
                status: TaskStatus::Todo,
                assignee: String::new(),
                claimed_at: None,
                completed_at: None,
                created_at: now,
                updated_at: now,
                notes: Vec::new(),
                depends_on: args.depends_on.clone(),
                blocked_by: blocked_by.clone(),
            };
            match self.store.put_task(&task, None).await {
                Ok(_) => return Ok(task),
                Err(StoreError::Conflict) if attempt + 1 >= CAS_MAX_ATTEMPTS => {
                    return Err(StoreError::Conflict);
                }
                Err(StoreError::Conflict) => {
                    cas_backoff(attempt).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::Conflict)
    }

    /// Service-layer `task.claim` — self-CAS on the task object.
    pub async fn claim_task(&self, args: &ClaimTask) -> Result<Task, StoreError> {
        self.cas_mutate_task(&args.id, |task| {
            apply_claim_task(task, &args.actor, now_utc())
        })
        .await
    }

    /// Service-layer `task.update` — self-CAS on the task object.
    pub async fn update_task(&self, args: UpdateTask) -> Result<Task, StoreError> {
        if args.actor.is_empty() {
            return Err(invalid_msg("actor is required to update a task"));
        }
        let UpdateTask {
            id,
            body,
            status,
            note,
            actor,
            depends_on,
            blocked_by,
        } = args;
        let body = body.clone();
        let note = note.clone();
        let depends_on = depends_on.clone();
        let actor = actor.clone();
        let blocked_by = blocked_by
            .map(|values| normalize_blocked_by(&values))
            .transpose()
            .map_err(|e| domain_err(&e))?;
        self.cas_mutate_task(&id, move |mut task| {
            let now = now_utc();
            if let Some(target) = status {
                task = apply_task_status_update(task, target, now)?;
            }
            if let Some(body) = body.clone() {
                task = apply_task_body_update(task, body, now)?;
            }
            if let Some(depends_on) = depends_on.clone() {
                task.depends_on = depends_on;
            }
            if let Some(blocked_by) = blocked_by.clone() {
                task.blocked_by = blocked_by;
            }
            if let Some(note) = note.clone() {
                append_task_note(&mut task, now, &actor, &note)?;
            }
            task.updated_at = now;
            Ok(task)
        })
        .await
    }

    /// Service-layer `task.complete` — self-CAS on the task object.
    pub async fn complete_task(&self, args: CompleteTask) -> Result<Task, StoreError> {
        if args.actor.is_empty() {
            return Err(invalid_msg("actor is required to complete a task"));
        }
        let CompleteTask { id, note, actor } = args;
        let note = note.clone();
        let actor = actor.clone();
        self.cas_mutate_task(&id, move |mut task| {
            let now = now_utc();
            task = apply_task_complete(task, &actor, now)?;
            if let Some(note) = note.clone() {
                append_task_note(&mut task, now, &actor, &note)?;
            }
            Ok(task)
        })
        .await
    }

    async fn cas_mutate_task<F>(&self, id: &str, mut apply: F) -> Result<Task, StoreError>
    where
        F: FnMut(Task) -> Result<Task, AnyhowError>,
    {
        for attempt in 0..CAS_MAX_ATTEMPTS {
            let Versioned {
                value: current,
                version,
            } = self.store.get_task(id).await?;
            match apply(current.clone()) {
                Err(e) => return Err(domain_err(&e)),
                Ok(updated)
                    if updated.updated_at == current.updated_at
                        && task_logical_eq(&updated, &current) =>
                {
                    return Ok(updated);
                }
                Ok(updated) => match self.store.put_task(&updated, Some(version)).await {
                    Ok(_) => return Ok(updated),
                    Err(StoreError::Conflict) => {
                        let Versioned {
                            value: reread,
                            version: _,
                        } = self.store.get_task(id).await?;
                        match apply(reread.clone()) {
                            Err(e) => return Err(domain_err(&e)),
                            Ok(desired)
                                if desired.updated_at == reread.updated_at
                                    && task_logical_eq(&desired, &reread) =>
                            {
                                return Ok(desired);
                            }
                            Ok(_) => {
                                if attempt + 1 >= CAS_MAX_ATTEMPTS {
                                    return Err(StoreError::Conflict);
                                }
                                cas_backoff(attempt).await;
                            }
                        }
                    }
                    Err(e) => return Err(e),
                },
            }
        }
        Err(StoreError::Conflict)
    }
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
    use crate::domain::TaskGetArgs;
    use crate::server::testutil::{
        block_on, fresh_service, repo_root, seed_project, seed_store, seed_task,
    };
    use crate::server::ProjectCreateArgs;
    use crate::store::FsStore;
    use rmcp::handler::server::wrapper::{Json, Parameters};
    use rmcp::model::ErrorCode;

    #[test]
    fn task_update_unknown_id_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        match block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: "tsk_01KRSZG60JG3S0JF294AA3459V".to_owned(),
            body: None,
            status: None,
            note: None,
            actor: "human:test".to_owned(),
            depends_on: None,
            blocked_by: None,
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert!(
                    err.message.contains("not found"),
                    "unexpected message: {}",
                    err.message
                );
            }
            Ok(_) => panic!("unknown task id must be rejected"),
        }
    }

    #[test]
    fn get_task_errors_on_malformed_id() {
        let (_tmp, svc) = fresh_service();
        match block_on(svc.task_get(Parameters(TaskGetArgs {
            id: "tsk_nope".to_owned(),
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert_eq!(err.message, "invalid id format");
            }
            Ok(_) => panic!("malformed task id must be rejected"),
        }
    }

    #[test]
    fn get_task_not_found_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        let absent = "tsk_01KRSZG60JG3S0JF294AA3459V";
        match block_on(svc.task_get(Parameters(TaskGetArgs {
            id: absent.to_owned(),
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert!(
                    err.message.contains("task not found"),
                    "unexpected error: {}",
                    err.message
                );
            }
            Ok(_) => panic!("absent task id must return not-found"),
        }
    }

    #[test]
    fn task_complete_on_todo_transitions_to_done() {
        let (_tmp, svc) = fresh_service();
        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("project.create");
        let Json(task) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "t1".to_owned(),
            title: "T".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("task.create");
        let Json(done) = block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: task.id,
            note: None,
            actor: "human:test".to_owned(),
        })))
        .expect("task.complete from todo");
        assert_eq!(done.status, TaskStatus::Done);
        assert_eq!(done.assignee, "human:test");
        assert!(done.claimed_at.is_some());
        assert!(done.completed_at.is_some());
    }

    #[test]
    fn task_complete_on_claimed_by_other_actor_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("project.create");
        let Json(task) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "t1".to_owned(),
            title: "T".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("task.create");
        block_on(svc.claim_task(&ClaimTask {
            id: task.id.clone(),
            actor: "human:alice".to_owned(),
        }))
        .expect("claim");
        match block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: task.id,
            note: None,
            actor: "human:bob".to_owned(),
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert!(
                    err.message.contains("task already claimed by"),
                    "unexpected message: {}",
                    err.message
                );
            }
            Ok(_) => panic!("cross-actor complete must be rejected"),
        }
    }

    #[test]
    fn task_update_reject_done_via_update_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("project.create");
        let Json(task) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "t1".to_owned(),
            title: "T".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("task.create");
        match block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: task.id,
            body: None,
            status: Some(TaskStatus::Done),
            note: None,
            actor: "human:test".to_owned(),
            depends_on: None,
            blocked_by: None,
        }))) {
            Err(err) => assert_eq!(err.code, ErrorCode::INVALID_PARAMS),
            Ok(_) => panic!("done via update must be rejected"),
        }
    }

    #[test]
    fn task_list_rejects_phase_without_project() {
        let (_tmp, svc) = fresh_service();
        let args = TaskListArgs {
            phase: Some("spec".to_owned()),
            ..Default::default()
        };
        match block_on(svc.task_list(Parameters(args))) {
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
        match block_on(svc.task_list(Parameters(args))) {
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
    fn task_list_bodies_false_strips_body_and_notes() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        let Json(t) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "t1".to_owned(),
            title: "T1".to_owned(),
            body: "secret task body".to_owned(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("task.create");
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: t.id,
            body: None,
            status: None,
            note: Some("a progress note".to_owned()),
            depends_on: None,
            blocked_by: None,
            actor: "human:test".to_owned(),
        })))
        .expect("note");

        // Default keeps body + notes.
        let Json(full) = block_on(svc.task_list(Parameters(TaskListArgs {
            project: Some("alpha".to_owned()),
            ..Default::default()
        })))
        .expect("task.list default");
        assert_eq!(full.tasks[0].body, "secret task body");
        assert!(!full.tasks[0].notes.is_empty());

        // bodies: false strips both.
        let Json(stripped) = block_on(svc.task_list(Parameters(TaskListArgs {
            project: Some("alpha".to_owned()),
            bodies: Some(false),
            ..Default::default()
        })))
        .expect("task.list bodies false");
        assert!(stripped.tasks[0].body.is_empty());
        assert!(stripped.tasks[0].notes.is_empty());
        assert_eq!(stripped.tasks[0].slug, "t1");
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
            include_terminal: None,
            assignee: Some("ship".to_owned()),
            blocked_by: Some("pr:owner/repo#7".to_owned()),
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
            bodies: None,
        });

        assert_eq!(f.project.as_deref(), Some("rho"));
        assert_eq!(f.phase.as_deref(), Some("implement"));
        assert_eq!(f.status, Some(vec![TaskStatus::Todo, TaskStatus::Blocked]));
        assert_eq!(f.assignee.as_deref(), Some("ship"));
        assert_eq!(f.blocked_by.as_deref(), Some("pr:owner/repo#7"));
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

    #[test]
    fn create_task_persists_depends_on() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");

        let created = block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "deps".to_owned(),
            title: "Deps".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: vec!["tsk_a".into(), "tsk_b".into()],
            blocked_by: Vec::new(),
        }))
        .expect("create task with depends_on");

        assert_eq!(
            created.depends_on,
            vec!["tsk_a".to_owned(), "tsk_b".to_owned()]
        );

        let Json(listed) = block_on(svc.task_list(Parameters(TaskListArgs {
            project: Some("alpha".to_owned()),
            ..Default::default()
        })))
        .expect("list tasks");
        let listed = listed
            .tasks
            .into_iter()
            .find(|t| t.id == created.id)
            .expect("created task in list");
        assert_eq!(
            listed.depends_on,
            vec!["tsk_a".to_owned(), "tsk_b".to_owned()]
        );

        let Json(got) =
            block_on(svc.task_get(Parameters(TaskGetArgs { id: created.id }))).expect("get task");
        assert_eq!(got.depends_on, vec!["tsk_a".to_owned(), "tsk_b".to_owned()]);
    }

    #[test]
    fn update_task_replaces_depends_on() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let task = block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "replace-deps".to_owned(),
            title: "Replace deps".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: vec!["tsk_a".into(), "tsk_b".into()],
            blocked_by: Vec::new(),
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: None,
            actor: "human:test".to_owned(),
            depends_on: Some(vec!["tsk_c".into()]),
            blocked_by: None,
        }))
        .expect("update depends_on");

        assert_eq!(updated.depends_on, vec!["tsk_c".to_owned()]);
    }

    #[test]
    fn update_task_clears_depends_on_with_empty_list() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let task = block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "clear-deps".to_owned(),
            title: "Clear deps".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: vec!["tsk_a".into()],
            blocked_by: Vec::new(),
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: None,
            actor: "human:test".to_owned(),
            depends_on: Some(vec![]),
            blocked_by: None,
        }))
        .expect("clear depends_on");

        assert!(updated.depends_on.is_empty());
    }

    #[test]
    fn update_task_leaves_depends_on_when_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let task = block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "keep-deps".to_owned(),
            title: "Keep deps".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: vec!["tsk_a".into(), "tsk_b".into()],
            blocked_by: Vec::new(),
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: Some("progress note".to_owned()),
            actor: "human:test".to_owned(),
            depends_on: None,
            blocked_by: None,
        }))
        .expect("update note only");

        assert_eq!(
            updated.depends_on,
            vec!["tsk_a".to_owned(), "tsk_b".to_owned()]
        );
    }

    #[test]
    fn claim_cas_one_winner_one_terminal() {
        use crate::store::{ClaimTask, StoreError};

        for _ in 0..8 {
            let tmp = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
            let svc = MeshService::new(FsStore::open(tmp.path()).expect("open race corpus"));
            seed_project(&svc, "race");
            let task = seed_task(&svc, "race", "target");

            block_on(svc.claim_task(&ClaimTask {
                id: task.id.clone(),
                actor: "alice".to_owned(),
            }))
            .expect("first claim wins");

            let err = block_on(svc.claim_task(&ClaimTask {
                id: task.id,
                actor: "bob".to_owned(),
            }))
            .expect_err("second claim must terminal-reject");
            let StoreError::Invalid(msg) = err else {
                panic!("expected terminal invalid error, got {err:?}");
            };
            assert!(
                msg.contains("already claimed"),
                "unexpected terminal message: {msg}"
            );
        }
    }

    #[test]
    fn create_task_cas_slug_one_winner_one_terminal() {
        use crate::store::StoreError;

        for _ in 0..8 {
            let tmp = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
            let svc = MeshService::new(FsStore::open(tmp.path()).expect("open race corpus"));
            seed_project(&svc, "race");
            let args = NewTask {
                project: "race".to_owned(),
                phase: None,
                slug: "dupe".to_owned(),
                title: "Dupe".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
                blocked_by: Vec::new(),
            };
            block_on(svc.create_task(args.clone())).expect("first create wins");
            let err =
                block_on(svc.create_task(args)).expect_err("duplicate slug must terminal-reject");
            let StoreError::Invalid(msg) = err else {
                panic!("expected terminal invalid error, got {err:?}");
            };
            assert!(
                msg.contains("task slug already exists in project"),
                "unexpected terminal message: {msg}"
            );

            let Json(listed) = block_on(svc.task_list(Parameters(TaskListArgs {
                project: Some("race".to_owned()),
                ..Default::default()
            })))
            .expect("list tasks");
            assert_eq!(
                listed.tasks.iter().filter(|t| t.slug == "dupe").count(),
                1,
                "project must hold exactly one task with slug dupe"
            );
        }
    }

    // --- default-live reads (include_terminal policy at the verb layer) ---

    fn tasks_listed(svc: &MeshService, args: TaskListArgs) -> Vec<Task> {
        let Json(result) = block_on(svc.task_list(Parameters(args))).expect("task.list");
        result.tasks
    }

    fn cancel_task(svc: &MeshService, id: &str) {
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: id.to_owned(),
            body: None,
            status: Some(TaskStatus::Cancelled),
            note: None,
            depends_on: None,
            blocked_by: None,
            actor: "human:test".to_owned(),
        })))
        .expect("cancel task");
    }

    /// One live (`todo`) + one terminal (`cancelled`) task in a project.
    fn seed_live_and_terminal_tasks(svc: &MeshService) {
        seed_project(svc, "alpha");
        seed_task(svc, "alpha", "live-task");
        let term = seed_task(svc, "alpha", "terminal-task");
        cancel_task(svc, &term.id);
    }

    #[test]
    fn task_list_defaults_to_live_only() {
        let (_tmp, svc) = fresh_service();
        seed_live_and_terminal_tasks(&svc);

        let tasks = tasks_listed(&svc, TaskListArgs::default());
        assert_eq!(tasks.len(), 1, "default task.list must drop terminal rows");
        assert_eq!(tasks[0].slug, "live-task");
        assert!(tasks.iter().all(|t| !t.status.is_terminal()));
    }

    #[test]
    fn task_list_include_terminal_restores_all() {
        let (_tmp, svc) = fresh_service();
        seed_live_and_terminal_tasks(&svc);

        let tasks = tasks_listed(
            &svc,
            TaskListArgs {
                include_terminal: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(tasks.len(), 2, "include_terminal:true returns every status");
        assert!(tasks.iter().any(|t| t.status.is_terminal()));
    }

    #[test]
    fn task_list_explicit_status_wins_over_default_live() {
        let (_tmp, svc) = fresh_service();
        seed_live_and_terminal_tasks(&svc);

        // Explicit terminal status returns terminal rows even though
        // include_terminal is omitted (D2 — explicit always wins).
        let tasks = tasks_listed(
            &svc,
            TaskListArgs {
                status: Some(vec![TaskStatus::Cancelled]),
                ..Default::default()
            },
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn task_list_empty_status_returns_all_including_terminal() {
        let (_tmp, svc) = fresh_service();
        seed_live_and_terminal_tasks(&svc);

        // `status: []` is an explicit empty filter; the store matcher treats
        // it as "no filter" => all rows incl. terminal. Intentional (D2).
        let tasks = tasks_listed(
            &svc,
            TaskListArgs {
                status: Some(Vec::new()),
                ..Default::default()
            },
        );
        assert_eq!(tasks.len(), 2);
    }

    // --- verb-level dogfood against the in-repo fixture (regression both ways) ---

    fn dogfood_service() -> MeshService {
        MeshService::new(seed_store(&repo_root()))
    }

    #[test]
    fn dogfood_task_list_live_count_and_full_count() {
        let svc = dogfood_service();

        // The committed fixture's tasks are all terminal (`done`), so the
        // live-by-default surface is empty and include_terminal recovers the
        // full set — exact counts, regression in both directions.
        let live = tasks_listed(
            &svc,
            TaskListArgs {
                project: Some("dossier".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            live.len(),
            0,
            "default task.list returns only live fixture tasks"
        );

        let all = tasks_listed(
            &svc,
            TaskListArgs {
                project: Some("dossier".to_owned()),
                include_terminal: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            all.len(),
            3,
            "include_terminal:true recovers every fixture task"
        );
    }
}
