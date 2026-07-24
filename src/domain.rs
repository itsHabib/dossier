//! Plain types and pure policy of the Agent Project Protocol. No I/O.
//!
//! Types derive `Serialize + Deserialize + JsonSchema` for the MCP surface;
//! transition, validation, id minting, and phase-order helpers are pure
//! functions both `FsStore` and future backends delegate to.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

/// Maximum number of keys in an artifact `meta` map.
pub const ARTIFACT_META_MAX_KEYS: usize = 16;
/// Maximum UTF-8 byte length of a `meta` key.
pub const ARTIFACT_META_MAX_KEY_LEN: usize = 64;
/// Maximum UTF-8 byte length of a `meta` value.
pub const ARTIFACT_META_MAX_VALUE_LEN: usize = 512;
/// Maximum serialized JSON byte length of the entire `meta` object.
pub const ARTIFACT_META_MAX_SERIALIZED_LEN: usize = 4096;

/// Lifecycle status of a project (`planning` | `active` | `paused` | `done` | `abandoned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Planning,
    Active,
    Paused,
    Done,
    Abandoned,
}

impl ProjectStatus {
    /// `true` for finished lifecycles (`done`, `abandoned`); `paused` is live —
    /// it is still yours and belongs in a default portfolio list.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Abandoned)
    }

    /// The non-terminal statuses, in declaration order. Injected as the
    /// default `status` filter when a list verb is called without one.
    pub fn live_statuses() -> Vec<Self> {
        vec![Self::Planning, Self::Active, Self::Paused]
    }
}

/// Lifecycle status of a phase (`pending` | `active` | `done` | `skipped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Active,
    Done,
    Skipped,
}

impl PhaseStatus {
    /// `true` for finished lifecycles (`done`, `skipped`).
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }

    /// The non-terminal statuses, in declaration order. Injected as the
    /// default `status` filter when a list verb is called without one.
    pub fn live_statuses() -> Vec<Self> {
        vec![Self::Pending, Self::Active]
    }
}

/// Task state-machine status (`todo` | `claimed` | `in_progress` | `blocked` | `done` | `cancelled`).
/// `claimed` and `done` are set only via `task.claim` / `task.complete`, not `task.update`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Todo,
    Claimed,
    InProgress,
    Blocked,
    Done,
    Cancelled,
}

impl TaskStatus {
    /// `true` for finished lifecycles (`done`, `cancelled`); `blocked` is live —
    /// stuck is not finished, and you want to see what is blocked.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Cancelled)
    }

    /// The non-terminal statuses, in declaration order. Injected as the
    /// default `status` filter when a list verb is called without one.
    pub fn live_statuses() -> Vec<Self> {
        vec![Self::Todo, Self::Claimed, Self::InProgress, Self::Blocked]
    }
}

/// Resolve a list verb's effective `status` filter under the live-by-default policy.
///
/// The single source of truth shared by the MCP `From<…Args>` conversions and
/// the `dossier task_list` CLI, so the two surfaces never diverge.
///
/// Explicit `status` always wins verbatim — including `Some(vec![])`, which the
/// store matcher treats as "no filter" => ALL rows incl. terminal (intentional,
/// not a bug). Only when `status` is omitted does `include_terminal` govern:
/// `Some(true)` => `None` (all statuses); otherwise inject the live set.
pub fn resolve_status<S>(
    status: Option<Vec<S>>,
    include_terminal: Option<bool>,
    live: impl FnOnce() -> Vec<S>,
) -> Option<Vec<S>> {
    if let Some(explicit) = status {
        return Some(explicit);
    }
    if include_terminal == Some(true) {
        return None;
    }
    Some(live())
}

/// Top-level project row — metadata in frontmatter, description body on disk.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub status: ProjectStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
}

/// Design-doc phase within a project; `order` is linear position on disk.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Phase {
    pub id: String,
    pub project: String,
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub order: i32,
    pub status: PhaseStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(
        default = "default_phase_created_by",
        skip_serializing_if = "String::is_empty"
    )]
    pub created_by: String,
    #[serde(default)]
    pub owner: String,
}

fn default_phase_created_by() -> String {
    String::from("unknown")
}

/// Unit of work within a project, optionally anchored to a phase by id.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    pub id: String,
    pub project: String,
    /// Slug of the owning project; stamped by the store from the on-disk path on load and create. Empty if deserialized without going through the store.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_slug: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    pub slug: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub assignee: String,
    pub claimed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// One append-only entry in a task's `## Notes` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    pub actor: String,
    pub body: String,
    pub posted_at: DateTime<Utc>,
}

/// Artifact `kind` is a free-form string so unknown kinds round-trip.
///
/// Unknown kinds are persisted untouched, per `PROTOCOL.md`. A future
/// minor version may promote well-known kinds (`commit`, `pr`, `file`,
/// `url`, `run`, `doc`) to a typed enum while still accepting strings
/// for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Artifact {
    pub id: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task: String,
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    pub linked_at: DateTime<Utc>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
}

/// Arguments for `task.get` — fetch one task by id without project context.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TaskGetArgs {
    pub id: String,
}

/// Predicate set for `FsStore::list_tasks`. Every field is optional; an
/// empty filter returns every task. Predicates AND-together.
///
/// `project = None` walks the whole corpus; `phase` requires `project`.
/// `status` is a list (OR-of-statuses). `body_contains` is a
/// case-insensitive literal substring against `Task.body`. The four
/// date-range pairs operate on the matching frontmatter timestamp;
/// `_after` is inclusive of equal, `_before` is strictly less than.
/// `order_by` defaults to `created_at` ASC; sorting by a nullable field
/// (`completed_at`, `claimed_at`) implicitly drops rows where the field
/// is null.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TaskListFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<TaskStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<TaskOrderField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Sort key for `TaskListFilter.order_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskOrderField {
    CreatedAt,
    UpdatedAt,
    CompletedAt,
    ClaimedAt,
}

/// Predicate set for `FsStore::list_phases`. `project = None` walks the
/// whole corpus. `order_by` defaults to `order` ASC, matching the
/// linear-position semantics of phases on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PhaseListFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<PhaseStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<PhaseOrderField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Sort key for `PhaseListFilter.order_by`. `Order` references the
/// `order` frontmatter field — linear position within a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseOrderField {
    CreatedAt,
    UpdatedAt,
    Order,
}

/// Predicate set for `FsStore::list_projects`. Always corpus-scoped —
/// no parent to nullify. Default sort is `created_at` ASC.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProjectListFilter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<ProjectStatus>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<ProjectOrderField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Sort key for `ProjectListFilter.order_by`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectOrderField {
    CreatedAt,
    UpdatedAt,
}

/// Which primitive kinds `FsStore::search` includes. Omitted on `search`
/// means all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Project,
    Phase,
    Task,
}

/// Arguments for corpus-wide substring search (`search` MCP tool).
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Literal substring to find (case-insensitive); must be non-empty after trim.
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<SearchKind>>,
    /// Restrict to this project slug; omit or `null` for the whole corpus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Include hits in terminal items (default `true` — finding completed
    /// work is a core use of search). Set `false` to scope to live items only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// One row from `FsStore::search`, ranked by `score` then recency.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    pub kind: SearchKind,
    pub id: String,
    pub project: String,
    /// Phase slug when `kind` is `task`, if the task is anchored to a phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub slug: String,
    pub title: String,
    pub snippet: String,
    /// Relevance signal (v1: overlapping literal match count in title+body).
    pub score: f64,
}

/// Per-status task tally for one phase row (or the `unphased` bucket) in a
/// [`ProjectOverview`]. Every field is always present (zero when none);
/// `total` is the authoritative sum of the six status buckets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TaskStatusCounts {
    pub todo: u32,
    pub claimed: u32,
    pub in_progress: u32,
    pub blocked: u32,
    pub done: u32,
    pub cancelled: u32,
    pub total: u32,
}

impl TaskStatusCounts {
    /// Tally one task's status into the matching bucket and bump `total`.
    pub const fn add(&mut self, status: TaskStatus) {
        match status {
            TaskStatus::Todo => self.todo += 1,
            TaskStatus::Claimed => self.claimed += 1,
            TaskStatus::InProgress => self.in_progress += 1,
            TaskStatus::Blocked => self.blocked += 1,
            TaskStatus::Done => self.done += 1,
            TaskStatus::Cancelled => self.cancelled += 1,
        }
        self.total += 1;
    }
}

/// Per-status phase tally for the project-level rollup in a
/// [`ProjectOverview`]. Every field is always present (zero when none).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PhaseStatusCounts {
    pub pending: u32,
    pub active: u32,
    pub done: u32,
    pub skipped: u32,
}

impl PhaseStatusCounts {
    /// Tally one phase's status into the matching bucket.
    pub const fn add(&mut self, status: PhaseStatus) {
        match status {
            PhaseStatus::Pending => self.pending += 1,
            PhaseStatus::Active => self.active += 1,
            PhaseStatus::Done => self.done += 1,
            PhaseStatus::Skipped => self.skipped += 1,
        }
    }
}

/// One ordered phase row in a [`ProjectOverview`]: identity + status +
/// recency + per-status task counts (no bodies).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PhaseOverview {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub order: i32,
    pub status: PhaseStatus,
    pub owner: String,
    pub updated_at: DateTime<Utc>,
    pub task_counts: TaskStatusCounts,
}

/// Project-level rollups for a [`ProjectOverview`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OverviewTotals {
    pub phases_by_status: PhaseStatusCounts,
    pub tasks_by_status: TaskStatusCounts,
    pub artifact_count: u32,
}

/// The `unphased` bucket of a [`ProjectOverview`]: tasks whose `phase` is
/// empty or refers to no existing phase.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UnphasedOverview {
    pub task_counts: TaskStatusCounts,
}

/// Project identity block for a [`ProjectOverview`], carrying a bounded
/// description instead of the full project.md body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectOverviewMeta {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub status: ProjectStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created_by: String,
    /// First 600 characters of the project.md body (char-safe), bounded.
    pub description: String,
    /// `true` iff the body was clipped to fit the 600-char bound.
    pub description_truncated: bool,
}

/// Bounded "state of this project" snapshot returned by `project.overview`.
///
/// Carries project meta, an ordered phase index with per-status task counts
/// (no bodies), an `unphased` bucket, and project-level rollups.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ProjectOverview {
    pub project: ProjectOverviewMeta,
    pub phases: Vec<PhaseOverview>,
    pub unphased: UnphasedOverview,
    pub totals: OverviewTotals,
}

/// Max characters of the project description carried in a [`ProjectOverview`].
pub const OVERVIEW_DESCRIPTION_CHARS: usize = 600;

/// Char-safe truncation of `body` to [`OVERVIEW_DESCRIPTION_CHARS`]. Returns
/// the (possibly clipped) string and whether it was clipped. Never splits a
/// UTF-8 codepoint — operates on `chars`, not bytes.
pub fn truncate_description(body: &str) -> (String, bool) {
    // Single pass: take the first N chars, then peek one more to decide
    // truncation — avoids a second full `chars().count()` over a large body.
    let mut chars = body.chars();
    let out: String = chars.by_ref().take(OVERVIEW_DESCRIPTION_CHARS).collect();
    (out, chars.next().is_some())
}

// --- Write-verb argument DTOs (protocol inputs; no I/O) ---

/// Arguments for `project.create`.
#[derive(Debug, Clone)]
pub struct NewProject {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub actor: String,
}

/// Arguments for `project.update`. Slug is the addressing key; `id` and
/// `created_at` are immutable.
#[derive(Debug, Clone, Default)]
pub struct UpdateProject {
    pub slug: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<ProjectStatus>,
}

/// Arguments for `phase.add`. `after_phase` (a phase slug, optional)
/// inserts in order; default appends.
#[derive(Debug, Clone)]
pub struct NewPhase {
    pub project: String,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub after_phase: Option<String>,
    pub actor: String,
    pub owner: String,
}

/// Arguments for `phase.update`. (`project`, `slug`) is the addressing key.
#[derive(Debug, Clone, Default)]
pub struct UpdatePhase {
    pub project: String,
    pub slug: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<PhaseStatus>,
    pub owner: Option<String>,
}

/// Arguments for `task.create`.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub project: String,
    pub phase: Option<String>,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub actor: String,
    pub depends_on: Vec<String>,
}

/// Arguments for `task.claim`. Same-actor re-claim on a non-terminal task
/// is a no-op (no `updated_at` bump).
#[derive(Debug, Clone)]
pub struct ClaimTask {
    pub id: String,
    pub actor: String,
}

/// Arguments for `task.update`. `status=claimed` and `status=done` are
/// rejected — use `task.claim` or `task.complete` instead.
#[derive(Debug, Clone, Default)]
pub struct UpdateTask {
    pub id: String,
    pub body: Option<String>,
    pub status: Option<TaskStatus>,
    pub note: Option<String>,
    pub actor: String,
    pub depends_on: Option<Vec<String>>,
}

/// Arguments for `task.complete`. Completes from `in_progress`, or walks
/// `todo` / `claimed` (same actor) through claim → `in_progress` → done.
#[derive(Debug, Clone)]
pub struct CompleteTask {
    pub id: String,
    pub note: Option<String>,
    pub actor: String,
}

/// Arguments for `artifact.link`.
#[derive(Debug, Clone)]
pub struct LinkArtifact {
    pub project: String,
    pub task: Option<String>,
    pub kind: String,
    pub reference: String,
    pub label: String,
    pub actor: String,
    pub meta: BTreeMap<String, String>,
}

// --- Pure policy (no `std::fs` / I/O) ---

/// Generate a new prefixed ULID (`prj_`, `phs_`, `tsk_`, `art_`).
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Ulid::new())
}

/// Slugs are lowercase ASCII with `-` and `_` allowed.
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Compute the `order` value for a new phase given existing phases.
pub fn compute_new_phase_order(phases: &[Phase], after_phase: Option<&str>) -> Result<i32> {
    let Some(after_slug) = after_phase else {
        return Ok(phases.iter().map(|p| p.order).max().unwrap_or(0) + 1);
    };
    let after = phases
        .iter()
        .find(|p| p.slug == after_slug)
        .ok_or_else(|| anyhow::anyhow!("after_phase not found: {after_slug}"))?;
    Ok(after.order + 1)
}

/// Enforce artifact `meta` caps (key count, key/value length, total serialized size).
/// Returns an error naming the failing key when a cap is exceeded.
pub fn validate_artifact_meta(meta: &BTreeMap<String, String>) -> Result<()> {
    if meta.is_empty() {
        return Ok(());
    }
    if meta.len() > ARTIFACT_META_MAX_KEYS {
        let key = meta
            .iter()
            .nth(ARTIFACT_META_MAX_KEYS)
            .map_or("?", |(k, _)| k.as_str());
        bail!("meta key count exceeds {ARTIFACT_META_MAX_KEYS}: {key}");
    }
    for (key, value) in meta {
        if key.len() > ARTIFACT_META_MAX_KEY_LEN {
            bail!("meta key exceeds {ARTIFACT_META_MAX_KEY_LEN} bytes: {key}");
        }
        if value.len() > ARTIFACT_META_MAX_VALUE_LEN {
            bail!("meta value exceeds {ARTIFACT_META_MAX_VALUE_LEN} bytes: {key}");
        }
    }
    let serialized = serde_json::to_string(meta)?;
    if serialized.len() > ARTIFACT_META_MAX_SERIALIZED_LEN {
        let key = meta.iter().last().map_or("?", |(k, _)| k.as_str());
        bail!("meta serialized size exceeds {ARTIFACT_META_MAX_SERIALIZED_LEN} bytes: {key}");
    }
    Ok(())
}

/// Reject values that would break JSONL grep-ability or note-line parsing.
pub fn validate_single_line(field: &str, value: &str) -> Result<()> {
    if value.contains(['\n', '\r']) {
        bail!("{field} must be single-line (no newline or carriage return)");
    }
    Ok(())
}

/// Reject task body content that would collide with the `## Notes` delimiter.
pub fn validate_task_body(body: &str) -> Result<()> {
    if body.lines().any(|l| l.trim() == "## Notes") {
        bail!("task body must not contain a `## Notes` heading (reserved as the notes-section delimiter; use a different heading like `### Notes`)");
    }
    Ok(())
}

const fn task_status_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Todo => "todo",
        TaskStatus::Claimed => "claimed",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// Guard transitions reachable via `task.update`. `claimed` and `done` are
/// owned by `task.claim` and `task.complete`; terminal sources reject all
/// status writes.
pub fn validate_task_update_transition(from: TaskStatus, to: TaskStatus) -> Result<()> {
    if matches!(to, TaskStatus::Claimed) {
        bail!("use task.claim to transition into claimed");
    }
    if matches!(to, TaskStatus::Done) {
        bail!("use task.complete to transition into done");
    }
    if matches!(from, TaskStatus::Done | TaskStatus::Cancelled) {
        bail!(
            "task is in a terminal state ({}); transitions are not allowed",
            task_status_str(from)
        );
    }
    if from == to {
        return Ok(());
    }
    let allowed = matches!(
        (from, to),
        (
            TaskStatus::Todo | TaskStatus::Claimed | TaskStatus::InProgress | TaskStatus::Blocked,
            TaskStatus::Cancelled,
        ) | (
            TaskStatus::Claimed | TaskStatus::Blocked,
            TaskStatus::InProgress
        ) | (TaskStatus::InProgress, TaskStatus::Blocked)
    );
    if !allowed {
        bail!(
            "invalid task transition: {} -> {}",
            task_status_str(from),
            task_status_str(to)
        );
    }
    Ok(())
}

/// Pure `task.claim` transition. `now` is supplied by the caller (store
/// stamps wall-clock time; tests may fix it).
pub fn apply_claim_task(mut task: Task, actor: &str, now: DateTime<Utc>) -> Result<Task> {
    if actor.is_empty() {
        bail!("actor is required to claim a task");
    }
    if matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled) {
        bail!(
            "cannot claim task in terminal state: {}",
            task_status_str(task.status)
        );
    }
    if matches!(task.status, TaskStatus::Todo) && !task.assignee.is_empty() {
        bail!(
            "task in todo has assignee {} (corrupt state)",
            task.assignee
        );
    }
    if !matches!(task.status, TaskStatus::Todo) && task.assignee.is_empty() {
        bail!(
            "task in state {} has no assignee (corrupt state)",
            task_status_str(task.status)
        );
    }
    if task.assignee == actor {
        return Ok(task);
    }
    if !task.assignee.is_empty() {
        bail!("task already claimed by {}", task.assignee);
    }

    actor.clone_into(&mut task.assignee);
    task.status = TaskStatus::Claimed;
    task.claimed_at = Some(now);
    task.updated_at = now;
    Ok(task)
}

/// Apply a status change from `task.update` (validation + stamp).
pub fn apply_task_status_update(
    mut task: Task,
    to: TaskStatus,
    now: DateTime<Utc>,
) -> Result<Task> {
    validate_task_update_transition(task.status, to)?;
    task.status = to;
    task.updated_at = now;
    Ok(task)
}

/// Apply a body change from `task.update`.
pub fn apply_task_body_update(mut task: Task, body: String, now: DateTime<Utc>) -> Result<Task> {
    validate_task_body(&body)?;
    task.body = body;
    task.updated_at = now;
    Ok(task)
}

/// Append a single-line note to a task's in-memory note list. Rejects
/// multi-line input and actor strings containing the note delimiter.
pub fn append_task_note(task: &mut Task, at: DateTime<Utc>, actor: &str, body: &str) -> Result<()> {
    if actor.contains(['\n', '\r']) || body.contains(['\n', '\r']) {
        bail!("note must be single-line (no newline or carriage return)");
    }
    if actor.contains(": ") {
        bail!("actor must not contain `: ` (reserved as the actor/body delimiter in note lines)");
    }
    let body_trimmed = body.trim();
    if body_trimmed.is_empty() {
        bail!("note body must not be empty");
    }
    task.notes.push(Note {
        actor: actor.to_owned(),
        body: body_trimmed.to_owned(),
        posted_at: at,
    });
    Ok(())
}

/// Pure `task.complete` transition from `in_progress` (status + timestamps
/// only; notes stay with the mechanism layer).
pub fn apply_complete_task(mut task: Task, now: DateTime<Utc>) -> Result<Task> {
    if !matches!(task.status, TaskStatus::InProgress) {
        bail!(
            "task must be in_progress to complete (got {})",
            task_status_str(task.status)
        );
    }
    task.status = TaskStatus::Done;
    task.completed_at = Some(now);
    task.updated_at = now;
    Ok(task)
}

/// Pure `task.complete` entry point. From `todo` or `claimed` (same actor),
/// walks claim → `in_progress` → done in one stamp. Cross-actor `claimed`
/// and terminal sources reject.
pub fn apply_task_complete(mut task: Task, actor: &str, now: DateTime<Utc>) -> Result<Task> {
    if actor.is_empty() {
        bail!("actor is required to complete a task");
    }
    match task.status {
        TaskStatus::Done | TaskStatus::Cancelled => {
            bail!(
                "task is in a terminal state ({}); cannot complete",
                task_status_str(task.status)
            );
        }
        TaskStatus::InProgress => apply_complete_task(task, now),
        TaskStatus::Blocked => {
            bail!(
                "task must be in_progress to complete (got {})",
                task_status_str(task.status)
            );
        }
        TaskStatus::Todo => {
            task = apply_claim_task(task, actor, now)?;
            task = apply_task_status_update(task, TaskStatus::InProgress, now)?;
            apply_complete_task(task, now)
        }
        TaskStatus::Claimed => {
            if task.assignee != actor {
                bail!("task already claimed by {}", task.assignee);
            }
            task = apply_task_status_update(task, TaskStatus::InProgress, now)?;
            apply_complete_task(task, now)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::{
        truncate_description, PhaseStatus, ProjectStatus, TaskStatus, OVERVIEW_DESCRIPTION_CHARS,
    };

    #[test]
    fn task_is_terminal_matches_table() {
        assert!(TaskStatus::Done.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Todo.is_terminal());
        assert!(!TaskStatus::Claimed.is_terminal());
        assert!(!TaskStatus::InProgress.is_terminal());
        // `blocked` is stuck, not finished — live by design.
        assert!(!TaskStatus::Blocked.is_terminal());
    }

    #[test]
    fn task_live_statuses_are_the_non_terminal_set() {
        let live = TaskStatus::live_statuses();
        assert_eq!(
            live,
            vec![
                TaskStatus::Todo,
                TaskStatus::Claimed,
                TaskStatus::InProgress,
                TaskStatus::Blocked,
            ]
        );
        assert!(live.iter().all(|s| !s.is_terminal()));
    }

    #[test]
    fn phase_is_terminal_matches_table() {
        assert!(PhaseStatus::Done.is_terminal());
        assert!(PhaseStatus::Skipped.is_terminal());
        assert!(!PhaseStatus::Pending.is_terminal());
        assert!(!PhaseStatus::Active.is_terminal());
    }

    #[test]
    fn phase_live_statuses_are_the_non_terminal_set() {
        let live = PhaseStatus::live_statuses();
        assert_eq!(live, vec![PhaseStatus::Pending, PhaseStatus::Active]);
        assert!(live.iter().all(|s| !s.is_terminal()));
    }

    #[test]
    fn project_is_terminal_matches_table() {
        assert!(ProjectStatus::Done.is_terminal());
        assert!(ProjectStatus::Abandoned.is_terminal());
        assert!(!ProjectStatus::Planning.is_terminal());
        assert!(!ProjectStatus::Active.is_terminal());
        // a `paused` project is still yours — live by design.
        assert!(!ProjectStatus::Paused.is_terminal());
    }

    #[test]
    fn project_live_statuses_are_the_non_terminal_set() {
        let live = ProjectStatus::live_statuses();
        assert_eq!(
            live,
            vec![
                ProjectStatus::Planning,
                ProjectStatus::Active,
                ProjectStatus::Paused,
            ]
        );
        assert!(live.iter().all(|s| !s.is_terminal()));
    }

    #[test]
    fn truncate_short_description_unchanged() {
        let (out, truncated) = truncate_description("a short body");
        assert_eq!(out, "a short body");
        assert!(!truncated);
    }

    #[test]
    fn truncate_long_description_clips_to_bound() {
        let body = "x".repeat(OVERVIEW_DESCRIPTION_CHARS + 100);
        let (out, truncated) = truncate_description(&body);
        assert_eq!(out.chars().count(), OVERVIEW_DESCRIPTION_CHARS);
        assert!(truncated);
    }

    #[test]
    fn truncate_at_exact_bound_is_not_truncated() {
        let body = "x".repeat(OVERVIEW_DESCRIPTION_CHARS);
        let (out, truncated) = truncate_description(&body);
        assert_eq!(out.chars().count(), OVERVIEW_DESCRIPTION_CHARS);
        assert!(!truncated, "exactly at the bound is not clipped");
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // Multi-byte chars: clipping must count chars, never split a codepoint.
        let body = "é".repeat(OVERVIEW_DESCRIPTION_CHARS + 50);
        let (out, truncated) = truncate_description(&body);
        assert_eq!(out.chars().count(), OVERVIEW_DESCRIPTION_CHARS);
        assert!(truncated);
        // Round-trips as valid UTF-8 (String guarantees it; assert no partial char).
        assert!(out.chars().all(|c| c == 'é'));
    }
}
