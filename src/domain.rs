//! Plain types of the Agent Project Protocol. No I/O. Every type derives
//! `Serialize + Deserialize + JsonSchema` so it can flow through the MCP
//! surface and into JSON Schema for tool descriptions.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    Planning,
    Active,
    Paused,
    Done,
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PhaseStatus {
    Pending,
    Active,
    Done,
    Skipped,
}

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
}

fn default_phase_created_by() -> String {
    String::from("unknown")
}

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
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
