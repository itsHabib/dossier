//! Project verbs: DTOs and service-layer policy for `project.*`.
//!
//! The `#[tool]` wrappers stay in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the argument/result DTOs, the create/update
//! policy, the overview aggregation, and the project tests.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{
    is_valid_slug, new_id, resolve_status, truncate_description, Artifact, OverviewTotals, Phase,
    PhaseOverview, PhaseStatusCounts, Project, ProjectListFilter, ProjectOrderField,
    ProjectOverview, ProjectOverviewMeta, ProjectStatus, Task, TaskStatusCounts, UnphasedOverview,
};
use crate::store::{now_utc, NewProject, StoreError, UpdateProject, Versioned};

use super::{invalid_msg, MeshService};

/// Response envelope for `project.list`.
#[derive(Serialize, JsonSchema)]
pub struct ProjectListResult {
    pub projects: Vec<Project>,
}

/// Arguments for `project.get`.
#[derive(Deserialize, JsonSchema)]
pub struct ProjectGetArgs {
    /// project slug, e.g. "dossier"
    pub slug: String,
}

/// Arguments for `project.overview`.
#[derive(Deserialize, JsonSchema)]
pub struct ProjectOverviewArgs {
    /// project slug, e.g. "dossier"
    pub slug: String,
}

/// Aggregated project snapshot returned by `project.get` — project row plus
/// all phases, tasks, and artifacts for that slug.
#[derive(Serialize, JsonSchema)]
pub struct ProjectView {
    pub project: Project,
    pub phases: Vec<Phase>,
    pub tasks: Vec<Task>,
    pub artifacts: Vec<Artifact>,
}

/// Predicate-shaped arguments for `project.list`. Every field is optional.
///
/// With no `status` filter the default is live projects only (non-terminal),
/// sorted by `created_at` ASC — pass `include_terminal: true` to include
/// `done` / `abandoned`.
#[derive(Deserialize, JsonSchema, Default)]
pub struct ProjectListArgs {
    /// if set, only projects whose status is in this list
    /// (`planning` | `active` | `paused` | `done` | `abandoned`).
    /// Omit `status` for the live-only default (non-terminal rows); pass
    /// `include_terminal: true` to include terminal rows. An explicit list
    /// selects exact statuses; an explicit empty `[]` is "no filter" (all
    /// statuses) — distinct from omitting.
    #[serde(default)]
    pub status: Option<Vec<ProjectStatus>>,
    /// when `status` is omitted, default to live (non-terminal) projects only.
    /// Set `true` to include terminal (`done`, `abandoned`) rows; ignored when
    /// an explicit `status` is given (explicit always wins).
    #[serde(default)]
    pub include_terminal: Option<bool>,
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

impl From<ProjectListArgs> for ProjectListFilter {
    fn from(a: ProjectListArgs) -> Self {
        Self {
            status: resolve_status(a.status, a.include_terminal, ProjectStatus::live_statuses),
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

/// Arguments for `project.create`. Slug must be unique in the corpus and pass slug rules.
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

/// Arguments for `project.update`. Slug is immutable; omit fields to leave them unchanged.
#[derive(Deserialize, JsonSchema)]
pub struct ProjectUpdateArgs {
    /// project slug (addressing key — slug is immutable in v0)
    pub slug: String,
    /// optional actor for clients that send it; omit on update.
    /// An explicit empty string is rejected — v0 does not record actors on
    /// updates, but `""` is never meaningful for auditing.
    #[serde(default)]
    pub actor: Option<String>,
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

/// Aggregate a project's phases, tasks, and artifact count into a bounded
/// [`ProjectOverview`]. Pure policy over already-loaded rows: partitions
/// tasks across phase rows (joined by `task.phase == phase.id`) and an
/// `unphased` bucket (empty or dangling phase id), tallies per-status
/// counts, and bounds the description. No bodies or notes are read.
pub(super) fn build_overview(
    project: &Project,
    phases: Vec<Phase>,
    tasks: &[Task],
    artifact_count: usize,
) -> ProjectOverview {
    let (description, description_truncated) = truncate_description(&project.description);

    let mut phase_counts: std::collections::HashMap<String, TaskStatusCounts> =
        std::collections::HashMap::with_capacity(phases.len());
    for ph in &phases {
        phase_counts.insert(ph.id.clone(), TaskStatusCounts::default());
    }

    let mut unphased = TaskStatusCounts::default();
    let mut tasks_by_status = TaskStatusCounts::default();
    for t in tasks {
        tasks_by_status.add(t.status);
        match phase_counts.get_mut(&t.phase) {
            Some(counts) => counts.add(t.status),
            None => unphased.add(t.status),
        }
    }

    let mut phases_by_status = PhaseStatusCounts::default();
    let mut phase_rows: Vec<PhaseOverview> = Vec::with_capacity(phases.len());
    for ph in phases {
        phases_by_status.add(ph.status);
        let task_counts = phase_counts.get(&ph.id).copied().unwrap_or_default();
        phase_rows.push(PhaseOverview {
            id: ph.id,
            slug: ph.slug,
            title: ph.title,
            order: ph.order,
            status: ph.status,
            owner: ph.owner,
            updated_at: ph.updated_at,
            task_counts,
        });
    }
    phase_rows.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));

    ProjectOverview {
        project: ProjectOverviewMeta {
            id: project.id.clone(),
            slug: project.slug.clone(),
            title: project.title.clone(),
            status: project.status,
            created_at: project.created_at,
            updated_at: project.updated_at,
            created_by: project.created_by.clone(),
            description,
            description_truncated,
        },
        phases: phase_rows,
        unphased: UnphasedOverview {
            task_counts: unphased,
        },
        totals: OverviewTotals {
            phases_by_status,
            tasks_by_status,
            #[allow(clippy::cast_possible_truncation)] // corpus artifact counts are far below u32::MAX
            artifact_count: artifact_count as u32,
        },
    }
}

impl MeshService {
    /// Service-layer `project.create` — rejects duplicate slugs before `put_project`.
    pub async fn create_project(&self, args: NewProject) -> Result<Project, StoreError> {
        if args.slug.is_empty() {
            return Err(invalid_msg("slug is required"));
        }
        if !is_valid_slug(&args.slug) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            )));
        }
        match self.store.get_project(&args.slug).await {
            Ok(_) => {
                return Err(invalid_msg(format!(
                    "project slug already exists: {}",
                    args.slug
                )));
            }
            Err(StoreError::NotFound) => {}
            Err(e) => return Err(e),
        }

        let now = now_utc();
        let project = Project {
            id: new_id("prj"),
            slug: args.slug,
            title: args.title,
            description: args.description,
            status: ProjectStatus::Planning,
            created_at: now,
            updated_at: now,
            created_by: args.actor,
        };
        self.store.put_project(&project, None).await?;
        Ok(project)
    }

    /// Service-layer `project.update` — CAS on `project.md`.
    pub async fn update_project(&self, args: UpdateProject) -> Result<Project, StoreError> {
        if args.slug.is_empty() {
            return Err(invalid_msg("slug is required"));
        }
        if !is_valid_slug(&args.slug) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            )));
        }
        let Versioned {
            value: mut project,
            version,
        } = match self.store.get_project(&args.slug).await {
            Ok(v) => v,
            Err(StoreError::NotFound) => {
                return Err(invalid_msg(format!("project not found: {}", args.slug)));
            }
            Err(e) => return Err(e),
        };
        if let Some(title) = args.title {
            project.title = title;
        }
        if let Some(description) = args.description {
            project.description = description;
        }
        if let Some(status) = args.status {
            project.status = status;
        }
        project.updated_at = now_utc();
        self.store.put_project(&project, Some(version)).await?;
        Ok(project)
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
    use crate::domain::{PhaseStatus, TaskStatus};
    use crate::server::phase::{PhaseAddArgs, PhaseUpdateArgs};
    use crate::server::task::{TaskClaimArgs, TaskCompleteArgs, TaskCreateArgs, TaskUpdateArgs};
    use crate::server::testutil::{
        assert_rejects_invalid_project_slug, block_on, fresh_service, repo_root, seed_project,
        seed_task, set_task_field, task_file_path, INVALID_PROJECT_SLUG,
    };
    use crate::store::FsStore;
    use rmcp::handler::server::wrapper::{Json, Parameters};
    use rmcp::model::ErrorCode;
    use std::path::{Path, PathBuf};

    #[test]
    fn project_update_explicit_empty_actor_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("project.create");
        match block_on(svc.project_update(Parameters(ProjectUpdateArgs {
            slug: "alpha".to_owned(),
            actor: Some(String::new()),
            title: None,
            description: None,
            status: None,
        }))) {
            Err(err) => assert_eq!(err.code, ErrorCode::INVALID_PARAMS),
            Ok(_) => panic!("empty actor must be rejected"),
        }
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
    fn project_list_accepts_empty_args() {
        // Smoke: a completely empty arg set is a valid call (list everything).
        let (_tmp, svc) = fresh_service();
        let out = match block_on(svc.project_list(Parameters(ProjectListArgs::default()))) {
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
    fn project_update_args_ignores_actor_from_old_clients() {
        let raw = r#"{"slug": "alpha", "actor": "human:legacy"}"#;
        let args: ProjectUpdateArgs = serde_json::from_str(raw).expect("deserialize");
        assert_eq!(args.slug, "alpha");
        assert!(args.title.is_none());
        assert!(args.description.is_none());
        assert!(args.status.is_none());
    }

    #[test]
    fn project_update_and_phase_update_succeed_without_actor() {
        let (_tmp, svc) = fresh_service();

        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("project.create");

        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "spec".to_owned(),
            title: "Spec".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        })))
        .expect("phase.add");

        let Json(project) = block_on(svc.project_update(Parameters(ProjectUpdateArgs {
            slug: "alpha".to_owned(),
            actor: None,
            title: Some("Alpha2".to_owned()),
            description: None,
            status: Some(ProjectStatus::Active),
        })))
        .expect("project.update");
        assert_eq!(project.title, "Alpha2");
        assert_eq!(project.status, ProjectStatus::Active);

        let Json(phase) = block_on(svc.phase_update(Parameters(PhaseUpdateArgs {
            project: "alpha".to_owned(),
            slug: "spec".to_owned(),
            title: Some("Spec2".to_owned()),
            body: None,
            status: Some(PhaseStatus::Done),
            owner: None,
        })))
        .expect("phase.update");
        assert_eq!(phase.title, "Spec2");
        assert_eq!(phase.status, PhaseStatus::Done);
    }

    #[test]
    fn project_get_scopes_phases_and_tasks_to_requested_project() {
        let (_tmp, svc) = fresh_service();

        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "alpha".to_owned(),
            title: "Alpha".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("create alpha");

        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "beta".to_owned(),
            title: "Beta".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("create beta");

        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "alpha-phase".to_owned(),
            title: "Alpha phase".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        })))
        .expect("alpha phase");

        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "beta".to_owned(),
            slug: "beta-phase".to_owned(),
            title: "Beta phase".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        })))
        .expect("beta phase");

        block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "alpha".to_owned(),
            phase: None,
            slug: "alpha-task".to_owned(),
            title: "Alpha task".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("alpha task");

        block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "beta".to_owned(),
            phase: None,
            slug: "beta-task".to_owned(),
            title: "Beta task".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("beta task");

        let Json(view) = block_on(svc.project_get(Parameters(ProjectGetArgs {
            slug: "alpha".to_owned(),
        })))
        .expect("project.get alpha");

        assert_eq!(view.phases.len(), 1, "scoped to alpha project");
        assert_eq!(view.phases[0].slug, "alpha-phase");

        assert_eq!(view.tasks.len(), 1, "scoped to alpha project");
        assert_eq!(view.tasks[0].slug, "alpha-task");
    }

    fn phase_file_path(corpus: &Path, project_slug: &str, order: i32, slug: &str) -> PathBuf {
        corpus
            .join("projects")
            .join(project_slug)
            .join("phases")
            .join(format!("{order:02}-{slug}.md"))
    }

    fn overview_for(svc: &MeshService, slug: &str) -> ProjectOverview {
        let Json(ov) = block_on(svc.project_overview(Parameters(ProjectOverviewArgs {
            slug: slug.to_owned(),
        })))
        .expect("project.overview");
        ov
    }

    fn create_task_in_phase(svc: &MeshService, project: &str, phase: &str, slug: &str) -> Task {
        let Json(t) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: project.to_owned(),
            phase: Some(phase.to_owned()),
            slug: slug.to_owned(),
            title: slug.to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })))
        .expect("task.create");
        t
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn project_overview_aggregates_counts_and_partitions_tasks() {
        let (tmp, svc) = fresh_service();
        let corpus = tmp.path();
        seed_project(&svc, "alpha");

        // Two phases at mixed statuses.
        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "p1".to_owned(),
            title: "Phase 1".to_owned(),
            body: "phase one body".to_owned(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:p1".to_owned(),
        })))
        .expect("p1");
        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "p2".to_owned(),
            title: "Phase 2".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:p2".to_owned(),
        })))
        .expect("p2");
        block_on(svc.phase_update(Parameters(PhaseUpdateArgs {
            project: "alpha".to_owned(),
            slug: "p2".to_owned(),
            title: None,
            body: None,
            status: Some(PhaseStatus::Done),
            owner: None,
        })))
        .expect("p2 done");

        // p1: one todo, one done. p2: one in_progress.
        let _t_todo = create_task_in_phase(&svc, "alpha", "p1", "t-todo");
        let t_done = create_task_in_phase(&svc, "alpha", "p1", "t-done");
        block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: t_done.id,
            note: None,
            actor: "human:test".to_owned(),
        })))
        .expect("complete t-done");
        let t_prog = create_task_in_phase(&svc, "alpha", "p2", "t-prog");
        block_on(svc.task_claim(Parameters(TaskClaimArgs {
            id: t_prog.id.clone(),
            actor: "human:test".to_owned(),
        })))
        .expect("claim t-prog");
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: t_prog.id,
            body: None,
            status: Some(TaskStatus::InProgress),
            note: None,
            depends_on: None,
            blocked_by: None,
            actor: "human:test".to_owned(),
        })))
        .expect("t-prog in_progress");

        // A genuinely unphased task (no phase).
        let _u = seed_task(&svc, "alpha", "t-unphased");

        // An orphaned-phase-id task: hand-edit its phase to a dangling id.
        let orphan = seed_task(&svc, "alpha", "t-orphan");
        set_task_field(corpus, &orphan, "phase", "phs_01J05M3K9N4F8Z7K3N9P5Q1RZZ");

        let ov = overview_for(&svc, "alpha");

        // All phases present, ordered by order ASC.
        assert_eq!(ov.phases.len(), 2);
        assert_eq!(ov.phases[0].slug, "p1");
        assert_eq!(ov.phases[1].slug, "p2");
        assert_eq!(ov.phases[0].owner, "human:p1");

        // p1 counts: 1 todo, 1 done, total 2.
        let p1 = &ov.phases[0].task_counts;
        assert_eq!(p1.todo, 1);
        assert_eq!(p1.done, 1);
        assert_eq!(p1.total, 2);
        // p2 counts: 1 in_progress, total 1.
        let p2 = &ov.phases[1].task_counts;
        assert_eq!(p2.in_progress, 1);
        assert_eq!(p2.total, 1);

        // unphased holds the empty-phase task AND the dangling-id task.
        assert_eq!(ov.unphased.task_counts.total, 2);
        assert_eq!(ov.unphased.task_counts.todo, 2);

        // Totals.
        assert_eq!(ov.totals.tasks_by_status.total, 5);
        assert_eq!(ov.totals.tasks_by_status.todo, 3);
        assert_eq!(ov.totals.tasks_by_status.done, 1);
        assert_eq!(ov.totals.tasks_by_status.in_progress, 1);
        assert_eq!(ov.totals.phases_by_status.pending, 1);
        assert_eq!(ov.totals.phases_by_status.done, 1);
        assert_eq!(ov.totals.artifact_count, 0);

        // RECONCILE INVARIANT: sum(phase totals) + unphased == grand total.
        let phase_sum: u32 = ov.phases.iter().map(|p| p.task_counts.total).sum();
        assert_eq!(
            phase_sum + ov.unphased.task_counts.total,
            ov.totals.tasks_by_status.total,
            "partition must be exhaustive even with an orphan task"
        );

        // No body/notes anywhere in the serialized output.
        let json = serde_json::to_string(&ov).expect("serialize overview");
        assert!(
            !json.contains("phase one body"),
            "no phase body in overview"
        );
        assert!(
            !json.contains("\"body\""),
            "no body field anywhere in overview"
        );
        assert!(
            !json.contains("\"notes\""),
            "no notes field anywhere in overview"
        );
    }

    #[test]
    fn project_overview_counts_have_every_key_and_total_is_sum() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "p1".to_owned(),
            title: "Phase 1".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:p1".to_owned(),
        })))
        .expect("p1");
        create_task_in_phase(&svc, "alpha", "p1", "t1");

        let ov = overview_for(&svc, "alpha");
        // Serialize and assert every status key is present (zero when none).
        let v: serde_json::Value = serde_json::to_value(&ov).expect("to value");
        let counts = &v["phases"][0]["task_counts"];
        for key in [
            "todo",
            "claimed",
            "in_progress",
            "blocked",
            "done",
            "cancelled",
            "total",
        ] {
            assert!(
                counts.get(key).is_some(),
                "missing key {key} in task_counts"
            );
        }
        let c = &ov.phases[0].task_counts;
        assert_eq!(
            c.total,
            c.todo + c.claimed + c.in_progress + c.blocked + c.done + c.cancelled,
            "total must equal the sum of the six buckets"
        );
    }

    #[test]
    fn project_overview_truncates_long_description() {
        let (_tmp, svc) = fresh_service();
        let long = "x".repeat(900);
        block_on(svc.create_project(NewProject {
            slug: "longp".to_owned(),
            title: "Long".to_owned(),
            description: long,
            actor: "human:test".to_owned(),
        }))
        .expect("create longp");

        let ov = overview_for(&svc, "longp");
        assert_eq!(ov.project.description.chars().count(), 600);
        assert!(ov.project.description_truncated);
    }

    #[test]
    fn project_overview_keeps_short_description_full() {
        let (_tmp, svc) = fresh_service();
        block_on(svc.create_project(NewProject {
            slug: "shortp".to_owned(),
            title: "Short".to_owned(),
            description: "a tight paragraph".to_owned(),
            actor: "human:test".to_owned(),
        }))
        .expect("create shortp");

        let ov = overview_for(&svc, "shortp");
        assert_eq!(ov.project.description, "a tight paragraph");
        assert!(!ov.project.description_truncated);
    }

    #[test]
    fn project_overview_empty_project_is_zeroed_not_error() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "empty");
        let ov = overview_for(&svc, "empty");
        assert!(ov.phases.is_empty());
        assert_eq!(ov.totals.tasks_by_status.total, 0);
        assert_eq!(ov.totals.phases_by_status.pending, 0);
        assert_eq!(ov.totals.artifact_count, 0);
        assert_eq!(ov.unphased.task_counts.total, 0);
    }

    #[test]
    fn project_overview_not_found_returns_invalid_params() {
        let (_tmp, svc) = fresh_service();
        match block_on(svc.project_overview(Parameters(ProjectOverviewArgs {
            slug: "nope".to_owned(),
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert!(
                    err.message.contains("project not found"),
                    "unexpected message: {}",
                    err.message
                );
            }
            Ok(_) => panic!("unknown slug must be rejected"),
        }
    }

    #[test]
    fn project_overview_fails_on_corrupt_file() {
        let (tmp, svc) = fresh_service();
        let corpus = tmp.path();
        seed_project(&svc, "alpha");
        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "p1".to_owned(),
            title: "Phase 1".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:p1".to_owned(),
        })))
        .expect("p1");

        // Corrupt the phase file (no frontmatter delimiters).
        let path = phase_file_path(corpus, "alpha", 1, "p1");
        std::fs::write(&path, "garbage, not a phase file").expect("corrupt phase");

        let result = block_on(svc.project_overview(Parameters(ProjectOverviewArgs {
            slug: "alpha".to_owned(),
        })));
        assert!(
            result.is_err(),
            "corrupt file must fail the whole overview, not skip-and-undercount"
        );
    }

    #[test]
    fn project_overview_fails_on_corrupt_task_file() {
        let (tmp, svc) = fresh_service();
        let corpus = tmp.path();
        seed_project(&svc, "alpha");
        let task = seed_task(&svc, "alpha", "t1");

        // Corrupt the task file (no frontmatter delimiters). D8 covers tasks
        // too, not just phases — a corrupt task must fail the whole overview.
        std::fs::write(task_file_path(corpus, &task), "garbage, not a task file")
            .expect("corrupt task");

        let result = block_on(svc.project_overview(Parameters(ProjectOverviewArgs {
            slug: "alpha".to_owned(),
        })));
        assert!(
            result.is_err(),
            "corrupt task file must fail the whole overview, not skip-and-undercount"
        );
    }

    #[test]
    fn project_overview_dogfood_is_bounded() {
        let store = FsStore::open(repo_root()).expect("open corpus");
        let svc = MeshService::new(store);
        let ov = overview_for(&svc, "dossier");
        assert_eq!(ov.phases.len(), 4, "in-repo fixture has 4 phases");
        assert_eq!(ov.totals.tasks_by_status.total, 3, "fixture has 3 tasks");
        assert_eq!(ov.totals.artifact_count, 3, "fixture has 3 artifacts");
        let json = serde_json::to_string(&ov).expect("serialize");
        assert!(
            json.len() <= 25_000,
            "overview must stay bounded; got {} bytes",
            json.len()
        );
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
            include_terminal: None,
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
    fn create_project_rejects_invalid_project_slug() {
        let (_tmp, svc) = fresh_service();
        let err = block_on(svc.create_project(NewProject {
            slug: INVALID_PROJECT_SLUG.to_owned(),
            title: "Bad".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }

    #[test]
    fn update_project_rejects_invalid_project_slug() {
        let (_tmp, svc) = fresh_service();
        let err = block_on(svc.update_project(UpdateProject {
            slug: INVALID_PROJECT_SLUG.to_owned(),
            ..Default::default()
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }

    fn projects_listed(svc: &MeshService, args: ProjectListArgs) -> Vec<Project> {
        let Json(result) = block_on(svc.project_list(Parameters(args))).expect("project.list");
        result.projects
    }

    #[test]
    fn project_list_defaults_to_live_only() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "live-proj");
        seed_project(&svc, "done-proj");
        block_on(svc.project_update(Parameters(ProjectUpdateArgs {
            slug: "done-proj".to_owned(),
            actor: None,
            title: None,
            description: None,
            status: Some(ProjectStatus::Done),
        })))
        .expect("mark project done");

        let live = projects_listed(&svc, ProjectListArgs::default());
        assert_eq!(
            live.len(),
            1,
            "default project.list drops terminal projects"
        );
        assert_eq!(live[0].slug, "live-proj");

        let all = projects_listed(
            &svc,
            ProjectListArgs {
                include_terminal: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            all.len(),
            2,
            "include_terminal:true returns terminal projects"
        );
    }
}
