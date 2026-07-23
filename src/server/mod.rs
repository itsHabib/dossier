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
    compute_new_phase_order, is_valid_slug, new_id, resolve_status, truncate_description, Artifact,
    OverviewTotals, Phase, PhaseListFilter, PhaseOrderField, PhaseOverview, PhaseStatus,
    PhaseStatusCounts, Project, ProjectListFilter, ProjectOrderField, ProjectOverview,
    ProjectOverviewMeta, ProjectStatus, SearchArgs, SearchHit, SearchKind, Task, TaskGetArgs,
    TaskListFilter, TaskStatusCounts, UnphasedOverview,
};
use crate::store::{
    now_utc, ArtifactListFilter, ClaimTask, CompleteTask, FsStore, LinkArtifact, NewPhase,
    NewProject, NewTask, Store, StoreError, UpdatePhase, UpdateProject, UpdateTask, Versioned,
};

pub mod artifact;
pub mod task;
#[cfg(test)]
pub mod testutil;

use artifact::{ArtifactLinkArgs, ArtifactListArgs, ArtifactListResult};
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

/// Predicate-shaped arguments for `phase.list`. `project = None`
/// (omitted or explicit `null`) walks every project in the corpus.
#[derive(Deserialize, JsonSchema, Default)]
pub struct PhaseListArgs {
    /// project slug; omit or pass `null` to list phases across every project
    #[serde(default)]
    pub project: Option<String>,
    /// if set, only phases whose status is in this list
    /// (`pending` | `active` | `done` | `skipped`).
    /// Omit `status` for the live-only default (non-terminal rows); pass
    /// `include_terminal: true` to include terminal rows. An explicit list
    /// selects exact statuses; an explicit empty `[]` is "no filter" (all
    /// statuses) — distinct from omitting.
    #[serde(default)]
    pub status: Option<Vec<PhaseStatus>>,
    /// when `status` is omitted, default to live (non-terminal) phases only.
    /// Set `true` to include terminal (`done`, `skipped`) rows; ignored when
    /// an explicit `status` is given (explicit always wins).
    #[serde(default)]
    pub include_terminal: Option<bool>,
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
    /// include phase bodies (default `true`); pass `false` to strip just the
    /// body markdown (all frontmatter is still returned) for a bounded read
    #[serde(default)]
    pub bodies: Option<bool>,
}

/// Response envelope for `phase.list`.
#[derive(Serialize, JsonSchema)]
pub struct PhaseListResult {
    pub phases: Vec<Phase>,
}

/// Response envelope for `search`.
#[derive(Serialize, JsonSchema)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
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

impl From<PhaseListArgs> for PhaseListFilter {
    fn from(a: PhaseListArgs) -> Self {
        Self {
            project: a.project,
            status: resolve_status(a.status, a.include_terminal, PhaseStatus::live_statuses),
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
        // Search defaults to all statuses (finding completed work is core,
        // D4); `include_terminal: false` scopes the walk to live items. This
        // is applied here during the scan — never through the list-verb `From`
        // conversion — so the all-statuses store reads below are unaffected.
        let include_terminal = args.include_terminal.unwrap_or(true);

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
                // Item-level filtering: the project hit is gated inline (not an
                // early `continue`) because a terminal project can still hold
                // live phases/tasks that must be searched in the same iteration.
                let live_kept = include_terminal || !p.status.is_terminal();
                if score > 0 && live_kept {
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
                    if !include_terminal && ph.status.is_terminal() {
                        continue;
                    }
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
                    if !include_terminal && t.status.is_terminal() {
                        continue;
                    }
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

/// Aggregate a project's phases, tasks, and artifact count into a bounded
/// [`ProjectOverview`]. Pure policy over already-loaded rows: partitions
/// tasks across phase rows (joined by `task.phase == phase.id`) and an
/// `unphased` bucket (empty or dangling phase id), tallies per-status
/// counts, and bounds the description. No bodies or notes are read.
fn build_overview(
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

    use super::testutil::{
        assert_rejects_invalid_project_slug, block_on, fresh_service, repo_root, seed_project,
        seed_store, seed_task, set_task_body, set_task_field, task_file_path, INVALID_PROJECT_SLUG,
    };
    use super::*;
    use crate::domain::{SearchArgs, SearchKind, TaskStatus};
    use crate::store::{NewPhase, NewProject, NewTask, UpdateTask};
    use rmcp::model::ErrorCode;
    use std::path::{Path, PathBuf};

    fn search_hits(svc: &MeshService, args: SearchArgs) -> Vec<SearchHit> {
        let Json(result) = block_on(svc.search(Parameters(args))).expect("search");
        result.hits
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
                include_terminal: None,
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
    fn phase_list_bodies_false_strips_body() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        block_on(svc.phase_add(Parameters(PhaseAddArgs {
            project: "alpha".to_owned(),
            slug: "p1".to_owned(),
            title: "Phase 1".to_owned(),
            body: "secret phase body".to_owned(),
            after_phase: None,
            actor: "human:test".to_owned(),
            owner: "human:p1".to_owned(),
        })))
        .expect("p1");

        // Default keeps the body.
        let Json(full) = block_on(svc.phase_list(Parameters(PhaseListArgs {
            project: Some("alpha".to_owned()),
            ..Default::default()
        })))
        .expect("phase.list default");
        assert_eq!(full.phases[0].body, "secret phase body");

        // bodies: false strips it.
        let Json(stripped) = block_on(svc.phase_list(Parameters(PhaseListArgs {
            project: Some("alpha".to_owned()),
            bodies: Some(false),
            ..Default::default()
        })))
        .expect("phase.list bodies false");
        assert!(stripped.phases[0].body.is_empty());
        assert_eq!(stripped.phases[0].slug, "p1");
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
            include_terminal: None,
            body_contains: Some("phase-body".to_owned()),
            created_after: Some(created_after),
            created_before: Some(created_before),
            updated_after: Some(updated_after),
            updated_before: Some(updated_before),
            order_by: Some(PhaseOrderField::CreatedAt),
            desc: Some(true),
            limit: Some(42),
            bodies: None,
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

    fn phases_listed(svc: &MeshService, args: PhaseListArgs) -> Vec<Phase> {
        let Json(result) = block_on(svc.phase_list(Parameters(args))).expect("phase.list");
        result.phases
    }

    fn projects_listed(svc: &MeshService, args: ProjectListArgs) -> Vec<Project> {
        let Json(result) = block_on(svc.project_list(Parameters(args))).expect("project.list");
        result.projects
    }

    fn complete_task(svc: &MeshService, id: &str) {
        block_on(svc.task_complete(Parameters(TaskCompleteArgs {
            id: id.to_owned(),
            note: None,
            actor: "human:test".to_owned(),
        })))
        .expect("complete task");
    }

    #[test]
    fn phase_list_defaults_to_live_only() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        add_phase_simple(&svc, "alpha", "live-phase");
        add_phase_simple(&svc, "alpha", "terminal-phase");
        block_on(svc.phase_update(Parameters(PhaseUpdateArgs {
            project: "alpha".to_owned(),
            slug: "terminal-phase".to_owned(),
            title: None,
            body: None,
            status: Some(PhaseStatus::Done),
            owner: None,
        })))
        .expect("mark phase done");

        let live = phases_listed(
            &svc,
            PhaseListArgs {
                project: Some("alpha".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(live.len(), 1, "default phase.list drops terminal phases");
        assert_eq!(live[0].slug, "live-phase");

        let all = phases_listed(
            &svc,
            PhaseListArgs {
                project: Some("alpha".to_owned()),
                include_terminal: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            all.len(),
            2,
            "include_terminal:true returns terminal phases"
        );
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

    #[test]
    fn search_includes_terminal_by_default_and_scopes_with_flag() {
        let (_tmp, svc) = fresh_service();
        seed_project(&svc, "alpha");
        let live = seed_task(&svc, "alpha", "live-needle");
        let term = seed_task(&svc, "alpha", "terminal-needle");
        // Distinct bodies so the same query matches both.
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: live.id,
            body: Some("magicword lives".to_owned()),
            status: None,
            note: None,
            depends_on: None,
            actor: "human:test".to_owned(),
        })))
        .expect("body live");
        block_on(svc.task_update(Parameters(TaskUpdateArgs {
            id: term.id.clone(),
            body: Some("magicword done".to_owned()),
            status: None,
            note: None,
            depends_on: None,
            actor: "human:test".to_owned(),
        })))
        .expect("body term");
        complete_task(&svc, &term.id);

        let default_hits = search_hits(
            &svc,
            SearchArgs {
                query: "magicword".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(
            default_hits.len(),
            2,
            "search includes terminal hits by default (D4)"
        );

        let live_only = search_hits(
            &svc,
            SearchArgs {
                query: "magicword".to_owned(),
                include_terminal: Some(false),
                ..Default::default()
            },
        );
        assert_eq!(live_only.len(), 1, "include_terminal:false scopes to live");
        assert_eq!(live_only[0].slug, "live-needle");
    }
}
