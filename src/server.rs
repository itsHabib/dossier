//! `MCP` server wrapping a [`Store`] backend as dossier verbs.
//!
//! Read side: `project.list` / `project.get` / `phase.list` / `task.list`
//! / `task.get` / `artifact.list`. Write side: all verbs route through
//! [`MeshService`] policy over `Arc<dyn Store>`; task verbs use optimistic
//! CAS loops on the store primitives.

use std::sync::Arc;

use anyhow::Error as AnyhowError;
use chrono::{DateTime, Utc};
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::ErrorData,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::domain::{
    append_task_note, apply_claim_task, apply_complete_task, apply_task_body_update,
    apply_task_status_update, compute_new_phase_order, is_valid_slug, new_id, validate_single_line,
    validate_task_body, Artifact, Phase, PhaseListFilter, PhaseOrderField, PhaseStatus, Project,
    ProjectListFilter, ProjectOrderField, ProjectStatus, SearchArgs, SearchHit, SearchKind, Task,
    TaskGetArgs, TaskListFilter, TaskOrderField, TaskStatus,
};
use crate::store::{
    now_utc, ArtifactListFilter, ClaimTask, CompleteTask, FsStore, LinkArtifact, NewPhase,
    NewProject, NewTask, Store, StoreError, UpdatePhase, UpdateProject, UpdateTask, Versioned,
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

/// Aggregated project snapshot returned by `project.get` — project row plus
/// all phases, tasks, and artifacts for that slug.
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

/// Response envelope for `phase.list`.
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

/// Response envelope for `task.list`.
#[derive(Serialize, JsonSchema)]
pub struct TaskListResult {
    pub tasks: Vec<Task>,
}

/// Response envelope for `search`.
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

/// Arguments for `artifact.list`.
#[derive(Deserialize, JsonSchema)]
pub struct ArtifactListArgs {
    /// project slug
    pub project: String,
    /// if non-empty, only artifacts linked to this task ID
    #[serde(default)]
    pub task: String,
    /// if non-empty, only artifacts of this kind (commit, pr, file, url, run, doc)
    #[serde(default)]
    pub kind: String,
}

/// Response envelope for `artifact.list`.
#[derive(Serialize, JsonSchema)]
pub struct ArtifactListResult {
    pub artifacts: Vec<Artifact>,
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

/// Arguments for `phase.add`. Phase slug must be unique within the project;
/// `after_phase` inserts in order, omit to append.
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
    /// current responsible party (e.g. `human:mh`, `team:frontend`)
    pub owner: String,
}

/// Arguments for `phase.update`. (project, slug) is the addressing key.
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
    /// new owner; omit to leave unchanged
    #[serde(default)]
    pub owner: Option<String>,
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
    /// who's making the update
    pub actor: String,
}

/// Arguments for `task.complete`. Task must be `in_progress`.
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

/// Arguments for `artifact.link`. Appends one row to `artifacts.jsonl`.
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

impl MeshService {
    /// Case-insensitive literal substring search across the corpus via
    /// [`Store`] reads. Application-layer query — not a storage verb.
    // `too_many_lines`: single corpus walk; splitting adds indirection
    // without clarity benefit on a structurally linear function.
    // `cast_precision_loss`: `score` is a match count; f64 precision is
    // only lost past 2^53 matches, which is unreachable in practice.
    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    async fn search_corpus(&self, args: &SearchArgs) -> Result<Vec<SearchHit>, ErrorData> {
        const SNIPPET_CHARS: usize = 80;
        let needle = args.query.trim();
        let limit = args.limit.unwrap_or(50);
        let kinds = args
            .kinds
            .clone()
            .unwrap_or_else(|| vec![SearchKind::Project, SearchKind::Phase, SearchKind::Task]);
        let want_project = kinds.contains(&SearchKind::Project);
        let want_phase = kinds.contains(&SearchKind::Phase);
        let want_task = kinds.contains(&SearchKind::Task);

        let project_slugs: Vec<String> = match &args.project {
            Some(s) if s.is_empty() => {
                return Err(ErrorData::invalid_params(
                    "search: project filter must be non-empty (omit `project` field for corpus-wide search)",
                    None,
                ));
            }
            Some(s) => vec![s.clone()],
            None => self
                .store
                .list_projects(ProjectListFilter::default())
                .await
                .map_err(store_err)?
                .into_iter()
                .map(|v| v.value.slug)
                .collect(),
        };

        let mut ranked: Vec<(SearchHit, DateTime<Utc>)> = Vec::new();

        for proj_slug in &project_slugs {
            if want_project {
                let Ok(p) = self.store.get_project(proj_slug).await else {
                    continue;
                };
                let p = p.value;
                let hay = format!("{}\n{}", p.title, p.description);
                let score = count_ci_overlapping(&hay, needle);
                if score > 0 {
                    ranked.push((
                        SearchHit {
                            kind: SearchKind::Project,
                            id: p.id.clone(),
                            project: p.slug.clone(),
                            phase: None,
                            slug: p.slug.clone(),
                            title: p.title.clone(),
                            snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                            score: score as f64,
                        },
                        p.updated_at,
                    ));
                }
            }

            let phase_list = if want_phase || want_task {
                self.store
                    .list_phases(PhaseListFilter {
                        project: Some(proj_slug.clone()),
                        ..Default::default()
                    })
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .map(|v| v.value)
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let id_to_phase_slug: std::collections::HashMap<&str, &str> = phase_list
                .iter()
                .map(|ph| (ph.id.as_str(), ph.slug.as_str()))
                .collect();

            if want_phase {
                for ph in &phase_list {
                    let hay = format!("{}\n{}", ph.title, ph.body);
                    let score = count_ci_overlapping(&hay, needle);
                    if score > 0 {
                        ranked.push((
                            SearchHit {
                                kind: SearchKind::Phase,
                                id: ph.id.clone(),
                                project: proj_slug.clone(),
                                phase: None,
                                slug: ph.slug.clone(),
                                title: ph.title.clone(),
                                snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                                score: score as f64,
                            },
                            ph.updated_at,
                        ));
                    }
                }
            }

            if want_task {
                let tasks = self
                    .store
                    .list_tasks(TaskListFilter {
                        project: Some(proj_slug.clone()),
                        ..Default::default()
                    })
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .map(|v| v.value)
                    .collect::<Vec<_>>();
                for t in tasks {
                    let hay = format!("{}\n{}", t.title, t.body);
                    let score = count_ci_overlapping(&hay, needle);
                    if score > 0 {
                        let phase_slug = if t.phase.is_empty() {
                            None
                        } else {
                            id_to_phase_slug
                                .get(t.phase.as_str())
                                .map(|s| (*s).to_owned())
                        };
                        ranked.push((
                            SearchHit {
                                kind: SearchKind::Task,
                                id: t.id.clone(),
                                project: proj_slug.clone(),
                                phase: phase_slug,
                                slug: t.slug.clone(),
                                title: t.title.clone(),
                                snippet: search_snippet(&hay, needle, SNIPPET_CHARS),
                                score: score as f64,
                            },
                            t.updated_at,
                        ));
                    }
                }
            }
        }

        ranked.sort_by(|(ha, ta), (hb, tb)| hb.score.total_cmp(&ha.score).then_with(|| tb.cmp(ta)));

        Ok(ranked.into_iter().take(limit).map(|(h, _)| h).collect())
    }
}

fn unicode_ci_eq(a: char, b: char) -> bool {
    let sa: String = a.to_lowercase().collect();
    let sb: String = b.to_lowercase().collect();
    sa == sb
}

fn find_ci_start_byte(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    'outer: for (start, _) in haystack.char_indices() {
        let tail = &haystack[start..];
        let mut t = tail;
        for nc in needle.chars() {
            let Some(hc) = t.chars().next() else {
                continue 'outer;
            };
            if !unicode_ci_eq(hc, nc) {
                continue 'outer;
            }
            t = &t[hc.len_utf8()..];
        }
        return Some(start);
    }
    None
}

fn count_ci_overlapping(haystack: &str, needle: &str) -> usize {
    let h = haystack.to_lowercase();
    let n = needle.to_lowercase();
    if n.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut s = 0usize;
    while let Some(i) = h[s..].find(&n) {
        count += 1;
        let first_char_len = h[s + i..].chars().next().map_or(1, char::len_utf8);
        s += i + first_char_len;
    }
    count
}

fn search_snippet(haystack: &str, needle: &str, width: usize) -> String {
    let Some(start_byte) = find_ci_start_byte(haystack, needle) else {
        return String::new();
    };
    let start_char = haystack[..start_byte].chars().count();
    let chars: Vec<char> = haystack.chars().collect();
    let half = width / 2;
    let lo = start_char.saturating_sub(half);
    let hi = (lo + width).min(chars.len());
    chars.into_iter().skip(lo).take(hi - lo).collect()
}

#[tool_router(server_handler)]
impl MeshService {
    #[tool(
        name = "project.list",
        description = "List projects subject to a predicate filter. Every argument is optional; an empty arg set returns every project in the corpus, sorted by `created_at` ASC. Filters AND-together.\n\nFilters: `status` is a list of statuses (`planning` | `active` | `paused` | `done` | `abandoned`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the project's description body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` (default `created_at`); `desc: true` reverses (default ascending). `limit` caps the rows.\n\nReturns metadata only — call `project.get` for the full description body."
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
        description = "Get one project by slug, including phases, tasks, artifacts, and full description body."
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
        description = "Mark a task done. Sole entry into 'done' status. Task must be in 'in_progress'; stamps completed_at and bumps updated_at. Optionally appends a closing note."
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
        description = "List phases subject to a predicate filter. Phase bodies are included.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. A cross-project listing groups by project, then by `order` within each project, so the linear-position ordering stays meaningful.\n\nFilters: `status` is a list (`pending` | `active` | `done` | `skipped`) — OR-of-statuses. `body_contains` is a case-insensitive literal substring against the phase body. `created_after` / `created_before` and `updated_after` / `updated_before` are RFC 3339 timestamps; `_after` is inclusive (>=), `_before` is exclusive (<). Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `order` (default `order` — the linear-position frontmatter field). `desc: true` reverses (default ascending). `limit` caps the rows. Filters AND-together."
    )]
    async fn phase_list(
        &self,
        Parameters(args): Parameters<PhaseListArgs>,
    ) -> Result<Json<PhaseListResult>, ErrorData> {
        let filter = PhaseListFilter::from(args);
        let phases = self
            .store
            .list_phases(filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
        Ok(Json(PhaseListResult { phases }))
    }

    #[tool(
        name = "task.list",
        description = "List tasks subject to a predicate filter. Every argument is optional; an empty arg set returns every task in the corpus, sorted by `created_at` ASC. Filters AND-together.\n\nCross-project: `project` is optional — omit it (or pass `null`) to scan every project in the corpus. `phase` is a phase slug and REQUIRES `project` (validation error otherwise — phase slugs are unique per project, not globally).\n\nFilters: `status` is a list (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`) — OR-of-statuses. `assignee` is an exact match against the task's `assignee` frontmatter (e.g. `human:michael`, `ship`). `body_contains` is a case-insensitive literal substring against the task body. The four date-range pairs — `created`, `updated`, `completed`, `claimed` — each take `_after` (inclusive, >=) and `_before` (exclusive, <) RFC 3339 timestamps. Filtering on `completed_*` or `claimed_*` drops rows where that timestamp is null. Malformed timestamps are rejected.\n\nOrdering: `order_by` is `created_at` | `updated_at` | `completed_at` | `claimed_at` (default `created_at`); sorting by a nullable field (`completed_at`, `claimed_at`) drops rows where that field is null. `desc: true` reverses (default ascending). `limit` caps the rows."
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
        let filter = TaskListFilter::from(args);
        let tasks = self
            .store
            .list_tasks(filter)
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|v| v.value)
            .collect();
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
        description = "Unified case-insensitive literal substring search across project titles + description bodies, phase titles + bodies, and task titles + spec bodies (not notes, assignee, or other frontmatter). One call returns a single ranked list so the model can pick rows to open — use this instead of three `body_contains` list round-trips when you don't already know the primitive kind. Each hit includes `score` overlapping literal match count in title+newline+body (higher is stronger), `snippet` (~80 characters centered on the first match, no markdown awareness), and rows are ordered by `score` descending then `updated_at` descending; `limit` (default 50) applies after sorting. `kinds` filters to one or more of `project` | `phase` | `task` (default: all). `project` restricts to one project slug; omit or null for the whole corpus. Empty `query` is rejected; no matches returns an empty list. Prefer list verbs with `body_contains` when you already know you're only looking for tasks (or phases, projects)."
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
/// Bounded retry budget for concurrent `phase.add` writers racing on one project.
const PHASE_ADD_MAX_RETRIES: u32 = 8;

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
        } = args;
        let body = body.clone();
        let note = note.clone();
        let depends_on = depends_on.clone();
        let actor = actor.clone();
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
            task = apply_complete_task(task, now)?;
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

    /// Service-layer `phase.add` — project.md CAS gate, domain order compute, trait shift.
    pub async fn add_phase(&self, args: &NewPhase) -> Result<Phase, StoreError> {
        if args.project.is_empty() {
            return Err(invalid_msg("project is required"));
        }
        if !is_valid_slug(&args.project) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.project
            )));
        }
        if args.actor.is_empty() {
            return Err(invalid_msg("actor is required to add a phase"));
        }
        if args.owner.is_empty() {
            return Err(invalid_msg("owner is required to add a phase"));
        }
        if !is_valid_slug(&args.slug) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            )));
        }

        for _ in 0..PHASE_ADD_MAX_RETRIES {
            match self.try_add_phase_once(args).await {
                Ok(phase) => return Ok(phase),
                Err(StoreError::Conflict) => {}
                Err(e) => return Err(e),
            }
        }
        Err(invalid_msg("phase add failed: too many concurrent writers"))
    }

    async fn try_add_phase_once(&self, args: &NewPhase) -> Result<Phase, StoreError> {
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

        let existing = self
            .store
            .list_phases(PhaseListFilter {
                project: Some(args.project.clone()),
                ..Default::default()
            })
            .await?;
        let phases: Vec<Phase> = existing.into_iter().map(|v| v.value).collect();
        if phases.iter().any(|p| p.slug == args.slug) {
            return Err(invalid_msg(format!(
                "phase slug already exists in project: {}",
                args.slug
            )));
        }

        let new_order = compute_new_phase_order(&phases, args.after_phase.as_deref())
            .map_err(|e| invalid_msg(e.to_string()))?;

        let mut project_gate = project.clone();
        project_gate.updated_at = now_utc();
        self.store
            .put_project(&project_gate, Some(project_version))
            .await?;

        // If put_phase (below) fails after shift_phases succeeds, existing phases
        // are renumbered but new_order has no occupant until the next successful
        // add. The write_lock prevents concurrent observation of the gap; a crash
        // between the two leaves it until a later write reconciles.
        self.store.shift_phases(&args.project, new_order).await?;

        let now = now_utc();
        let phase = Phase {
            id: new_id("phs"),
            project: project.id,
            slug: args.slug.clone(),
            title: args.title.clone(),
            body: args.body.clone(),
            order: new_order,
            status: PhaseStatus::Pending,
            created_at: now,
            updated_at: now,
            created_by: args.actor.clone(),
            owner: args.owner.clone(),
        };
        self.store.put_phase(&phase, None).await?;
        Ok(phase)
    }

    /// Service-layer `phase.update` — CAS on the phase object.
    pub async fn update_phase(&self, args: UpdatePhase) -> Result<Phase, StoreError> {
        if args.project.is_empty() || args.slug.is_empty() {
            return Err(invalid_msg("project and slug are required"));
        }
        if !is_valid_slug(&args.project) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.project
            )));
        }
        let Versioned {
            value: mut phase,
            version,
        } = match self.store.get_phase(&args.project, &args.slug).await {
            Ok(v) => v,
            Err(StoreError::NotFound) => {
                return Err(invalid_msg(format!(
                    "phase not found: {}/{}",
                    args.project, args.slug
                )));
            }
            Err(e) => return Err(e),
        };
        if let Some(title) = args.title {
            phase.title = title;
        }
        if let Some(body) = args.body {
            phase.body = body;
        }
        if let Some(status) = args.status {
            phase.status = status;
        }
        if let Some(owner) = args.owner {
            if owner.is_empty() {
                return Err(invalid_msg("owner must not be empty"));
            }
            phase.owner = owner;
        }
        phase.updated_at = now_utc();
        self.store.put_phase(&phase, Some(version)).await?;
        Ok(phase)
    }

    /// Service-layer `artifact.link` — validates inputs, idempotent on (task, kind, ref).
    pub async fn link_artifact(&self, args: LinkArtifact) -> Result<Artifact, StoreError> {
        self.link_artifact_outcome(args)
            .await
            .map(|(artifact, _existed)| artifact)
    }

    /// Like [`link_artifact`](Self::link_artifact), but also reports whether an
    /// existing row matched (`true`) versus a new row appended (`false`).
    pub async fn link_artifact_outcome(
        &self,
        args: LinkArtifact,
    ) -> Result<(Artifact, bool), StoreError> {
        if args.actor.is_empty() {
            return Err(invalid_msg("actor is required to link an artifact"));
        }
        if args.project.is_empty() {
            return Err(invalid_msg("project is required"));
        }
        if !is_valid_slug(&args.project) {
            return Err(invalid_msg(format!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.project
            )));
        }
        if args.kind.is_empty() {
            return Err(invalid_msg("kind is required"));
        }
        if args.reference.is_empty() {
            return Err(invalid_msg("ref is required"));
        }
        if args.label.is_empty() {
            return Err(invalid_msg("label is required"));
        }
        for (field, value) in [
            ("kind", &args.kind),
            ("ref", &args.reference),
            ("label", &args.label),
            ("actor", &args.actor),
        ] {
            validate_single_line(field, value).map_err(|e| domain_err(&e))?;
        }

        let Versioned { value: project, .. } = match self.store.get_project(&args.project).await {
            Ok(v) => v,
            Err(StoreError::NotFound) => {
                return Err(invalid_msg(format!("project not found: {}", args.project)));
            }
            Err(e) => return Err(e),
        };

        let task_id = match &args.task {
            Some(task_id) if task_id.is_empty() => {
                return Err(invalid_msg(
                    "task is empty (omit the field entirely for a project-wide artifact)",
                ));
            }
            Some(task_id) => {
                let tasks = self
                    .store
                    .list_tasks(TaskListFilter {
                        project: Some(args.project.clone()),
                        ..Default::default()
                    })
                    .await?;
                if !tasks.iter().any(|t| t.value.id == *task_id) {
                    return Err(invalid_msg(format!(
                        "task {task_id} not found in project {}",
                        args.project
                    )));
                }
                task_id.clone()
            }
            None => String::new(),
        };

        let existing = self
            .store
            .list_artifacts(ArtifactListFilter {
                project: args.project.clone(),
            })
            .await?;
        if let Some(artifact) = existing
            .into_iter()
            .find(|a| a.task == task_id && a.kind == args.kind && a.reference == args.reference)
        {
            return Ok((artifact, true));
        }

        let now = now_utc();
        let artifact = Artifact {
            id: new_id("art"),
            project: project.id,
            task: task_id,
            kind: args.kind,
            reference: args.reference,
            label: args.label,
            linked_at: now,
            actor: args.actor,
        };
        self.store.put_artifact(&artifact).await?;
        Ok((artifact, false))
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
    use crate::domain::{SearchArgs, SearchKind, TaskGetArgs};
    use crate::store::{NewPhase, NewProject, NewTask, StoreError, UpdateTask};
    use rmcp::model::ErrorCode;
    use std::path::{Path, PathBuf};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn search_hits(svc: &MeshService, args: SearchArgs) -> Vec<SearchHit> {
        let Json(result) = block_on(svc.search(Parameters(args))).expect("search");
        result.hits
    }

    fn seed_store(tmp: &Path) -> FsStore {
        FsStore::open(tmp).expect("open seed store")
    }

    fn seed_project(svc: &MeshService, slug: &str) -> Project {
        block_on(svc.create_project(NewProject {
            slug: slug.to_owned(),
            title: format!("Project {slug}"),
            description: String::new(),
            actor: "human:test".to_owned(),
        }))
        .expect("seed project")
    }

    fn seed_task(svc: &MeshService, project: &str, slug: &str) -> Task {
        block_on(svc.create_task(NewTask {
            project: project.to_owned(),
            phase: None,
            slug: slug.to_owned(),
            title: slug.to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed task")
    }

    fn task_file_path(corpus: &Path, task: &Task) -> PathBuf {
        corpus
            .join("projects")
            .join(&task.project_slug)
            .join("tasks")
            .join(format!("{}-{}.md", task.id, task.slug))
    }

    fn set_task_field(corpus: &Path, task: &Task, field: &str, value: &str) {
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

    fn set_task_body(corpus: &Path, task: &Task, body: &str) {
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

    fn fresh_service() -> (tempfile::TempDir, MeshService) {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let store = FsStore::open(tmp.path()).expect("open fresh corpus");
        let service = MeshService::new(store);
        (tmp, service)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(future)
    }

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
    fn task_complete_on_todo_returns_invalid_params() {
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
        })))
        .expect("task.create");
        match block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: task.id,
            note: None,
            actor: "human:test".to_owned(),
        }))) {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
                assert!(
                    err.message.contains("in_progress"),
                    "unexpected message: {}",
                    err.message
                );
            }
            Ok(_) => panic!("todo cannot complete"),
        }
    }

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
        })))
        .expect("task.create");
        match block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: task.id,
            body: None,
            status: Some(TaskStatus::Done),
            note: None,
            actor: "human:test".to_owned(),
            depends_on: None,
        }))) {
            Err(err) => assert_eq!(err.code, ErrorCode::INVALID_PARAMS),
            Ok(_) => panic!("done via update must be rejected"),
        }
    }

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
        match block_on(svc.search(Parameters(args))) {
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
    fn dogfood_search_smoke_against_real_corpus() {
        let store = FsStore::open(repo_root()).expect("open corpus");
        let svc = MeshService::new(store);

        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "dossier".to_owned(),
                ..Default::default()
            },
        );
        assert!(!hits.is_empty(), "expected at least one 'dossier' hit");

        let none = search_hits(
            &svc,
            SearchArgs {
                query: "DEFINITELY-NOT-IN-CORPUS-XYZ-9999".to_owned(),
                ..Default::default()
            },
        );
        assert!(none.is_empty());
    }

    #[test]
    fn search_filters_in_temp_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));

        block_on(svc.create_project(NewProject {
            slug: "alpha".to_owned(),
            title: "alpha needle project".to_owned(),
            description: "needle".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed alpha project");
        block_on(svc.create_project(NewProject {
            slug: "beta".to_owned(),
            title: "beta needle project".to_owned(),
            description: "needle".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed beta project");
        block_on(svc.add_phase(&NewPhase {
            project: "alpha".to_owned(),
            slug: "ph".to_owned(),
            title: "phase has needle".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "t".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("seed alpha phase");
        block_on(svc.create_task(NewTask {
            project: "alpha".to_owned(),
            phase: None,
            slug: "ta".to_owned(),
            title: "task has needle".to_owned(),
            body: String::new(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed alpha task");
        block_on(svc.create_task(NewTask {
            project: "beta".to_owned(),
            phase: None,
            slug: "tb".to_owned(),
            title: "task has needle".to_owned(),
            body: String::new(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed beta task");

        let all = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                ..Default::default()
            },
        );
        let projects: std::collections::HashSet<_> =
            all.iter().map(|h| h.project.clone()).collect();
        assert_eq!(projects.len(), 2);

        let tasks = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|h| h.kind == SearchKind::Task));

        let alpha_only = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                ..Default::default()
            },
        );
        assert!(!alpha_only.is_empty());
        assert!(alpha_only.iter().all(|h| h.project == "alpha"));

        let alpha_tasks = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert!(!alpha_tasks.is_empty());
        assert!(alpha_tasks
            .iter()
            .all(|h| h.project == "alpha" && h.kind == SearchKind::Task));
    }

    #[test]
    fn search_title_and_body_hits_in_temp_corpus() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));

        block_on(svc.create_project(NewProject {
            slug: "p1".to_owned(),
            title: "TITLEKEY-alpha project".to_owned(),
            description: "minimal".to_owned(),
            actor: "t".to_owned(),
        }))
        .expect("seed project");
        block_on(svc.add_phase(&NewPhase {
            project: "p1".to_owned(),
            slug: "ph1".to_owned(),
            title: "TITLEKEY-beta phase".to_owned(),
            body: "body has BODYKEY-gamma token".to_owned(),
            after_phase: None,
            actor: "t".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("seed phase");
        block_on(svc.create_task(NewTask {
            project: "p1".to_owned(),
            phase: Some("ph1".to_owned()),
            slug: "tsk".to_owned(),
            title: "TITLEKEY-delta task".to_owned(),
            body: "BODYKEY-epsilon in spec".to_owned(),
            actor: "t".to_owned(),
            depends_on: Vec::new(),
        }))
        .expect("seed task");

        let t_alpha = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-alpha".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_alpha.len(), 1);
        assert_eq!(t_alpha[0].kind, SearchKind::Project);

        let t_beta = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-beta".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_beta.len(), 1);
        assert_eq!(t_beta[0].kind, SearchKind::Phase);

        let t_gamma = search_hits(
            &svc,
            SearchArgs {
                query: "bodykey-gamma".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_gamma.len(), 1);
        assert_eq!(t_gamma[0].kind, SearchKind::Phase);

        let t_delta = search_hits(
            &svc,
            SearchArgs {
                query: "titlekey-delta".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_delta.len(), 1);
        assert_eq!(t_delta[0].kind, SearchKind::Task);

        let t_eps = search_hits(
            &svc,
            SearchArgs {
                query: "bodykey-epsilon".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(t_eps.len(), 1);
        assert_eq!(t_eps[0].kind, SearchKind::Task);

        let uni = search_hits(
            &svc,
            SearchArgs {
                query: "KEY-".to_owned(),
                ..Default::default()
            },
        );
        let kinds: std::collections::HashSet<_> = uni.iter().map(|h| h.kind).collect();
        assert_eq!(kinds.len(), 3);
    }

    #[test]
    fn search_ranks_higher_score_before_lower() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let a = seed_task(&svc, "alpha", "one-match");
        let b = seed_task(&svc, "alpha", "triple");
        set_task_body(tmp.path(), &a, "needleonce");
        set_task_body(tmp.path(), &b, "needleneedleneedle");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "triple");
        assert_eq!(hits[0].score as i64, 3);
        assert_eq!(hits[1].score as i64, 1);
    }

    #[test]
    fn search_tiebreaks_by_updated_at_desc() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let a = seed_task(&svc, "alpha", "oldhit");
        let b = seed_task(&svc, "alpha", "newhit");
        set_task_body(tmp.path(), &a, "sameneedle");
        set_task_body(tmp.path(), &b, "sameneedle");
        set_task_field(tmp.path(), &a, "updated_at", "2026-01-01T00:00:00Z");
        set_task_field(tmp.path(), &b, "updated_at", "2026-06-01T00:00:00Z");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "sameneedle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                ..Default::default()
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "newhit");
        assert_eq!(hits[1].slug, "oldhit");
    }

    #[test]
    fn search_limit_after_sort() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        for i in 0..5 {
            let slug = format!("t{i}");
            let task = seed_task(&svc, "alpha", &slug);
            set_task_body(tmp.path(), &task, &format!("{} needle", "x".repeat(i + 1)));
            set_task_field(
                tmp.path(),
                &task,
                "updated_at",
                &format!("2026-05-{:02}T00:00:00Z", 10 + i),
            );
        }
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "needle".to_owned(),
                project: Some("alpha".to_owned()),
                kinds: Some(vec![SearchKind::Task]),
                limit: Some(2),
            },
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].slug, "t4");
        assert_eq!(hits[1].slug, "t3");
    }

    #[test]
    fn search_notes_section_not_indexed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let _store = seed_store(tmp.path());
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open service store"));
        seed_project(&svc, "alpha");
        let task = seed_task(&svc, "alpha", "n");
        set_task_body(tmp.path(), &task, "spec has no secret");
        block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: Some("note about zzzuniquezzz term".to_owned()),
            actor: "t".to_owned(),
            depends_on: None,
        }))
        .expect("append note");
        let hits = search_hits(
            &svc,
            SearchArgs {
                query: "zzzuniquezzz".to_owned(),
                project: Some("alpha".to_owned()),
                ..Default::default()
            },
        );
        assert!(
            hits.is_empty(),
            "notes must not be searchable, got {hits:?}"
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn artifact_list_filters_combine_task_and_kind_predicates() {
        let (_tmp, svc) = fresh_service();

        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "corp".to_owned(),
            title: "Corp".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("create project");

        let Json(t1) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "corp".to_owned(),
            phase: None,
            slug: "t1".to_owned(),
            title: "T1".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
        })))
        .expect("t1");

        let Json(t2) = block_on(svc.task_create(Parameters(TaskCreateArgs {
            project: "corp".to_owned(),
            phase: None,
            slug: "t2".to_owned(),
            title: "T2".to_owned(),
            body: String::new(),
            actor: "human:test".to_owned(),
            depends_on: Vec::new(),
        })))
        .expect("t2");

        let tid1 = t1.id;
        let tid2 = t2.id;

        let Json(Artifact { id: id_a1, .. }) =
            block_on(svc.artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid1.clone()),
                kind: "pr".to_owned(),
                reference: "https://example/pr/1".to_owned(),
                label: "art1".to_owned(),
                actor: "human:test".to_owned(),
            })))
            .expect("art1");

        let Json(Artifact { id: id_a2, .. }) =
            block_on(svc.artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid2),
                kind: "pr".to_owned(),
                reference: "https://example/pr/2".to_owned(),
                label: "art2".to_owned(),
                actor: "human:test".to_owned(),
            })))
            .expect("art2");

        let Json(Artifact { id: id_a3, .. }) =
            block_on(svc.artifact_link(Parameters(ArtifactLinkArgs {
                project: "corp".to_owned(),
                task: Some(tid1.clone()),
                kind: "commit".to_owned(),
                reference: "deadbeef".to_owned(),
                label: "art3".to_owned(),
                actor: "human:test".to_owned(),
            })))
            .expect("art3");

        macro_rules! ids {
            ($args:expr) => {{
                let Json(out) =
                    block_on(svc.artifact_list(Parameters($args))).expect("artifact_list");
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
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: None,
            actor: "human:test".to_owned(),
            depends_on: Some(vec!["tsk_c".into()]),
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
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: None,
            actor: "human:test".to_owned(),
            depends_on: Some(vec![]),
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
        }))
        .expect("create task");

        let updated = block_on(svc.update_task(UpdateTask {
            id: task.id,
            body: None,
            status: None,
            note: Some("progress note".to_owned()),
            actor: "human:test".to_owned(),
            depends_on: None,
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

    fn add_phase_simple(svc: &MeshService, project: &str, slug: &str) -> Phase {
        block_on(svc.add_phase(&NewPhase {
            project: project.to_owned(),
            slug: slug.to_owned(),
            title: slug.to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("add phase")
    }

    #[test]
    fn add_phase_inserts_after_and_shifts_without_orphans() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open corpus"));
        seed_project(&svc, "alpha");
        add_phase_simple(&svc, "alpha", "spec");
        add_phase_simple(&svc, "alpha", "build");
        add_phase_simple(&svc, "alpha", "ship");

        let inserted = block_on(svc.add_phase(&NewPhase {
            project: "alpha".to_owned(),
            slug: "design".to_owned(),
            title: "design".to_owned(),
            body: String::new(),
            after_phase: Some("spec".to_owned()),
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect("insert after spec");
        assert_eq!(inserted.order, 2);

        let phases_dir = tmp.path().join("projects").join("alpha").join("phases");
        let mut names: Vec<String> = std::fs::read_dir(&phases_dir)
            .expect("read phases dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "01-spec.md".to_owned(),
                "02-design.md".to_owned(),
                "03-build.md".to_owned(),
                "04-ship.md".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn add_phase_cas_race_two_independent_writers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let path = tmp.path();

        let svc_a = MeshService::new(FsStore::open(path).expect("writer a"));
        svc_a
            .create_project(NewProject {
                slug: "race".to_owned(),
                title: "Race".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .await
            .expect("create project");
        let svc_b = MeshService::new(FsStore::open(path).expect("writer b"));

        let args_a = NewPhase {
            project: "race".to_owned(),
            slug: "phase-a".to_owned(),
            title: "Phase A".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        };
        let args_b = NewPhase {
            project: "race".to_owned(),
            slug: "phase-b".to_owned(),
            title: "Phase B".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        };

        let phase_a = svc_a.add_phase(&args_a).await.expect("writer a lands");
        assert_eq!(phase_a.order, 1);

        let phase_b = svc_b
            .add_phase(&args_b)
            .await
            .expect("writer b retry lands");
        assert_eq!(phase_b.order, 2);
        assert_ne!(phase_a.order, phase_b.order);
    }

    #[tokio::test]
    async fn add_phase_cas_project_gate_conflict_then_retry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let svc = MeshService::new(FsStore::open(tmp.path()).expect("open corpus"));
        svc.create_project(NewProject {
            slug: "cas".to_owned(),
            title: "CAS".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })
        .await
        .expect("create");

        let phase_a = svc
            .add_phase(&NewPhase {
                project: "cas".to_owned(),
                slug: "a".to_owned(),
                title: "A".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .await
            .expect("add a");
        let phase_b = svc
            .add_phase(&NewPhase {
                project: "cas".to_owned(),
                slug: "b".to_owned(),
                title: "B".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .await
            .expect("add b");
        assert_ne!(phase_a.order, phase_b.order);
    }

    const INVALID_PROJECT_SLUG: &str = "Bad-Slug";

    fn assert_rejects_invalid_project_slug(err: StoreError, slug: &str) {
        let StoreError::Invalid(msg) = err else {
            panic!("expected invalid error, got {err:?}");
        };
        assert!(
            msg.contains("slug must be lowercase ascii"),
            "unexpected message: {msg}"
        );
        assert!(msg.contains(slug), "message should include slug: {msg}");
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

    #[test]
    fn add_phase_rejects_invalid_project_slug() {
        let (_tmp, svc) = fresh_service();
        let err = block_on(svc.add_phase(&NewPhase {
            project: INVALID_PROJECT_SLUG.to_owned(),
            slug: "valid-phase".to_owned(),
            title: "Phase".to_owned(),
            body: String::new(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:test".to_owned(),
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }

    #[test]
    fn update_phase_rejects_invalid_project_slug() {
        let (_tmp, svc) = fresh_service();
        let err = block_on(svc.update_phase(UpdatePhase {
            project: INVALID_PROJECT_SLUG.to_owned(),
            slug: "spec".to_owned(),
            ..Default::default()
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }

    #[test]
    fn link_artifact_rejects_invalid_project_slug() {
        let (_tmp, svc) = fresh_service();
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: INVALID_PROJECT_SLUG.to_owned(),
            task: None,
            kind: "pr".to_owned(),
            reference: "https://example.com/pr/1".to_owned(),
            label: "PR".to_owned(),
            actor: "human:test".to_owned(),
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }
}
