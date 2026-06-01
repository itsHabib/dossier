//! Read and write the on-disk corpus described in `LAYOUT.md`.
//!
//! Single-writer assumption — concurrent `FsStore` handles against the
//! same root will eventually corrupt it. The mesh serializes writes with
//! a `std::sync::Mutex` for the within-process case; cross-process
//! coordination is out of scope.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

pub use crate::domain::{
    apply_claim_task, apply_complete_task, apply_task_body_update, apply_task_status_update,
    compute_new_phase_order, is_valid_slug, new_id, validate_single_line, validate_task_body,
    validate_task_update_transition, Artifact, ClaimTask, CompleteTask, LinkArtifact, NewPhase,
    NewProject, NewTask, Phase, PhaseListFilter, PhaseOrderField, PhaseStatus, Project,
    ProjectListFilter, ProjectOrderField, ProjectStatus, Task, TaskListFilter, TaskOrderField,
    TaskStatus, UpdatePhase, UpdateProject, UpdateTask,
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Opaque content version token — for `FsStore`, the SHA-256 hex digest of
/// the object's raw on-disk bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version(String);

impl Version {
    /// Borrow the underlying digest string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Wrap a backend-specific content token (SHA-256 hex, S3 `ETag`, etc.).
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A domain value paired with its current storage version for CAS writes.
#[derive(Debug, Clone)]
pub struct Versioned<T> {
    pub value: T,
    pub version: Version,
}

/// Typed storage-layer errors surfaced by [`Store`] implementations.
#[derive(Debug)]
pub enum StoreError {
    NotFound,
    Conflict,
    Unavailable,
    Invalid(String),
    Io(std::io::Error),
}

/// Filter for [`Store::list_artifacts`].
#[derive(Debug, Clone, Default)]
pub struct ArtifactListFilter {
    pub project: String,
}

/// Backend seam — async, object-safe, versioned reads and CAS writes.
#[async_trait]
pub trait Store: Send + Sync {
    async fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError>;
    async fn list_projects(
        &self,
        filter: ProjectListFilter,
    ) -> Result<Vec<Versioned<Project>>, StoreError>;
    async fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError>;
    async fn list_phases(
        &self,
        filter: PhaseListFilter,
    ) -> Result<Vec<Versioned<Phase>>, StoreError>;
    async fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError>;
    async fn list_tasks(&self, filter: TaskListFilter) -> Result<Vec<Versioned<Task>>, StoreError>;
    async fn list_artifacts(&self, filter: ArtifactListFilter)
        -> Result<Vec<Artifact>, StoreError>;

    async fn put_project(
        &self,
        project: &Project,
        expected: Option<Version>,
    ) -> Result<Version, StoreError>;
    async fn put_phase(
        &self,
        phase: &Phase,
        expected: Option<Version>,
    ) -> Result<Version, StoreError>;
    async fn put_task(&self, task: &Task, expected: Option<Version>)
        -> Result<Version, StoreError>;
    async fn put_artifact(&self, artifact: &Artifact) -> Result<(), StoreError>;
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn file_version(path: &Path) -> Result<Version, StoreError> {
    let bytes = fs::read(path).map_err(StoreError::Io)?;
    Ok(Version(hash_bytes(&bytes)))
}

#[allow(clippy::needless_pass_by_value)] // map_err hands errors by value
fn store_invalid(err: anyhow::Error) -> StoreError {
    StoreError::Invalid(err.to_string())
}

fn store_error_to_anyhow(err: StoreError) -> anyhow::Error {
    match err {
        StoreError::NotFound => anyhow!("not found"),
        StoreError::Conflict => anyhow!("conflict"),
        StoreError::Unavailable => anyhow!("unavailable"),
        StoreError::Invalid(msg) => anyhow!("{msg}"),
        StoreError::Io(io) => io.into(),
    }
}

fn store_io(err: anyhow::Error) -> StoreError {
    // Fast path: if the outermost error *is* an `io::Error`, take it whole. Otherwise
    // `write_atomic` / `append_jsonl` wrapped it with `.with_context`, so `downcast` on the
    // outermost error misses it — walk the source chain to find the io cause and keep its
    // `kind` (the full context message is preserved in the reconstructed error's payload).
    match err.downcast::<std::io::Error>() {
        Ok(io) => StoreError::Io(io),
        Err(err) => err
            .chain()
            .find_map(|cause| cause.downcast_ref::<std::io::Error>())
            .map_or_else(
                || StoreError::Invalid(err.to_string()),
                |io| StoreError::Io(std::io::Error::new(io.kind(), err.to_string())),
            ),
    }
}

fn versioned_project(
    store: &FsStore,
    slug: &str,
    with_body: bool,
) -> Result<Versioned<Project>, StoreError> {
    let path = store
        .project_dir(slug)
        .map_err(store_invalid)?
        .join("project.md");
    if !path.exists() {
        return Err(StoreError::NotFound);
    }
    let project = store.load_project(slug, with_body).map_err(store_invalid)?;
    let version = file_version(&path)?;
    Ok(Versioned {
        value: project,
        version,
    })
}

fn versioned_phase(path: &Path) -> Result<Versioned<Phase>, StoreError> {
    let phase = load_phase(path).map_err(store_invalid)?;
    let version = file_version(path)?;
    Ok(Versioned {
        value: phase,
        version,
    })
}

fn versioned_task(path: &Path, project_slug: &str) -> Result<Versioned<Task>, StoreError> {
    let (task, _notes) = load_task_with_notes(path, project_slug).map_err(store_invalid)?;
    let version = file_version(path)?;
    Ok(Versioned {
        value: task,
        version,
    })
}

pub(crate) fn notes_lines_for_task(task: &Task) -> Vec<String> {
    task.notes
        .iter()
        .map(|n| format_note_line(n.posted_at, &n.actor, &n.body))
        .collect()
}

/// Compare-and-swap write: verifies `expected` against the file's current version,
/// then writes atomically. `None` = create-only (fail if the file already exists);
/// `Some(v)` = update only if the current version matches `v`.
///
/// The version check and the write are not atomic with respect to other writers, so this
/// assumes single-writer-per-corpus — the same invariant the inherent write verbs uphold
/// by serializing through `MeshService.write_lock`. A caller invoking this via
/// `Arc<dyn Store>` must serialize its own read-modify-write the same way.
fn cas_write(
    path: &Path,
    content: &[u8],
    expected: Option<Version>,
) -> Result<Version, StoreError> {
    match expected {
        None => {
            if path.exists() {
                return Err(StoreError::Conflict);
            }
        }
        Some(expected_v) => {
            if !path.exists() {
                return Err(StoreError::NotFound);
            }
            let current = file_version(path)?;
            if current != expected_v {
                return Err(StoreError::Conflict);
            }
        }
    }
    write_atomic(path, content).map_err(store_io)?;
    file_version(path)
}

fn project_slug_for_id(store: &FsStore, project_id: &str) -> Result<String, StoreError> {
    for slug in store.project_slugs().map_err(store_invalid)? {
        let project = store.load_project(&slug, false).map_err(store_invalid)?;
        if project.id == project_id {
            return Ok(slug);
        }
    }
    Err(StoreError::NotFound)
}

#[async_trait]
impl Store for FsStore {
    async fn get_project(&self, slug: &str) -> Result<Versioned<Project>, StoreError> {
        versioned_project(self, slug, true)
    }

    async fn list_projects(
        &self,
        filter: ProjectListFilter,
    ) -> Result<Vec<Versioned<Project>>, StoreError> {
        let projects = Self::list_projects(self, &filter).map_err(store_invalid)?;
        projects
            .into_iter()
            .map(|p| {
                let path = self
                    .project_dir(&p.slug)
                    .map_err(store_invalid)?
                    .join("project.md");
                let version = file_version(&path)?;
                Ok(Versioned { value: p, version })
            })
            .collect()
    }

    async fn get_phase(&self, project: &str, slug: &str) -> Result<Versioned<Phase>, StoreError> {
        let path = self.find_phase_path(project, slug).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not found") {
                StoreError::NotFound
            } else {
                StoreError::Invalid(msg)
            }
        })?;
        versioned_phase(&path)
    }

    async fn list_phases(
        &self,
        filter: PhaseListFilter,
    ) -> Result<Vec<Versioned<Phase>>, StoreError> {
        let phases = Self::list_phases(self, &filter).map_err(store_invalid)?;
        let project = filter.project.as_deref();
        phases
            .into_iter()
            .map(|p| {
                let project_slug = project
                    .map(str::to_owned)
                    .or_else(|| self.project_slug_for_phase_id(&p.id).ok())
                    .ok_or(StoreError::NotFound)?;
                let path = self
                    .find_phase_path(&project_slug, &p.slug)
                    .map_err(|e| StoreError::Invalid(e.to_string()))?;
                versioned_phase(&path)
            })
            .collect()
    }

    async fn get_task(&self, id: &str) -> Result<Versioned<Task>, StoreError> {
        let (project_slug, path) = self.find_task_path(id).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("not found") {
                StoreError::NotFound
            } else {
                StoreError::Invalid(msg)
            }
        })?;
        versioned_task(&path, &project_slug)
    }

    async fn list_tasks(&self, filter: TaskListFilter) -> Result<Vec<Versioned<Task>>, StoreError> {
        let tasks = Self::list_tasks(self, &filter).map_err(store_invalid)?;
        tasks
            .into_iter()
            .map(|t| {
                let path = self
                    .project_dir(&t.project_slug)
                    .map_err(store_invalid)?
                    .join("tasks")
                    .join(task_filename(&t.id, &t.slug));
                versioned_task(&path, &t.project_slug)
            })
            .collect()
    }

    async fn list_artifacts(
        &self,
        filter: ArtifactListFilter,
    ) -> Result<Vec<Artifact>, StoreError> {
        Self::list_artifacts(self, &filter.project).map_err(store_invalid)
    }

    async fn put_project(
        &self,
        project: &Project,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        let dir = self.project_dir(&project.slug).map_err(store_invalid)?;
        let path = dir.join("project.md");
        if expected.is_none() && !dir.exists() {
            fs::create_dir_all(&dir).map_err(StoreError::Io)?;
        }
        let content = serialize_project_file(project).map_err(store_invalid)?;
        cas_write(&path, content.as_bytes(), expected)
    }

    async fn put_phase(
        &self,
        phase: &Phase,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        let project_slug = project_slug_for_id(self, &phase.project)?;
        let content = serialize_phase_file(phase).map_err(store_invalid)?;
        let path = if expected.is_none() {
            let phases_dir = self
                .project_dir(&project_slug)
                .map_err(store_invalid)?
                .join("phases");
            fs::create_dir_all(&phases_dir).map_err(StoreError::Io)?;
            phases_dir.join(phase_filename(phase.order, &phase.slug))
        } else {
            self.find_phase_path(&project_slug, &phase.slug)
                .map_err(|e| {
                    let msg = e.to_string();
                    if msg.contains("not found") {
                        StoreError::NotFound
                    } else {
                        StoreError::Invalid(msg)
                    }
                })?
        };
        cas_write(&path, content.as_bytes(), expected)
    }

    async fn put_task(
        &self,
        task: &Task,
        expected: Option<Version>,
    ) -> Result<Version, StoreError> {
        let path = self
            .project_dir(&task.project_slug)
            .map_err(store_invalid)?
            .join("tasks")
            .join(task_filename(&task.id, &task.slug));
        if expected.is_none() {
            let tasks_dir = path
                .parent()
                .ok_or_else(|| StoreError::Invalid("task path has no parent".to_owned()))?;
            fs::create_dir_all(tasks_dir).map_err(StoreError::Io)?;
        }
        // Regenerates the `## Notes` section from the parsed `task.notes`; any raw note
        // lines the in-memory `Task` doesn't carry are not round-tripped, so callers must
        // load the full task (notes included) before mutating to avoid dropping history.
        let notes_lines = notes_lines_for_task(task);
        let content = serialize_task_file(task, &notes_lines).map_err(store_invalid)?;
        cas_write(&path, content.as_bytes(), expected)
    }

    async fn put_artifact(&self, artifact: &Artifact) -> Result<(), StoreError> {
        let project_slug = project_slug_for_id(self, &artifact.project)?;
        let existing = Self::list_artifacts(self, &project_slug).map_err(store_invalid)?;
        if existing.iter().any(|a| a.id == artifact.id) {
            return Err(StoreError::Conflict);
        }
        let line =
            serde_json::to_string(artifact).map_err(|e| StoreError::Invalid(e.to_string()))?;
        let path = self
            .project_dir(&project_slug)
            .map_err(store_invalid)?
            .join("artifacts.jsonl");
        append_jsonl(&path, &line).map_err(store_io)?;
        Ok(())
    }
}

/// Filesystem-backed access to the on-disk dossier corpus at `root`.
///
/// Open via [`Self::open`]. Reads are lock-free; callers must serialize
/// writes (the mesh uses a process-local `Mutex` for that).
#[derive(Debug)]
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    /// Open a corpus rooted at `root`. The directory must contain a
    /// `.dossier/` marker.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let arg = root.as_ref().to_path_buf();
        let canonical = fs::canonicalize(&arg)
            .with_context(|| format!("resolve corpus root {}", arg.display()))?;
        let marker = canonical.join(".dossier");
        if !marker.is_dir() {
            bail!(
                "not a dossier corpus (no .dossier/ at {})",
                canonical.display()
            );
        }
        Ok(Self { root: canonical })
    }

    /// Absolute canonical corpus root directory (as resolved by [`Self::open`]).
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn project_dir(&self, slug: &str) -> Result<PathBuf> {
        if !is_valid_slug(slug) {
            bail!("invalid slug: {slug}");
        }
        Ok(self.root.join("projects").join(slug))
    }

    /// List projects subject to a predicate filter.
    ///
    /// An empty filter (`ProjectListFilter::default()`) returns every
    /// project in the corpus, sorted by `created_at` ASC. Description
    /// bodies are loaded only when needed (when a `body_contains`
    /// predicate is set); the default path stays as cheap as the
    /// pre-filter version. Description fields are always blanked
    /// before return so the response shape is uniform regardless of
    /// which filters fired — `body_contains` matches against the full
    /// body first, then the field is dropped.
    pub fn list_projects(&self, filter: &ProjectListFilter) -> Result<Vec<Project>> {
        let dir = self.root.join("projects");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let needs_body = filter.body_contains.is_some();
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let slug = entry.file_name().to_string_lossy().into_owned();
            let proj = self
                .load_project(&slug, needs_body)
                .with_context(|| format!("load project {slug}"))?;
            out.push(proj);
        }
        out.retain(|p| project_matches(p, filter));
        sort_projects(&mut out, filter);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        for p in &mut out {
            p.description.clear();
        }
        Ok(out)
    }

    /// Get one project by slug, including the description body.
    pub fn get_project(&self, slug: &str) -> Result<Project> {
        self.load_project(slug, true)
    }

    /// List phases subject to a predicate filter.
    ///
    /// When `filter.project` is `Some`, scans only that project; when
    /// `None`, walks every project in the corpus. Default sort is
    /// `order` ASC for a single-project listing and (`project_id`,
    /// `order`) ASC across projects, so cross-project results stay
    /// readable. An explicit `order_by` overrides the default.
    pub fn list_phases(&self, filter: &PhaseListFilter) -> Result<Vec<Phase>> {
        let mut out = if let Some(slug) = &filter.project {
            self.load_phases_for(slug)?
        } else {
            let mut all = Vec::new();
            for slug in self.project_slugs()? {
                all.extend(self.load_phases_for(&slug)?);
            }
            all
        };
        out.retain(|p| phase_matches(p, filter));
        sort_phases(&mut out, filter);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// List tasks subject to a predicate filter.
    ///
    /// When `filter.project` is `Some`, scans only that project; when
    /// `None`, walks every project in the corpus. `filter.phase` is
    /// matched against the task's phase **slug** for ergonomics — the
    /// internal `Task.phase` field stores the phase id, so the matcher
    /// resolves the slug to an id once per project before filtering.
    /// Default sort is `created_at` ASC; an explicit `order_by` on a
    /// nullable field drops rows where that field is null.
    pub fn list_tasks(&self, filter: &TaskListFilter) -> Result<Vec<Task>> {
        // Resolve `filter.phase` (a slug) into a phase id so task-row
        // equality on `phase` (which stores the id) lines up with what
        // the caller meant. An unresolvable slug is a typed error — not
        // a silent "no matches" — so a caller with a typo learns about
        // it instead of getting an empty result that looks correct.
        let resolved_phase_id: Option<String> = match (&filter.project, &filter.phase) {
            (Some(project_slug), Some(phase_slug)) => {
                let phases = self.load_phases_for(project_slug)?;
                let phase = phases
                    .iter()
                    .find(|p| &p.slug == phase_slug)
                    .ok_or_else(|| {
                        anyhow!("phase not found: {phase_slug} in project {project_slug}")
                    })?;
                Some(phase.id.clone())
            }
            _ => None,
        };

        let mut out = if let Some(slug) = &filter.project {
            self.load_tasks_for(slug)?
        } else {
            let mut all = Vec::new();
            for slug in self.project_slugs()? {
                all.extend(self.load_tasks_for(&slug)?);
            }
            all
        };

        out.retain(|t| task_matches(t, filter, resolved_phase_id.as_deref()));
        sort_tasks(&mut out, filter);
        if let Some(limit) = filter.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    /// Slugs of every project directory under `projects/`, sorted for
    /// determinism. Used by the cross-project list paths.
    fn project_slugs(&self) -> Result<Vec<String>> {
        let dir = self.root.join("projects");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            out.push(entry.file_name().to_string_lossy().into_owned());
        }
        out.sort();
        Ok(out)
    }

    /// Phases of a single project, unfiltered. Internal helper for both
    /// the single-project and cross-project list paths.
    fn load_phases_for(&self, project_slug: &str) -> Result<Vec<Phase>> {
        let dir = self.project_dir(project_slug)?.join("phases");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let phase =
                load_phase(&path).with_context(|| format!("load phase {}", path.display()))?;
            out.push(phase);
        }
        Ok(out)
    }

    /// Tasks of a single project, unfiltered. Internal helper for both
    /// the single-project and cross-project list paths.
    fn load_tasks_for(&self, project_slug: &str) -> Result<Vec<Task>> {
        let dir = self.project_dir(project_slug)?.join("tasks");
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let task = load_task(&path, project_slug)
                .with_context(|| format!("load task {}", path.display()))?;
            out.push(task);
        }
        Ok(out)
    }

    /// Artifacts linked to a project. JSONL on disk; the file may be
    /// missing or empty (both yield an empty vec).
    pub fn list_artifacts(&self, project_slug: &str) -> Result<Vec<Artifact>> {
        let path = self.project_dir(project_slug)?.join("artifacts.jsonl");
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let mut out = Vec::new();
        for (idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let a: Artifact = serde_json::from_str(line)
                .with_context(|| format!("parse {} line {}", path.display(), idx + 1))?;
            out.push(a);
        }
        Ok(out)
    }

    fn load_project(&self, slug: &str, with_body: bool) -> Result<Project> {
        let path = self.project_dir(slug)?.join("project.md");
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        parse_project(&raw, with_body).with_context(|| format!("parse project {}", path.display()))
    }
}

fn load_phase(path: &Path) -> Result<Phase> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_phase(&raw).with_context(|| format!("parse phase {}", path.display()))
}

fn load_task(path: &Path, project_slug: &str) -> Result<Task> {
    let (t, _notes) = load_task_with_notes(path, project_slug)?;
    Ok(t)
}

/// Read a task file and split its body into the spec section (above
/// `## Notes`) and the raw note lines. `Task.body` holds the spec
/// section only (trimmed); `Task.notes` is populated by parsing each
/// line — unparseable lines are skipped from the struct but kept in
/// the returned `Vec<String>`. Note that leading and trailing blank
/// lines inside the notes section are trimmed during the split, so the
/// round trip preserves entries and order but not surrounding blank
/// padding.
///
/// `project_slug` is stamped onto the returned task so callers (and the
/// JSON output) get the owning-project slug without having to thread it
/// alongside every Task value. It is not read from frontmatter.
fn load_task_with_notes(path: &Path, project_slug: &str) -> Result<(Task, Vec<String>)> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    parse_task(&raw, project_slug).with_context(|| format!("parse task {}", path.display()))
}

/// Parse a single `## Notes` line into a structured `Note`. Accepts the
/// canonical `- <timestamp> — <actor>: <body>` shape; timestamps may be
/// RFC3339 (what we write) or `YYYY-MM-DD` (legacy dogfood format).
/// Returns `None` if the line doesn't match — the raw line is still
/// preserved in the file body, so this is lossless on round-trip.
fn parse_note_line(line: &str) -> Option<crate::domain::Note> {
    use chrono::{NaiveDate, NaiveTime};
    let rest = line.trim_start().strip_prefix('-')?.trim_start();
    let (ts, after_ts) = rest.split_once(" — ")?;
    let (actor, body) = after_ts.split_once(": ")?;
    let posted_at = DateTime::parse_from_rfc3339(ts.trim())
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDate::parse_from_str(ts.trim(), "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_time(NaiveTime::MIN).and_local_timezone(Utc).single())
        })?;
    Some(crate::domain::Note {
        actor: actor.trim().to_owned(),
        body: body.trim().to_owned(),
        posted_at,
    })
}

/// Split a task markdown body into the spec section (lines above the
/// `## Notes` heading) and the raw lines below. Leading blank lines
/// after the heading are dropped so the round-trip stays tidy.
fn split_task_body(raw: &str) -> (String, Vec<String>) {
    let mut spec_lines: Vec<&str> = Vec::new();
    let mut notes_lines: Vec<String> = Vec::new();
    let mut in_notes = false;
    for line in raw.lines() {
        if !in_notes && line.trim() == "## Notes" {
            in_notes = true;
            continue;
        }
        if in_notes {
            notes_lines.push(line.to_owned());
        } else {
            spec_lines.push(line);
        }
    }
    let spec = spec_lines.join("\n").trim().to_owned();
    // Trim leading and trailing blank lines in linear time. `position` finds
    // the first non-blank index, then a single `drain(..first_real)` removes
    // the leading run; the tail uses `pop` since blank padding there is
    // bounded by a few lines. The previous `remove(0)` loop was quadratic
    // for long logs.
    let first_real = notes_lines
        .iter()
        .position(|l| !l.trim().is_empty())
        .unwrap_or(notes_lines.len());
    notes_lines.drain(..first_real);
    while notes_lines.last().is_some_and(|l| l.trim().is_empty()) {
        notes_lines.pop();
    }
    (spec, notes_lines)
}

fn split_frontmatter(raw: &str) -> Result<(String, String)> {
    let after_open = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("missing frontmatter delimiter"))?;
    let close_idx = after_open
        .find("\n---")
        .ok_or_else(|| anyhow!("unterminated frontmatter"))?;
    let front = after_open[..close_idx].to_owned();
    let after = &after_open[close_idx + 4..];
    let body = after.trim_start_matches(['\r', '\n']).to_owned();
    Ok((front, body))
}

/// Parse a project markdown blob (YAML frontmatter + optional description body).
pub(crate) fn parse_project(raw: &str, with_body: bool) -> Result<Project> {
    let (front, body) = split_frontmatter(raw)?;
    let mut p: Project = serde_yml::from_str(&front).context("parse project frontmatter")?;
    if with_body {
        body.trim().clone_into(&mut p.description);
    }
    Ok(p)
}

/// Parse a phase markdown blob (YAML frontmatter + optional body).
pub(crate) fn parse_phase(raw: &str) -> Result<Phase> {
    let (front, body) = split_frontmatter(raw)?;
    let mut p: Phase = serde_yml::from_str(&front).context("parse phase frontmatter")?;
    body.trim().clone_into(&mut p.body);
    Ok(p)
}

/// Parse a task markdown blob into a domain task and raw note lines.
pub(crate) fn parse_task(raw: &str, project_slug: &str) -> Result<(Task, Vec<String>)> {
    let (front, body) = split_frontmatter(raw)?;
    let mut t: Task = serde_yml::from_str(&front).context("parse task frontmatter")?;
    let (spec, notes_lines) = split_task_body(&body);
    t.body = spec;
    project_slug.clone_into(&mut t.project_slug);
    t.notes = notes_lines
        .iter()
        .filter_map(|l| parse_note_line(l))
        .collect();
    Ok((t, notes_lines))
}

// =============================================================================
// Write side
// =============================================================================

/// Bounded retry budget for concurrent `phase.add` writers — enough to drain a
/// burst of independent writers racing on one project; exhausting it signals
/// pathological contention rather than normal load.
const PHASE_ADD_MAX_RETRIES: u32 = 8;

impl FsStore {
    /// Create a new project on disk.
    ///
    /// Errors if the slug is already taken or fails the slug rules.
    /// Server-stamps `id`, `created_at`, `updated_at`, and initial
    /// `Planning` status.
    pub fn create_project(&self, args: NewProject) -> Result<Project> {
        if args.slug.is_empty() {
            bail!("slug is required");
        }
        let dir = self.project_dir(&args.slug)?;
        if dir.exists() {
            bail!("project slug already exists: {}", args.slug);
        }
        fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;

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

        let path = dir.join("project.md");
        let content = serialize_project_file(&project)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(project)
    }

    /// Update mutable fields of a project (`title`, `description`,
    /// `status`). Slug is the addressing key; id and `created_at` are
    /// preserved; `updated_at` is bumped.
    pub fn update_project(&self, args: UpdateProject) -> Result<Project> {
        if args.slug.is_empty() {
            bail!("slug is required");
        }
        let project_dir = self.project_dir(&args.slug)?;
        if !project_dir.try_exists()? {
            bail!("project not found: {}", args.slug);
        }
        let mut project = self.load_project(&args.slug, true)?;
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

        let path = self.project_dir(&project.slug)?.join("project.md");
        let content = serialize_project_file(&project)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(project)
    }

    /// Add a new phase to a project.
    ///
    /// `after_phase` (a phase slug, optional) inserts the new phase
    /// immediately after that phase, shifting subsequent phases up by 1.
    /// When omitted, the new phase is appended to the end. The phase
    /// slug must be unique within the project.
    ///
    /// Ordering is guarded by a compare-and-swap on `project.md`: two
    /// concurrent writers racing on the same project version retry with
    /// a freshly computed `order` rather than silently stomping.
    pub fn add_phase(&self, args: &NewPhase) -> Result<Phase> {
        if args.project.is_empty() {
            bail!("project is required");
        }
        if args.actor.is_empty() {
            bail!("actor is required to add a phase");
        }
        if args.owner.is_empty() {
            bail!("owner is required to add a phase");
        }
        if !is_valid_slug(&args.slug) {
            bail!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            );
        }
        let project_dir = self.project_dir(&args.project)?;
        if !project_dir.exists() {
            bail!("project not found: {}", args.project);
        }

        for _ in 0..PHASE_ADD_MAX_RETRIES {
            match self.try_add_phase_once(args, &project_dir) {
                Ok(phase) => return Ok(phase),
                Err(StoreError::Conflict) => (),
                Err(e) => return Err(store_error_to_anyhow(e)),
            }
        }
        bail!("phase add failed: too many concurrent writers (conflict)");
    }

    fn try_add_phase_once(&self, args: &NewPhase, project_dir: &Path) -> Result<Phase, StoreError> {
        // Load WITH body: the CAS gate below re-serializes this project back to
        // project.md, so an empty description here would wipe it on every phase.add.
        let Versioned {
            value: project,
            version: project_version,
        } = versioned_project(self, &args.project, true)?;

        let mut existing = self.load_phases_for(&args.project).map_err(store_invalid)?;
        if existing.iter().any(|p| p.slug == args.slug) {
            return Err(StoreError::Invalid(format!(
                "phase slug already exists in project: {}",
                args.slug
            )));
        }

        let new_order = compute_new_phase_order(&existing, args.after_phase.as_deref())
            .map_err(|e| StoreError::Invalid(e.to_string()))?;

        // Touch project.md (bump updated_at -> new version) under CAS: the gate that
        // makes two concurrent phase.add writers conflict instead of both computing the
        // same order and silently losing one.
        let mut project_gate = project.clone();
        project_gate.updated_at = now_utc();
        let project_path = project_dir.join("project.md");
        let project_content = serialize_project_file(&project_gate).map_err(store_invalid)?;
        cas_write(
            &project_path,
            project_content.as_bytes(),
            Some(project_version),
        )?;

        let phases_dir = project_dir.join("phases");
        fs::create_dir_all(&phases_dir).map_err(StoreError::Io)?;

        // Shift existing phases at or above new_order (descending so renames
        // don't collide).
        existing.sort_by_key(|p| std::cmp::Reverse(p.order));
        for p in &mut existing {
            if p.order < new_order {
                continue;
            }
            let old_name = phase_filename(p.order, &p.slug);
            let new_name = phase_filename(p.order + 1, &p.slug);
            fs::rename(phases_dir.join(&old_name), phases_dir.join(&new_name))
                .map_err(StoreError::Io)?;
            p.order += 1;
            let path = phases_dir.join(&new_name);
            let content = serialize_phase_file(p).map_err(store_invalid)?;
            write_atomic(&path, content.as_bytes()).map_err(store_io)?;
        }

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
        let path = phases_dir.join(phase_filename(new_order, &args.slug));
        let content = serialize_phase_file(&phase).map_err(store_invalid)?;
        write_atomic(&path, content.as_bytes()).map_err(store_io)?;
        Ok(phase)
    }

    /// Update mutable fields of a phase (`title`, `body`, `status`).
    ///
    /// (`project`, `slug`) is the addressing key; both are immutable.
    /// `id`, `order`, and `created_at` are preserved; `updated_at` is
    /// bumped.
    pub fn update_phase(&self, args: UpdatePhase) -> Result<Phase> {
        if args.project.is_empty() || args.slug.is_empty() {
            bail!("project and slug are required");
        }
        let path = self.find_phase_path(&args.project, &args.slug)?;
        let mut phase = load_phase(&path)?;
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
                bail!("owner must not be empty");
            }
            phase.owner = owner;
        }
        phase.updated_at = now_utc();
        let content = serialize_phase_file(&phase)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(phase)
    }

    fn find_phase_path(&self, project_slug: &str, phase_slug: &str) -> Result<PathBuf> {
        let phases_dir = self.project_dir(project_slug)?.join("phases");
        if !phases_dir.exists() {
            bail!("phase not found: {project_slug}/{phase_slug}");
        }
        let entries =
            fs::read_dir(&phases_dir).with_context(|| format!("read {}", phases_dir.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if let Some((_, slug)) = stem.split_once('-') {
                if slug == phase_slug {
                    return Ok(path);
                }
            }
        }
        bail!("phase not found: {project_slug}/{phase_slug}")
    }

    fn project_slug_for_phase_id(&self, phase_id: &str) -> Result<String> {
        for slug in self.project_slugs()? {
            let phases = self.load_phases_for(&slug)?;
            if phases.iter().any(|p| p.id == phase_id) {
                return Ok(slug);
            }
        }
        bail!("phase id not found: {phase_id}")
    }
}

impl FsStore {
    /// Create a new task in a project. Optionally anchored to a phase.
    ///
    /// Errors on duplicate slug within the project, invalid slug, unknown
    /// project, or unknown phase. Server-stamps id, timestamps, and
    /// `Todo` status.
    pub fn create_task(&self, args: NewTask) -> Result<Task> {
        if args.actor.is_empty() {
            bail!("actor is required to create a task");
        }
        if args.project.is_empty() {
            bail!("project is required");
        }
        if args.slug.is_empty() {
            bail!("slug is required");
        }
        if let Some(phase) = &args.phase {
            if phase.is_empty() {
                bail!("phase is required (omit the field entirely for a project-wide task)");
            }
            if !is_valid_slug(phase) {
                bail!("phase slug must be lowercase ascii (a-z, 0-9, -, _): {phase}");
            }
        }
        if !is_valid_slug(&args.slug) {
            bail!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            );
        }
        let project_dir = self.project_dir(&args.project)?;
        if !project_dir.exists() {
            bail!("project not found: {}", args.project);
        }
        let project = self.load_project(&args.project, false)?;

        let phase_id = match &args.phase {
            Some(phase_slug) => {
                let phases = self.load_phases_for(&args.project)?;
                let phase = phases
                    .iter()
                    .find(|p| &p.slug == phase_slug)
                    .ok_or_else(|| anyhow!("phase not found: {phase_slug}"))?;
                phase.id.clone()
            }
            None => String::new(),
        };

        let existing = self.load_tasks_for(&args.project)?;
        if existing.iter().any(|t| t.slug == args.slug) {
            bail!("task slug already exists in project: {}", args.slug);
        }
        validate_task_body(&args.body)?;

        let tasks_dir = project_dir.join("tasks");
        fs::create_dir_all(&tasks_dir)
            .with_context(|| format!("create {}", tasks_dir.display()))?;

        let now = now_utc();
        let id = new_id("tsk");
        let task = Task {
            id: id.clone(),
            project: project.id,
            project_slug: args.project.clone(),
            phase: phase_id,
            slug: args.slug.clone(),
            title: args.title,
            body: args.body,
            status: TaskStatus::Todo,
            assignee: String::new(),
            claimed_at: None,
            completed_at: None,
            created_at: now,
            updated_at: now,
            notes: Vec::new(),
            depends_on: args.depends_on,
        };
        let _ = args.actor; // creator recorded in commit history; no created_by on task in v0
        let path = tasks_dir.join(task_filename(&id, &args.slug));
        let content = serialize_task_file(&task, &[])?;
        write_atomic(&path, content.as_bytes())?;
        Ok(task)
    }

    /// Claim a task for `actor`. Sole entry into `claimed` status.
    ///
    /// - `Todo` with empty assignee → claimed by `actor`.
    /// - Non-terminal status with `assignee == actor` → no-op return.
    /// - Different actor on a held task → error.
    /// - Terminal (`done` / `cancelled`) → error.
    pub fn claim_task(&self, args: &ClaimTask) -> Result<Task> {
        let (project_slug, path) = self.find_task_path(&args.id)?;
        let (task, notes_lines) = load_task_with_notes(&path, &project_slug)?;
        let task = apply_claim_task(task, &args.actor, now_utc())?;
        let content = serialize_task_file(&task, &notes_lines)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(task)
    }

    /// Update a task's body, status, and/or append a note.
    ///
    /// `status=claimed` and `status=done` are rejected — use
    /// `claim_task` / `complete_task` instead. Terminal states reject
    /// all status transitions.
    pub fn update_task(&self, args: UpdateTask) -> Result<Task> {
        if args.actor.is_empty() {
            bail!("actor is required to update a task");
        }
        let (project_slug, path) = self.find_task_path(&args.id)?;
        let (mut task, mut notes_lines) = load_task_with_notes(&path, &project_slug)?;

        let now = now_utc();
        if let Some(target) = args.status {
            task = apply_task_status_update(task, target, now)?;
        }
        if let Some(body) = args.body {
            task = apply_task_body_update(task, body, now)?;
        }
        if let Some(depends_on) = args.depends_on {
            task.depends_on = depends_on;
        }
        if let Some(note) = args.note {
            append_note(&mut task, &mut notes_lines, now, &args.actor, &note)?;
        }
        task.updated_at = now;

        let content = serialize_task_file(&task, &notes_lines)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(task)
    }

    /// Mark a task done. Sole entry into `done` status.
    ///
    /// Errors unless the task is in `InProgress`. Stamps `completed_at`,
    /// bumps `updated_at`, and optionally appends a closing note.
    pub fn complete_task(&self, args: CompleteTask) -> Result<Task> {
        if args.actor.is_empty() {
            bail!("actor is required to complete a task");
        }
        let (project_slug, path) = self.find_task_path(&args.id)?;
        let (mut task, mut notes_lines) = load_task_with_notes(&path, &project_slug)?;

        let now = now_utc();
        task = apply_complete_task(task, now)?;
        if let Some(note) = args.note {
            append_note(&mut task, &mut notes_lines, now, &args.actor, &note)?;
        }

        let content = serialize_task_file(&task, &notes_lines)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(task)
    }

    /// Load a task by id without naming its owning project. Returns
    /// `Ok(None)` when no file matches; propagates I/O / parse failures
    /// and duplicate-id corpus state.
    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        let Some((project_slug, path)) = self.try_find_task_path(id)? else {
            return Ok(None);
        };
        let (task, _notes) = load_task_with_notes(&path, &project_slug)?;
        Ok(Some(task))
    }

    /// Locate a task file by id by walking each project's `tasks/` dir.
    /// O(projects × tasks) — cheap at v0 corpus sizes; an index lands
    /// when it actually matters.
    fn find_task_path(&self, task_id: &str) -> Result<(String, PathBuf)> {
        match self.try_find_task_path(task_id)? {
            Some(pair) => Ok(pair),
            None => bail!("task not found: {task_id}"),
        }
    }

    /// Like [`Self::find_task_path`], but returns `Ok(None)` when the id
    /// is absent. Duplicate ids under different projects still `bail!`.
    fn try_find_task_path(&self, task_id: &str) -> Result<Option<(String, PathBuf)>> {
        let projects_dir = self.root.join("projects");
        if !projects_dir.exists() {
            return Ok(None);
        }
        let entries = fs::read_dir(&projects_dir)
            .with_context(|| format!("read {}", projects_dir.display()))?;
        let mut found: Option<(String, PathBuf)> = None;
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let project_slug = entry.file_name().to_string_lossy().into_owned();
            let tasks_dir = entry.path().join("tasks");
            if !tasks_dir.exists() {
                continue;
            }
            let task_entries = fs::read_dir(&tasks_dir)
                .with_context(|| format!("read {}", tasks_dir.display()))?;
            for task_entry in task_entries {
                let task_entry = task_entry?;
                let path = task_entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if let Some((id, _slug)) = stem.split_once('-') {
                    if id == task_id {
                        if found.is_some() {
                            bail!("duplicate task id: {task_id}");
                        }
                        found = Some((project_slug.clone(), path));
                    }
                }
            }
        }
        Ok(found)
    }
}

impl FsStore {
    /// Append an artifact pointing at something concrete (a commit, PR,
    /// file, URL, run, doc, …). Append-only — `artifacts.jsonl` never
    /// rewrites a prior entry; deletes are tombstones if we ever need
    /// them.
    ///
    /// Validates that the project exists and (when supplied) the task
    /// belongs to that same project. `kind`, `reference`, `label`, and
    /// `actor` are required and rejected if they contain newline / CR.
    /// JSON serialization would *escape* embedded newlines (so the
    /// JSONL framing stays valid), but a multi-line label or ref is
    /// almost certainly a caller bug and breaks grep-ability of the
    /// file; cheap to refuse at the boundary.
    pub fn link_artifact(&self, args: LinkArtifact) -> Result<Artifact> {
        if args.actor.is_empty() {
            bail!("actor is required to link an artifact");
        }
        if args.project.is_empty() {
            bail!("project is required");
        }
        if args.kind.is_empty() {
            bail!("kind is required");
        }
        if args.reference.is_empty() {
            bail!("ref is required");
        }
        if args.label.is_empty() {
            bail!("label is required");
        }
        for (field, value) in [
            ("kind", &args.kind),
            ("ref", &args.reference),
            ("label", &args.label),
            ("actor", &args.actor),
        ] {
            validate_single_line(field, value)?;
        }

        let project_dir = self.project_dir(&args.project)?;
        if !project_dir.exists() {
            bail!("project not found: {}", args.project);
        }
        let project = self.load_project(&args.project, false)?;

        let task_id = match &args.task {
            Some(task_id) if task_id.is_empty() => {
                bail!("task is empty (omit the field entirely for a project-wide artifact)")
            }
            Some(task_id) => {
                // Project-scoped lookup: the artifact's project is already
                // known, so walking the whole corpus is wasteful and the
                // resulting error ("task not found") doesn't tell the
                // caller *where* dossier looked. Scan only this project's
                // tasks/ directory, mirroring `list_tasks`' file filter
                // (regular `.md` files only) and propagating I/O errors
                // rather than silently dropping them — a permissions hit
                // mid-walk shouldn't masquerade as "not found".
                let tasks_dir = project_dir.join("tasks");
                let mut found = false;
                if tasks_dir.exists() {
                    let entries = fs::read_dir(&tasks_dir)
                        .with_context(|| format!("read {}", tasks_dir.display()))?;
                    for entry in entries {
                        let entry = entry?;
                        let path = entry.path();
                        if path.is_dir() {
                            continue;
                        }
                        if path.extension().and_then(|s| s.to_str()) != Some("md") {
                            continue;
                        }
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default();
                        if let Some((id, _)) = stem.split_once('-') {
                            if id == task_id {
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if !found {
                    bail!("task {task_id} not found in project {}", args.project);
                }
                task_id.clone()
            }
            None => String::new(),
        };

        let existing = self.list_artifacts(&args.project)?;
        if let Some(artifact) = existing
            .into_iter()
            .find(|a| a.task == task_id && a.kind == args.kind && a.reference == args.reference)
        {
            return Ok(artifact);
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

        let line = serde_json::to_string(&artifact).context("serialize artifact")?;
        let path = project_dir.join("artifacts.jsonl");
        append_jsonl(&path, &line)?;
        Ok(artifact)
    }
}

/// Filename for a task: ULID + slug. ULID never contains a `-` so the
/// split on the first `-` is unambiguous.
pub(crate) fn task_filename(id: &str, slug: &str) -> String {
    format!("{id}-{slug}.md")
}

/// Filename for a phase: zero-padded order + slug. The order prefix
/// gives stable sort in directory listings AND a human-readable hint.
pub(crate) fn phase_filename(order: i32, slug: &str) -> String {
    format!("{order:02}-{slug}.md")
}

/// Format a single Notes line: `- <RFC3339> — <actor>: <body>`.
fn format_note_line(at: DateTime<Utc>, actor: &str, body: &str) -> String {
    use chrono::SecondsFormat;
    format!(
        "- {} — {}: {}",
        at.to_rfc3339_opts(SecondsFormat::Secs, true),
        actor,
        body
    )
}

/// Append a note to both the on-disk lines and the in-memory `Task`
/// so the verb's return value reflects the new state. Rejects multi-
/// line input — the `## Notes` section is one entry per line, and
/// `\n` / `\r` in either field would break parsing on the round trip.
fn append_note(
    task: &mut Task,
    notes_lines: &mut Vec<String>,
    at: DateTime<Utc>,
    actor: &str,
    body: &str,
) -> Result<()> {
    if actor.contains(['\n', '\r']) || body.contains(['\n', '\r']) {
        bail!("note must be single-line (no newline or carriage return)");
    }
    // `parse_note_line` splits actor from body on the first `": "`.
    // Allowing it in the actor string would yield a truncated actor and
    // a mangled body on round-trip, even though the raw line is intact.
    if actor.contains(": ") {
        bail!("actor must not contain `: ` (reserved as the actor/body delimiter in note lines)");
    }
    let body_trimmed = body.trim();
    if body_trimmed.is_empty() {
        bail!("note body must not be empty");
    }
    notes_lines.push(format_note_line(at, actor, body_trimmed));
    task.notes.push(crate::domain::Note {
        actor: actor.to_owned(),
        body: body_trimmed.to_owned(),
        posted_at: at,
    });
    Ok(())
}

/// Write `content` to `path` atomically: write to a `.tmp` sibling, then
/// rename. Atomic on every supported filesystem (NTFS, APFS, ext4 / xfs
/// / btrfs). Caller must ensure the parent directory exists.
pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    fs::write(&tmp, content).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Append a single line (terminated by `\n`) to `path` under a lock.
///
/// File is created if missing. The exclusive lock guards against torn
/// writes between processes that share a corpus, even though v0's
/// single-writer assumption means we don't expect contention.
pub fn append_jsonl(path: &Path, line: &str) -> Result<()> {
    // Windows `LockFileEx` rejects FILE_APPEND_DATA-only handles; pair
    // append with read so the handle has GENERIC_READ. POSIX is unaffected.
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    let write_result =
        writeln!(file, "{line}").with_context(|| format!("append to {}", path.display()));
    let unlock_result = file.unlock().context("unlock");
    write_result.and(unlock_result)
}

/// Current UTC timestamp. Wrapper for testability — callers that want
/// deterministic time can stub at the boundary.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[derive(Serialize)]
struct ProjectFrontmatter<'a> {
    id: &'a str,
    slug: &'a str,
    title: &'a str,
    status: ProjectStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "str::is_empty")]
    created_by: &'a str,
}

impl<'a> From<&'a Project> for ProjectFrontmatter<'a> {
    fn from(p: &'a Project) -> Self {
        Self {
            id: &p.id,
            slug: &p.slug,
            title: &p.title,
            status: p.status,
            created_at: p.created_at,
            updated_at: p.updated_at,
            created_by: &p.created_by,
        }
    }
}

/// Serialize a project to its on-disk markdown form: YAML frontmatter,
/// blank line, description body, trailing newline. Body absent when
/// description is empty so the file stays tidy.
pub(crate) fn serialize_project_file(project: &Project) -> Result<String> {
    let frontmatter = serde_yml::to_string(&ProjectFrontmatter::from(project))
        .context("serialize frontmatter")?;
    let body = project.description.trim();
    Ok(if body.is_empty() {
        format!("---\n{frontmatter}---\n")
    } else {
        format!("---\n{frontmatter}---\n\n{body}\n")
    })
}

#[derive(Serialize)]
struct PhaseFrontmatter<'a> {
    id: &'a str,
    project: &'a str,
    slug: &'a str,
    title: &'a str,
    order: i32,
    status: PhaseStatus,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "str::is_empty")]
    created_by: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    owner: &'a str,
}

impl<'a> From<&'a Phase> for PhaseFrontmatter<'a> {
    fn from(p: &'a Phase) -> Self {
        Self {
            id: &p.id,
            project: &p.project,
            slug: &p.slug,
            title: &p.title,
            order: p.order,
            status: p.status,
            created_at: p.created_at,
            updated_at: p.updated_at,
            created_by: &p.created_by,
            owner: &p.owner,
        }
    }
}

pub(crate) fn serialize_phase_file(phase: &Phase) -> Result<String> {
    let frontmatter = serde_yml::to_string(&PhaseFrontmatter::from(phase))
        .context("serialize phase frontmatter")?;
    let body = phase.body.trim();
    Ok(if body.is_empty() {
        format!("---\n{frontmatter}---\n")
    } else {
        format!("---\n{frontmatter}---\n\n{body}\n")
    })
}

#[derive(Serialize)]
struct TaskFrontmatter<'a> {
    id: &'a str,
    project: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    phase: &'a str,
    slug: &'a str,
    title: &'a str,
    status: TaskStatus,
    #[serde(skip_serializing_if = "str::is_empty")]
    assignee: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    claimed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    depends_on: &'a [String],
}

impl<'a> From<&'a Task> for TaskFrontmatter<'a> {
    fn from(t: &'a Task) -> Self {
        Self {
            id: &t.id,
            project: &t.project,
            phase: &t.phase,
            slug: &t.slug,
            title: &t.title,
            status: t.status,
            assignee: &t.assignee,
            claimed_at: t.claimed_at,
            completed_at: t.completed_at,
            created_at: t.created_at,
            updated_at: t.updated_at,
            depends_on: &t.depends_on,
        }
    }
}

/// Serialize a task to its on-disk form: YAML frontmatter, blank line,
/// spec body (if any), then a `## Notes` section reconstructed from
/// `notes_lines`. Each note line is emitted verbatim with one trailing
/// newline.
pub(crate) fn serialize_task_file(task: &Task, notes_lines: &[String]) -> Result<String> {
    let frontmatter =
        serde_yml::to_string(&TaskFrontmatter::from(task)).context("serialize task frontmatter")?;
    let spec = task.body.trim();
    let mut out = format!("---\n{frontmatter}---\n");
    if !spec.is_empty() {
        out.push('\n');
        out.push_str(spec);
        out.push('\n');
    }
    if !notes_lines.is_empty() {
        out.push_str("\n## Notes\n\n");
        for line in notes_lines {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

// =============================================================================
// Filter matchers and sorters
// =============================================================================

/// Case-insensitive literal substring check. Allocates lowercased
/// copies of both inputs — cheap at v0 corpus sizes, replace with a
/// streaming check when measurement disagrees.
fn icontains(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// `_after` is inclusive (>=), `_before` is strictly less than. Picks
/// the boundary semantics that match how a caller phrases "since" /
/// "before" in natural language.
fn in_range(
    value: DateTime<Utc>,
    after: Option<DateTime<Utc>>,
    before: Option<DateTime<Utc>>,
) -> bool {
    if let Some(a) = after {
        if value < a {
            return false;
        }
    }
    if let Some(b) = before {
        if value >= b {
            return false;
        }
    }
    true
}

pub(crate) fn project_matches(p: &Project, f: &ProjectListFilter) -> bool {
    if let Some(statuses) = &f.status {
        if !statuses.is_empty() && !statuses.contains(&p.status) {
            return false;
        }
    }
    if let Some(needle) = &f.body_contains {
        if !icontains(&p.description, needle) {
            return false;
        }
    }
    in_range(p.created_at, f.created_after, f.created_before)
        && in_range(p.updated_at, f.updated_after, f.updated_before)
}

pub(crate) fn sort_projects(out: &mut [Project], f: &ProjectListFilter) {
    let order = f.order_by.unwrap_or(ProjectOrderField::CreatedAt);
    out.sort_by(|a, b| match order {
        ProjectOrderField::CreatedAt => a
            .created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id)),
        ProjectOrderField::UpdatedAt => a
            .updated_at
            .cmp(&b.updated_at)
            .then_with(|| a.id.cmp(&b.id)),
    });
    if f.desc.unwrap_or(false) {
        out.reverse();
    }
}

pub(crate) fn phase_matches(p: &Phase, f: &PhaseListFilter) -> bool {
    if let Some(statuses) = &f.status {
        if !statuses.is_empty() && !statuses.contains(&p.status) {
            return false;
        }
    }
    if let Some(needle) = &f.body_contains {
        if !icontains(&p.body, needle) {
            return false;
        }
    }
    in_range(p.created_at, f.created_after, f.created_before)
        && in_range(p.updated_at, f.updated_after, f.updated_before)
}

pub(crate) fn sort_phases(out: &mut [Phase], f: &PhaseListFilter) {
    let order = f.order_by.unwrap_or(PhaseOrderField::Order);
    out.sort_by(|a, b| match order {
        PhaseOrderField::CreatedAt => a
            .created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id)),
        PhaseOrderField::UpdatedAt => a
            .updated_at
            .cmp(&b.updated_at)
            .then_with(|| a.id.cmp(&b.id)),
        // Cross-project listings group by project so the linear `order`
        // sort stays meaningful — phase order is per-project, not global.
        PhaseOrderField::Order => a
            .project
            .cmp(&b.project)
            .then_with(|| a.order.cmp(&b.order))
            .then_with(|| a.id.cmp(&b.id)),
    });
    if f.desc.unwrap_or(false) {
        out.reverse();
    }
}

pub(crate) fn task_matches(t: &Task, f: &TaskListFilter, resolved_phase_id: Option<&str>) -> bool {
    if let Some(pid) = resolved_phase_id {
        if t.phase != pid {
            return false;
        }
    }
    if let Some(statuses) = &f.status {
        if !statuses.is_empty() && !statuses.contains(&t.status) {
            return false;
        }
    }
    if let Some(assignee) = &f.assignee {
        if &t.assignee != assignee {
            return false;
        }
    }
    if let Some(needle) = &f.body_contains {
        if !icontains(&t.body, needle) {
            return false;
        }
    }
    if !in_range(t.created_at, f.created_after, f.created_before) {
        return false;
    }
    if !in_range(t.updated_at, f.updated_after, f.updated_before) {
        return false;
    }
    if f.completed_after.is_some() || f.completed_before.is_some() {
        let Some(ts) = t.completed_at else {
            return false;
        };
        if !in_range(ts, f.completed_after, f.completed_before) {
            return false;
        }
    }
    if f.claimed_after.is_some() || f.claimed_before.is_some() {
        let Some(ts) = t.claimed_at else {
            return false;
        };
        if !in_range(ts, f.claimed_after, f.claimed_before) {
            return false;
        }
    }
    true
}

pub(crate) fn sort_tasks(out: &mut Vec<Task>, f: &TaskListFilter) {
    let order = f.order_by.unwrap_or(TaskOrderField::CreatedAt);
    // Sort keys on nullable fields (`completed_at`, `claimed_at`)
    // implicitly drop nulls — sorting by a field you don't have is
    // almost certainly not what the caller wants. This mirrors the
    // documented behavior in the spec.
    match order {
        TaskOrderField::CreatedAt => {
            out.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        TaskOrderField::UpdatedAt => {
            out.sort_by(|a, b| {
                a.updated_at
                    .cmp(&b.updated_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        TaskOrderField::CompletedAt => {
            out.retain(|t| t.completed_at.is_some());
            out.sort_by(|a, b| {
                a.completed_at
                    .cmp(&b.completed_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        TaskOrderField::ClaimedAt => {
            out.retain(|t| t.claimed_at.is_some());
            out.sort_by(|a, b| {
                a.claimed_at
                    .cmp(&b.claimed_at)
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
    }
    if f.desc.unwrap_or(false) {
        out.reverse();
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]

    use super::*;
    use crate::domain::{Note, TaskListFilter};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn task_filter_for(project: &str) -> TaskListFilter {
        TaskListFilter {
            project: Some(project.to_owned()),
            ..Default::default()
        }
    }

    fn phase_filter_for(project: &str) -> PhaseListFilter {
        PhaseListFilter {
            project: Some(project.to_owned()),
            ..Default::default()
        }
    }

    /// Path-traversal-shaped caller input; must not reach filesystem join
    /// without validation.
    const BAD_PROJECT_SLUG: &str = "../etc/passwd";

    #[test]
    fn read_dogfood_corpus() {
        let store = FsStore::open(repo_root()).expect("open corpus");

        let projects = store
            .list_projects(&ProjectListFilter::default())
            .expect("list projects");
        assert!(projects.iter().any(|p| p.slug == "dossier"));
        for p in &projects {
            assert!(
                p.description.is_empty(),
                "list_projects returned a body for {} — should be metadata only",
                p.slug
            );
        }

        let p = store.get_project("dossier").expect("get project");
        assert!(
            p.description.contains("Agent-native project management"),
            "description body missing or wrong: {:?}",
            p.description
        );

        let phases = store
            .list_phases(&phase_filter_for("dossier"))
            .expect("list phases");
        assert_eq!(phases.len(), 4);
        for (i, ph) in phases.iter().enumerate() {
            assert_eq!(ph.order, i as i32 + 1, "phase[{i}] order");
        }

        let tasks = store
            .list_tasks(&task_filter_for("dossier"))
            .expect("list tasks");
        assert_eq!(tasks.len(), 3);
        for task in &tasks {
            assert!(!task.body.contains("## Notes"));
        }

        let arts = store.list_artifacts("dossier").expect("list artifacts");
        assert_eq!(arts.len(), 3);
    }

    #[test]
    fn open_rejects_non_corpus() {
        let tmp = std::env::temp_dir().join(format!(
            "dossier-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmp).unwrap();
        let result = FsStore::open(&tmp);
        let _ = fs::remove_dir_all(&tmp);
        let err = result.expect_err("opening empty dir should fail");
        assert!(
            err.to_string().contains("not a dossier corpus"),
            "got: {err}"
        );
    }

    fn fresh_corpus() -> (tempfile::TempDir, FsStore) {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let store = FsStore::open(tmp.path()).expect("open fresh corpus");
        (tmp, store)
    }

    #[tokio::test]
    async fn put_project_cas_round_trip() {
        let (_tmp, store) = fresh_corpus();
        let now = now_utc();
        let project = Project {
            id: new_id("prj"),
            slug: "cas-proj".to_owned(),
            title: "CAS".to_owned(),
            description: "v0 body".to_owned(),
            status: ProjectStatus::Planning,
            created_at: now,
            updated_at: now,
            created_by: "human:test".to_owned(),
        };
        let v0 = store.put_project(&project, None).await.expect("create");

        let mut updated = project.clone();
        updated.title = "changed".to_owned();
        updated.description = "v1 body".to_owned();
        let wrong = Version("deadbeef".to_owned());
        let err = store
            .put_project(&updated, Some(wrong))
            .await
            .expect_err("stale version");
        assert!(matches!(err, StoreError::Conflict));

        let path = store
            .project_dir("cas-proj")
            .expect("project dir")
            .join("project.md");
        let on_disk = fs::read_to_string(&path).expect("read project");
        assert!(on_disk.contains("CAS"));
        assert!(!on_disk.contains("changed"));

        let v1 = store
            .put_project(&updated, Some(v0.clone()))
            .await
            .expect("update");
        assert_ne!(v0, v1);

        let err = store
            .put_project(&project, None)
            .await
            .expect_err("duplicate create");
        assert!(matches!(err, StoreError::Conflict));
    }

    #[test]
    fn write_atomic_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        write_atomic(&path, b"hello world").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello world");
        // Overwrite with shorter content; previous bytes must not leak.
        write_atomic(&path, b"hi").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hi");
        // tmp sibling must not linger.
        let tmp = dir.path().join("hello.txt.tmp");
        assert!(!tmp.exists(), "tmp sibling lingered");
    }

    #[test]
    fn append_jsonl_creates_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("artifacts.jsonl");
        append_jsonl(&path, r#"{"id":"art_1"}"#).unwrap();
        append_jsonl(&path, r#"{"id":"art_2"}"#).unwrap();
        let body = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines, vec![r#"{"id":"art_1"}"#, r#"{"id":"art_2"}"#]);
    }

    #[test]
    fn new_id_format() {
        let id = new_id("prj");
        let parts: Vec<&str> = id.splitn(2, '_').collect();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0], "prj");
        // ULID is 26 chars Crockford base32.
        assert_eq!(parts[1].len(), 26);
        assert!(
            parts[1]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() && !"ILOU".contains(c.to_ascii_uppercase())),
            "ulid chars: {}",
            parts[1]
        );
    }

    #[test]
    fn slug_validation() {
        assert!(is_valid_slug("dossier"));
        assert!(is_valid_slug("hyrox-coach"));
        assert!(is_valid_slug("v0_2"));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("Dossier")); // uppercase
        assert!(!is_valid_slug("dos sier")); // space
        assert!(!is_valid_slug("dossier!")); // punctuation
    }

    #[test]
    fn create_project_round_trip() {
        let (_tmp, store) = fresh_corpus();

        let project = store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha".to_owned(),
                description: "First project body.\n\nA second paragraph.".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect("create alpha");

        assert_eq!(project.slug, "alpha");
        assert!(project.id.starts_with("prj_"));
        assert_eq!(project.status, ProjectStatus::Planning);
        assert_eq!(project.created_by, "human:test");
        assert_eq!(project.created_at, project.updated_at);

        // Round-trip through the read side.
        let listed = store.list_projects(&ProjectListFilter::default()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, project.id);

        let fetched = store.get_project("alpha").unwrap();
        assert_eq!(fetched.id, project.id);
        assert!(
            fetched.description.contains("First project body."),
            "body lost: {:?}",
            fetched.description
        );
    }

    #[test]
    fn create_project_rejects_duplicate_slug() {
        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        let err = store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha v2".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .expect_err("duplicate slug");
        assert!(
            err.to_string().contains("slug already exists"),
            "got: {err}"
        );
    }

    #[test]
    fn create_project_rejects_invalid_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .create_project(NewProject {
                slug: "Bad Slug!".to_owned(),
                title: "x".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn get_project_rejects_invalid_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .get_project(BAD_PROJECT_SLUG)
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn list_phases_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .list_phases(&PhaseListFilter {
                project: Some(BAD_PROJECT_SLUG.to_owned()),
                ..Default::default()
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn list_tasks_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .list_tasks(&TaskListFilter {
                project: Some(BAD_PROJECT_SLUG.to_owned()),
                ..Default::default()
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn list_artifacts_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .list_artifacts(BAD_PROJECT_SLUG)
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn update_project_rejects_invalid_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .update_project(UpdateProject {
                slug: BAD_PROJECT_SLUG.to_owned(),
                title: Some("x".to_owned()),
                ..Default::default()
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn update_project_errors_on_nonexistent_slug() {
        let (_tmp, store) = fresh_corpus();
        let slug = "ghost";
        let err = store
            .update_project(UpdateProject {
                slug: slug.to_owned(),
                title: Some("x".to_owned()),
                ..Default::default()
            })
            .expect_err("unknown project");
        let msg = err.to_string();
        assert!(msg.contains("project not found"), "got: {err}");
        assert!(msg.contains(slug), "got: {err}");
    }

    #[test]
    fn update_phase_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .update_phase(UpdatePhase {
                project: BAD_PROJECT_SLUG.to_owned(),
                slug: "spec".to_owned(),
                title: Some("x".to_owned()),
                ..Default::default()
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn add_phase_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .add_phase(&NewPhase {
                project: BAD_PROJECT_SLUG.to_owned(),
                slug: "spec".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn link_artifact_rejects_invalid_project_slug() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .link_artifact(LinkArtifact {
                project: BAD_PROJECT_SLUG.to_owned(),
                task: None,
                kind: "pr".to_owned(),
                reference: "https://example.com/pr/1".to_owned(),
                label: "PR #1".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect_err("invalid slug");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");
    }

    #[test]
    fn update_project_preserves_id_and_created_at() {
        let (_tmp, store) = fresh_corpus();
        let original = store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha".to_owned(),
                description: "v1 body".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        // Force a measurable gap so updated_at differs.
        std::thread::sleep(std::time::Duration::from_millis(10));

        let updated = store
            .update_project(UpdateProject {
                slug: "alpha".to_owned(),
                title: Some("Alpha v2".to_owned()),
                description: Some("v2 body".to_owned()),
                status: Some(ProjectStatus::Active),
            })
            .unwrap();

        assert_eq!(updated.id, original.id, "id changed");
        assert_eq!(
            updated.created_at, original.created_at,
            "created_at changed"
        );
        assert!(
            updated.updated_at > original.updated_at,
            "updated_at not bumped"
        );
        assert_eq!(updated.title, "Alpha v2");
        assert_eq!(updated.description, "v2 body");
        assert_eq!(updated.status, ProjectStatus::Active);

        // And confirm via re-read.
        let fetched = store.get_project("alpha").unwrap();
        assert_eq!(fetched.title, "Alpha v2");
        assert_eq!(fetched.description, "v2 body");
        assert_eq!(fetched.status, ProjectStatus::Active);
    }

    fn seed_project(store: &FsStore, slug: &str) -> Project {
        store
            .create_project(NewProject {
                slug: slug.to_owned(),
                title: format!("Project {slug}"),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .expect("seed project")
    }

    fn add_phase_simple(store: &FsStore, project: &str, slug: &str) -> Phase {
        store
            .add_phase(&NewPhase {
                project: project.to_owned(),
                slug: slug.to_owned(),
                title: slug.to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect("add phase")
    }

    #[test]
    fn add_phase_persists_created_by() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let created = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Spec phase".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "claude-code:alice".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect("add_phase");

        assert_eq!(created.created_by, "claude-code:alice");

        let phases = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let round_trip = phases
            .iter()
            .find(|p| p.slug == "spec")
            .expect("phase listed");
        assert_eq!(round_trip.created_by, "claude-code:alice");
    }

    #[test]
    fn add_phase_preserves_project_description() {
        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha".to_owned(),
                description: "important description body".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect("create project");

        // phase.add re-writes project.md as its CAS gate; it must not drop the body.
        add_phase_simple(&store, "alpha", "spec");

        let project = store.load_project("alpha", true).expect("load project");
        assert_eq!(
            project.description, "important description body",
            "phase.add must not wipe the project description"
        );
    }

    #[test]
    fn add_phase_rejects_empty_actor() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let err = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Spec phase".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: String::new(),
                owner: "human:test".to_owned(),
            })
            .expect_err("empty actor");
        assert!(err.to_string().contains("actor is required"), "got: {err}");
    }

    #[test]
    fn add_phase_persists_owner() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let created = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "owned".to_owned(),
                title: "Owned phase".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "claude-code:alice".to_owned(),
                owner: "team:frontend".to_owned(),
            })
            .expect("add_phase");

        assert_eq!(created.owner, "team:frontend");

        let phases = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let round_trip = phases
            .iter()
            .find(|p| p.slug == "owned")
            .expect("phase listed");
        assert_eq!(round_trip.owner, "team:frontend");
    }

    #[test]
    fn add_phase_rejects_empty_owner() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let err = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Spec phase".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: String::new(),
            })
            .expect_err("empty owner");
        assert!(err.to_string().contains("owner is required"), "got: {err}");
    }

    #[test]
    fn update_phase_replaces_owner() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");

        let updated = store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                owner: Some("team:platform".to_owned()),
                ..Default::default()
            })
            .expect("update_phase");
        assert_eq!(updated.owner, "team:platform");
    }

    #[test]
    fn update_phase_rejects_empty_owner() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");

        let err = store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                owner: Some(String::new()),
                ..Default::default()
            })
            .expect_err("empty owner update");
        assert!(
            err.to_string().contains("owner must not be empty"),
            "got: {err}"
        );
    }

    #[test]
    fn update_phase_leaves_owner_when_none() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Spec".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "team:frontend".to_owned(),
            })
            .expect("add_phase");

        let updated = store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: Some("Renamed".to_owned()),
                owner: None,
                ..Default::default()
            })
            .expect("update_phase");
        assert_eq!(updated.title, "Renamed");
        assert_eq!(updated.owner, "team:frontend");
    }

    #[test]
    fn read_phase_with_missing_owner_defaults_empty() {
        let (tmp, store) = fresh_corpus();
        let project = seed_project(&store, "alpha");
        let phs_id = new_id("phs");
        let phases_dir = tmp.path().join("projects").join("alpha").join("phases");
        fs::create_dir_all(&phases_dir).expect("mkdir phases");

        let file_body = format!(
            "---
id: {phs_id}
project: {}
slug: legacy-owner
title: Legacy
order: 1
status: pending
created_at: 2026-05-10T14:30:00Z
updated_at: 2026-05-10T14:30:00Z
created_by: human:test
---
",
            project.id
        );
        fs::write(phases_dir.join("01-legacy-owner.md"), file_body).expect("write legacy phase");

        let phases = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let p = phases
            .iter()
            .find(|ph| ph.slug == "legacy-owner")
            .expect("legacy phase readable");
        assert_eq!(p.owner, "");
    }

    #[test]
    fn read_phase_with_missing_created_by_defaults_gracefully() {
        let (tmp, store) = fresh_corpus();
        let project = seed_project(&store, "alpha");
        let phs_id = new_id("phs");
        let phases_dir = tmp.path().join("projects").join("alpha").join("phases");
        fs::create_dir_all(&phases_dir).expect("mkdir phases");

        let file_body = format!(
            "---\nid: {phs_id}\nproject: {}\nslug: legacy\ntitle: Legacy\norder: 1\nstatus: pending\ncreated_at: 2026-05-10T14:30:00Z\nupdated_at: 2026-05-10T14:30:00Z\n---\n",
            project.id
        );
        fs::write(phases_dir.join("01-legacy.md"), file_body).expect("write legacy phase");

        let phases = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let p = phases
            .iter()
            .find(|ph| ph.slug == "legacy")
            .expect("legacy phase readable");
        assert_eq!(p.created_by, "unknown");
        assert_eq!(&p.id, &phs_id);
    }

    #[test]
    fn add_phase_appends_to_end() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let p1 = add_phase_simple(&store, "alpha", "spec");
        let p2 = add_phase_simple(&store, "alpha", "build");
        let p3 = add_phase_simple(&store, "alpha", "ship");

        assert_eq!(p1.order, 1);
        assert_eq!(p2.order, 2);
        assert_eq!(p3.order, 3);

        let listed = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let slugs: Vec<&str> = listed.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(slugs, vec!["spec", "build", "ship"]);
    }

    #[test]
    fn add_phase_inserts_after_and_shifts() {
        let (tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec"); // order 1
        add_phase_simple(&store, "alpha", "build"); // order 2
        add_phase_simple(&store, "alpha", "ship"); // order 3

        // Insert after "spec": new phase becomes order 2; build/ship shift to 3/4.
        let inserted = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "design".to_owned(),
                title: "design".to_owned(),
                body: String::new(),
                after_phase: Some("spec".to_owned()),
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .unwrap();
        assert_eq!(inserted.order, 2);

        let listed = store.list_phases(&phase_filter_for("alpha")).unwrap();
        let pairs: Vec<(&str, i32)> = listed.iter().map(|p| (p.slug.as_str(), p.order)).collect();
        assert_eq!(
            pairs,
            vec![("spec", 1), ("design", 2), ("build", 3), ("ship", 4)]
        );

        // Files on disk reflect the new ordering.
        let phases_dir = tmp.path().join("projects").join("alpha").join("phases");
        let mut names: Vec<String> = fs::read_dir(&phases_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
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

    #[test]
    fn add_phase_rejects_duplicate_slug() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");

        let err = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Another spec".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect_err("duplicate slug");
        assert!(
            err.to_string().contains("phase slug already exists"),
            "got: {err}"
        );
    }

    #[test]
    fn add_phase_rejects_unknown_after_phase() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");

        let err = store
            .add_phase(&NewPhase {
                project: "alpha".to_owned(),
                slug: "design".to_owned(),
                title: "design".to_owned(),
                body: String::new(),
                after_phase: Some("nonexistent".to_owned()),
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect_err("unknown after_phase");
        assert!(
            err.to_string().contains("after_phase not found"),
            "got: {err}"
        );
    }

    #[test]
    fn add_phase_rejects_unknown_project() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .add_phase(&NewPhase {
                project: "ghost".to_owned(),
                slug: "spec".to_owned(),
                title: "spec".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect_err("unknown project");
        assert!(err.to_string().contains("project not found"), "got: {err}");
    }

    #[test]
    fn update_phase_preserves_id_order_created_at() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let original = add_phase_simple(&store, "alpha", "spec");

        std::thread::sleep(std::time::Duration::from_millis(10));

        let updated = store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: Some("Spec v2".to_owned()),
                body: Some("acceptance criteria here".to_owned()),
                status: Some(PhaseStatus::Active),
                owner: None,
            })
            .unwrap();

        assert_eq!(updated.id, original.id, "id changed");
        assert_eq!(updated.order, original.order, "order changed");
        assert_eq!(
            updated.created_at, original.created_at,
            "created_at changed"
        );
        assert!(
            updated.updated_at > original.updated_at,
            "updated_at not bumped"
        );
        assert_eq!(updated.title, "Spec v2");
        assert_eq!(updated.body, "acceptance criteria here");
        assert_eq!(updated.status, PhaseStatus::Active);

        // Confirm round-trip.
        let listed = store.list_phases(&phase_filter_for("alpha")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "Spec v2");
        assert_eq!(listed[0].status, PhaseStatus::Active);
    }

    #[test]
    fn update_phase_unknown_errors() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let err = store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "ghost".to_owned(),
                title: Some("x".to_owned()),
                ..Default::default()
            })
            .expect_err("missing phase");
        assert!(err.to_string().contains("phase not found"), "got: {err}");
    }

    fn seed_task(store: &FsStore, project: &str, slug: &str) -> Task {
        store
            .create_task(NewTask {
                project: project.to_owned(),
                phase: None,
                slug: slug.to_owned(),
                title: format!("Task {slug}"),
                body: "spec body".to_owned(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect("create task")
    }

    fn claim(store: &FsStore, id: &str, actor: &str) -> Task {
        store
            .claim_task(&ClaimTask {
                id: id.to_owned(),
                actor: actor.to_owned(),
            })
            .expect("claim task")
    }

    fn advance_to_in_progress(store: &FsStore, id: &str, actor: &str) -> Task {
        claim(store, id, actor);
        store
            .update_task(UpdateTask {
                id: id.to_owned(),
                status: Some(TaskStatus::InProgress),
                actor: actor.to_owned(),
                ..Default::default()
            })
            .expect("advance to in_progress")
    }

    #[test]
    fn task_create_persists_depends_on() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let dep = "tsk_01KSDVQST4YP73CYV9K7G7GZ8G".to_owned();
        let created = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "blocked-work".to_owned(),
                title: "Blocked work".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: vec![dep.clone()],
            })
            .expect("create_task");
        assert_eq!(created.depends_on, vec![dep.clone()]);

        let listed = store.list_tasks(&task_filter_for("alpha")).unwrap();
        let round_trip = listed
            .iter()
            .find(|t| t.slug == "blocked-work")
            .expect("task listed");
        assert_eq!(round_trip.depends_on, vec![dep]);
    }

    #[test]
    fn task_create_defaults_depends_on_empty() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "no-deps");
        assert!(task.depends_on.is_empty());
    }

    #[test]
    fn task_update_replaces_depends_on() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "dep-target");

        let updated = store
            .update_task(UpdateTask {
                id: task.id,
                depends_on: Some(vec!["tsk_FOO".to_owned()]),
                actor: "human:test".to_owned(),
                ..Default::default()
            })
            .expect("update_task");
        assert_eq!(updated.depends_on, vec!["tsk_FOO".to_owned()]);
    }

    #[test]
    fn task_update_clears_depends_on_with_empty_list() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "had-deps".to_owned(),
                title: "Had deps".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: vec!["tsk_BAR".to_owned()],
            })
            .expect("create_task");

        let updated = store
            .update_task(UpdateTask {
                id: task.id,
                depends_on: Some(vec![]),
                actor: "human:test".to_owned(),
                ..Default::default()
            })
            .expect("update_task");
        assert!(updated.depends_on.is_empty());
    }

    #[test]
    fn task_update_leaves_depends_on_when_none() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "keep-deps".to_owned(),
                title: "Keep deps".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: vec!["tsk_KEEP".to_owned()],
            })
            .expect("create_task");

        let updated = store
            .update_task(UpdateTask {
                id: task.id,
                body: Some("new body".to_owned()),
                depends_on: None,
                actor: "human:test".to_owned(),
                ..Default::default()
            })
            .expect("update_task");
        assert_eq!(updated.body, "new body");
        assert_eq!(updated.depends_on, vec!["tsk_KEEP".to_owned()]);
    }

    #[test]
    fn read_task_with_missing_depends_on_defaults_empty() {
        let (tmp, store) = fresh_corpus();
        let project = seed_project(&store, "alpha");
        let tsk_id = new_id("tsk");
        let tasks_dir = tmp.path().join("projects").join("alpha").join("tasks");
        fs::create_dir_all(&tasks_dir).expect("mkdir tasks");

        let file_body = format!(
            "---
id: {tsk_id}
project: {}
slug: legacy-deps
title: Legacy
status: todo
created_at: 2026-05-10T14:30:00Z
updated_at: 2026-05-10T14:30:00Z
---
",
            project.id
        );
        fs::write(
            tasks_dir.join(task_filename(&tsk_id, "legacy-deps")),
            file_body,
        )
        .expect("write legacy task");

        let tasks = store.list_tasks(&task_filter_for("alpha")).unwrap();
        let t = tasks
            .iter()
            .find(|tk| tk.slug == "legacy-deps")
            .expect("legacy task readable");
        assert!(t.depends_on.is_empty());
    }

    #[test]
    fn task_with_empty_depends_on_omits_field_on_write() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "no-dep-field");
        let (_proj, path) = store.find_task_path(&task.id).unwrap();
        let raw = fs::read_to_string(&path).expect("read task file");
        assert!(
            !raw.contains("depends_on:"),
            "empty depends_on must be omitted from frontmatter"
        );
    }

    #[test]
    fn create_task_round_trip() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let task = seed_task(&store, "alpha", "write-protocol");
        assert!(task.id.starts_with("tsk_"));
        assert_eq!(task.slug, "write-protocol");
        assert_eq!(task.status, TaskStatus::Todo);
        assert!(task.assignee.is_empty());
        assert!(task.claimed_at.is_none());
        assert!(task.completed_at.is_none());

        let listed = store.list_tasks(&task_filter_for("alpha")).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, task.id);
        assert!(listed[0].body.contains("spec body"));
    }

    #[test]
    fn find_task_path_bails_on_duplicate_id_across_projects() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let task = seed_task(&store, "alpha", "original");
        let (_proj, alpha_path) = store.find_task_path(&task.id).unwrap();
        let beta_tasks = store.root().join("projects").join("beta").join("tasks");
        fs::create_dir_all(&beta_tasks).unwrap();
        let dup_path = beta_tasks.join(format!("{}-mirror.md", task.id));
        fs::copy(&alpha_path, &dup_path).unwrap();
        let err = store
            .find_task_path(&task.id)
            .expect_err("duplicate task id must be rejected");
        assert!(err.to_string().contains("duplicate task id"), "got: {err}");
    }

    #[test]
    fn get_task_hits_across_projects() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let task = seed_task(&store, "alpha", "cross-lookup");
        let got = store
            .get_task(&task.id)
            .expect("get_task")
            .expect("task in alpha must resolve without naming project");
        assert_eq!(got.id, task.id);
        assert_eq!(got.slug, task.slug);
        assert_eq!(got.title, task.title);
    }

    #[test]
    fn get_task_errors_on_unknown_id() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let unknown = "tsk_01KRSZG60JG3S0JF294AA3459V";
        assert!(
            store.get_task(unknown).expect("get_task").is_none(),
            "well-formed id with no file must be None"
        );
    }

    #[test]
    fn create_task_resolves_phase_slug_to_id() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let phase = add_phase_simple(&store, "alpha", "spec");

        let task = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("spec".to_owned()),
                slug: "draft".to_owned(),
                title: "Draft".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .unwrap();
        assert_eq!(task.phase, phase.id, "phase slug should resolve to id");
    }

    #[test]
    fn create_task_rejects_duplicate_slug() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_task(&store, "alpha", "write-protocol");

        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "write-protocol".to_owned(),
                title: "dup".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("duplicate slug");
        assert!(
            err.to_string().contains("task slug already exists"),
            "got: {err}"
        );
    }

    #[test]
    fn create_task_rejects_unknown_project_and_phase() {
        let (_tmp, store) = fresh_corpus();

        let err = store
            .create_task(NewTask {
                project: "ghost".to_owned(),
                phase: None,
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("unknown project");
        assert!(err.to_string().contains("project not found"), "got: {err}");

        seed_project(&store, "alpha");
        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("ghost".to_owned()),
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("unknown phase");
        assert!(err.to_string().contains("phase not found"), "got: {err}");
    }

    #[test]
    fn create_task_rejects_traversal_in_project_or_phase() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let err = store
            .create_task(NewTask {
                project: "../../etc".to_owned(),
                phase: None,
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("project slug must be validated");
        assert!(err.to_string().contains("invalid slug"), "got: {err}");

        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("../sneaky".to_owned()),
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("phase slug must be validated");
        assert!(
            err.to_string().contains("phase slug must be lowercase"),
            "got: {err}"
        );
    }

    #[test]
    fn update_task_rejects_claimed_done_even_when_already_in_state() {
        // Reordering check: `from == to` short-circuit must not let
        // status=claimed / status=done through under any circumstances.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "guard");
        claim(&store, &task.id, "ship");

        // Task is now in `claimed`. update with status=claimed must still fail.
        let err = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Claimed),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("status=claimed must be rejected even when already claimed");
        assert!(err.to_string().contains("use task.claim"), "got: {err}");

        // Move to in_progress, complete, then attempt update with status=done.
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::InProgress),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        store
            .complete_task(CompleteTask {
                id: task.id.clone(),
                note: None,
                actor: "ship".to_owned(),
            })
            .unwrap();
        let err = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::Done),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("status=done must be rejected even when already done");
        assert!(err.to_string().contains("use task.complete"), "got: {err}");
    }

    #[test]
    fn task_notes_round_trip_through_api() {
        // After append, the next `list_tasks` should expose the note via
        // `Task.notes` so API consumers can read the log.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "logged");

        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                note: Some("first progress".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                note: Some("second progress".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();

        let tasks = store.list_tasks(&task_filter_for("alpha")).unwrap();
        let logged = tasks
            .iter()
            .find(|t| t.id == task.id)
            .expect("task in list");
        assert_eq!(logged.notes.len(), 2, "notes should be parsed from disk");
        assert_eq!(logged.notes[0].actor, "ship");
        assert_eq!(logged.notes[0].body, "first progress");
        assert_eq!(logged.notes[1].body, "second progress");
    }

    #[test]
    fn claim_task_happy_path() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        // Force a measurable gap so updated_at comparisons aren't flaky
        // on systems where consecutive Utc::now() calls can land in the
        // same monotonic tick (e.g. coarse-grained Windows clocks).
        std::thread::sleep(std::time::Duration::from_millis(10));

        let claimed = claim(&store, &task.id, "ship");
        assert_eq!(claimed.status, TaskStatus::Claimed);
        assert_eq!(claimed.assignee, "ship");
        assert!(claimed.claimed_at.is_some());
        assert!(
            claimed.updated_at > task.updated_at,
            "updated_at should bump"
        );
    }

    #[test]
    fn claim_task_same_actor_is_noop() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");
        let first = claim(&store, &task.id, "ship");

        std::thread::sleep(std::time::Duration::from_millis(10));

        let second = claim(&store, &task.id, "ship");
        assert_eq!(
            second.updated_at, first.updated_at,
            "re-claim should NOT bump updated_at"
        );
        assert_eq!(second.claimed_at, first.claimed_at, "claimed_at preserved");
        assert_eq!(second.assignee, "ship");
    }

    #[test]
    fn claim_task_different_actor_errors() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");
        claim(&store, &task.id, "ship");

        let err = store
            .claim_task(&ClaimTask {
                id: task.id,
                actor: "claude-code:michael".to_owned(),
            })
            .expect_err("different actor");
        assert!(
            err.to_string().contains("already claimed by ship"),
            "got: {err}"
        );
    }

    #[test]
    fn claim_task_terminal_errors() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");
        advance_to_in_progress(&store, &task.id, "ship");
        store
            .complete_task(CompleteTask {
                id: task.id.clone(),
                note: None,
                actor: "ship".to_owned(),
            })
            .unwrap();

        let err = store
            .claim_task(&ClaimTask {
                id: task.id,
                actor: "ship".to_owned(),
            })
            .expect_err("terminal state");
        assert!(
            err.to_string()
                .contains("cannot claim task in terminal state"),
            "got: {err}"
        );
    }

    #[test]
    fn update_task_allows_legal_transitions() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");
        claim(&store, &task.id, "ship");

        // claimed -> in_progress
        let ip = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::InProgress),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(ip.status, TaskStatus::InProgress);

        // in_progress -> blocked
        let blocked = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Blocked),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(blocked.status, TaskStatus::Blocked);

        // blocked -> in_progress
        let back = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::InProgress),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(back.status, TaskStatus::InProgress);

        // in_progress -> cancelled
        let cancelled = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::Cancelled),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(cancelled.status, TaskStatus::Cancelled);
    }

    #[test]
    fn update_task_rejects_claimed_and_done_targets() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        let err = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Claimed),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("update -> claimed forbidden");
        assert!(err.to_string().contains("use task.claim"), "got: {err}");

        claim(&store, &task.id, "ship");
        let err = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::Done),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("update -> done forbidden");
        assert!(err.to_string().contains("use task.complete"), "got: {err}");
    }

    #[test]
    fn update_task_rejects_terminal_transitions() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        // Cancel the task, then try to move it elsewhere.
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Cancelled),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();

        let err = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::InProgress),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("terminal -> anything forbidden");
        assert!(err.to_string().contains("terminal state"), "got: {err}");
        assert!(
            err.to_string().contains("cancelled"),
            "expected wire status name surfaced in terminal transition error ({err})"
        );
    }

    #[test]
    fn update_task_rejects_invalid_transition() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        // todo -> in_progress is not a legal update (must claim first)
        let err = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::InProgress),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("todo -> in_progress without claim");
        assert!(
            err.to_string().contains("invalid task transition"),
            "got: {err}"
        );
    }

    #[test]
    fn update_task_note_appends_to_notes_section() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                note: Some("first note".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                note: Some("second note".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();

        // Read the raw file to inspect the Notes section.
        let (proj, path) = store.find_task_path(&task.id).unwrap();
        assert_eq!(proj, "alpha");
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("## Notes"), "expected ## Notes section");
        assert!(raw.contains("first note"), "first note missing");
        assert!(raw.contains("second note"), "second note missing");
        assert!(raw.contains("— ship:"), "actor missing in note line: {raw}");
    }

    #[test]
    fn complete_task_happy_path() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");
        advance_to_in_progress(&store, &task.id, "ship");

        let done = store
            .complete_task(CompleteTask {
                id: task.id,
                note: Some("shipped".to_owned()),
                actor: "ship".to_owned(),
            })
            .unwrap();
        assert_eq!(done.status, TaskStatus::Done);
        assert!(done.completed_at.is_some());
    }

    #[test]
    fn complete_task_rejects_non_in_progress() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "write-protocol");

        // todo → complete: error
        let err = store
            .complete_task(CompleteTask {
                id: task.id.clone(),
                note: None,
                actor: "ship".to_owned(),
            })
            .expect_err("complete from todo");
        assert!(
            err.to_string().contains("must be in_progress"),
            "got: {err}"
        );

        // claimed → complete: error
        claim(&store, &task.id, "ship");
        let err = store
            .complete_task(CompleteTask {
                id: task.id,
                note: None,
                actor: "ship".to_owned(),
            })
            .expect_err("complete from claimed");
        assert!(
            err.to_string().contains("must be in_progress"),
            "got: {err}"
        );
    }

    #[test]
    fn find_task_walks_projects() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let task = seed_task(&store, "beta", "deep-task");

        let (proj, path) = store.find_task_path(&task.id).unwrap();
        assert_eq!(proj, "beta");
        assert!(path.to_string_lossy().contains("beta"));
    }

    #[test]
    fn task_round_trip_preserves_notes() {
        // Full lifecycle: create → claim → update(in_progress, note) →
        // update(blocked, note) → update(in_progress, note) → complete(note).
        // Verify the final on-disk file has all notes in order.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "lifecycle");

        claim(&store, &task.id, "ship");
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::InProgress),
                note: Some("started".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Blocked),
                note: Some("waiting on review".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::InProgress),
                note: Some("unblocked".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        let done = store
            .complete_task(CompleteTask {
                id: task.id.clone(),
                note: Some("shipped".to_owned()),
                actor: "ship".to_owned(),
            })
            .unwrap();
        assert_eq!(done.status, TaskStatus::Done);

        let (_proj, path) = store.find_task_path(&task.id).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        for snippet in ["started", "waiting on review", "unblocked", "shipped"] {
            assert!(raw.contains(snippet), "note missing: {snippet}\n{raw}");
        }
        // Order check: started should precede shipped.
        let started_idx = raw.find("started").unwrap();
        let shipped_idx = raw.find("shipped").unwrap();
        assert!(started_idx < shipped_idx, "notes out of order: {raw}");
    }

    #[test]
    fn update_task_rejects_terminal_idempotent_status() {
        // `cancelled → cancelled` is not a transition, but the terminal
        // guard must still reject it — terminal means nothing writes.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "to-cancel");

        store
            .update_task(UpdateTask {
                id: task.id.clone(),
                status: Some(TaskStatus::Cancelled),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();

        let err = store
            .update_task(UpdateTask {
                id: task.id,
                status: Some(TaskStatus::Cancelled),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("cancelled→cancelled must be rejected");
        assert!(err.to_string().contains("terminal state"), "got: {err}");
    }

    #[test]
    fn update_task_note_appears_in_returned_task() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "logged");

        let returned = store
            .update_task(UpdateTask {
                id: task.id,
                note: Some("first progress".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            returned.notes.len(),
            1,
            "appended note must appear in the returned Task"
        );
        assert_eq!(returned.notes[0].actor, "ship");
        assert_eq!(returned.notes[0].body, "first progress");
    }

    #[test]
    fn complete_task_note_appears_in_returned_task() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "to-complete");
        advance_to_in_progress(&store, &task.id, "ship");

        let returned = store
            .complete_task(CompleteTask {
                id: task.id,
                note: Some("shipped".to_owned()),
                actor: "ship".to_owned(),
            })
            .unwrap();
        assert_eq!(returned.notes.len(), 1);
        assert_eq!(returned.notes[0].body, "shipped");
    }

    #[test]
    fn note_input_rejects_newlines_and_empty() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "guarded");

        for bad in ["", "   ", "\n"] {
            let err = store
                .update_task(UpdateTask {
                    id: task.id.clone(),
                    note: Some(bad.to_owned()),
                    actor: "ship".to_owned(),
                    ..Default::default()
                })
                .expect_err("bad note input must be rejected");
            assert!(
                err.to_string().contains("note ")
                    && (err.to_string().contains("single-line")
                        || err.to_string().contains("must not be empty")),
                "unexpected error for input {bad:?}: {err}"
            );
        }

        let err = store
            .update_task(UpdateTask {
                id: task.id.clone(),
                note: Some("line1\nline2".to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("multi-line note must be rejected");
        assert!(err.to_string().contains("single-line"), "got: {err}");

        let err = store
            .update_task(UpdateTask {
                id: task.id,
                note: Some("ok".to_owned()),
                actor: "act\nor".to_owned(),
                ..Default::default()
            })
            .expect_err("multi-line actor must be rejected");
        assert!(err.to_string().contains("single-line"), "got: {err}");
    }

    #[test]
    fn claim_task_corrupt_todo_with_assignee_errors() {
        // Manually craft a corrupt task file (status=todo + assignee
        // set) and confirm claim_task surfaces it rather than silently
        // no-op'ing on same-actor.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "corrupt");
        let (_proj, path) = store.find_task_path(&task.id).unwrap();

        // Inject assignee while leaving status=todo.
        let raw = fs::read_to_string(&path).unwrap();
        let injected = raw.replace("status: todo\n", "status: todo\nassignee: ship\n");
        fs::write(&path, injected).unwrap();

        let err = store
            .claim_task(&ClaimTask {
                id: task.id,
                actor: "ship".to_owned(),
            })
            .expect_err("todo+assignee should surface as corrupt");
        assert!(err.to_string().contains("corrupt state"), "got: {err}");
    }

    #[test]
    fn create_task_rejects_empty_actor() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: String::new(),
                depends_on: Vec::new(),
            })
            .expect_err("empty actor must be rejected");
        assert!(err.to_string().contains("actor is required"), "got: {err}");
    }

    #[test]
    fn task_body_rejects_notes_heading() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let bad_body = "## Spec\n\nDo the thing.\n\n## Notes\n\nlater\n";
        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: None,
                slug: "collides".to_owned(),
                title: "x".to_owned(),
                body: bad_body.to_owned(),
                actor: "ship".to_owned(),
                depends_on: Vec::new(),
            })
            .expect_err("body with ## Notes must be rejected");
        assert!(err.to_string().contains("## Notes"), "got: {err}");

        // Update path also guarded.
        let task = seed_task(&store, "alpha", "good");
        let err = store
            .update_task(UpdateTask {
                id: task.id,
                body: Some(bad_body.to_owned()),
                actor: "ship".to_owned(),
                ..Default::default()
            })
            .expect_err("update body with ## Notes must be rejected");
        assert!(err.to_string().contains("## Notes"), "got: {err}");
    }

    #[test]
    fn claim_task_corrupt_todo_with_assignee_different_actor() {
        // Same corrupt-state injection as the same-actor variant, but
        // the claim attempt comes from a different actor. Should surface
        // as corrupt state, not as "already claimed".
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "corrupt-diff");
        let (_proj, path) = store.find_task_path(&task.id).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let injected = raw.replace("status: todo\n", "status: todo\nassignee: ship\n");
        fs::write(&path, injected).unwrap();

        let err = store
            .claim_task(&ClaimTask {
                id: task.id,
                actor: "different-actor".to_owned(),
            })
            .expect_err("todo+assignee should surface as corrupt regardless of actor");
        assert!(err.to_string().contains("corrupt state"), "got: {err}");
    }

    #[test]
    fn append_note_rejects_actor_with_delimiter() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "delim-actor");

        let err = store
            .update_task(UpdateTask {
                id: task.id,
                note: Some("body".to_owned()),
                actor: "ship: rogue".to_owned(),
                ..Default::default()
            })
            .expect_err("actor containing ': ' must be rejected");
        assert!(
            err.to_string().contains("actor must not contain"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_note_line_handles_formats_and_edge_cases() {
        let canonical = "- 2026-05-10T15:30:00Z — ship: did the thing";
        let n = parse_note_line(canonical).expect("canonical line parses");
        assert_eq!(n.actor, "ship");
        assert_eq!(n.body, "did the thing");

        let legacy = "- 2026-05-10 — claude-code:michael: claimed";
        let n = parse_note_line(legacy).expect("date-only legacy line parses");
        assert_eq!(n.actor, "claude-code:michael");
        assert_eq!(n.body, "claimed");

        // Bodies may contain `:` after the actor split.
        let colons = "- 2026-05-10T15:30:00Z — ship: needs follow-up: also see #42";
        let n = parse_note_line(colons).expect("body with colons parses");
        assert_eq!(n.actor, "ship");
        assert_eq!(n.body, "needs follow-up: also see #42");

        // Unparseable inputs should return None, not panic.
        assert!(parse_note_line("").is_none());
        assert!(parse_note_line("not a note line").is_none());
        assert!(parse_note_line("- 2026-05-10T15:30:00Z ship: no em dash").is_none());
        assert!(parse_note_line("- not-a-date — ship: body").is_none());
        assert!(parse_note_line("  ").is_none());
    }

    fn link_simple(
        store: &FsStore,
        project: &str,
        task: Option<&str>,
        kind: &str,
        reference: &str,
        label: &str,
    ) -> Artifact {
        store
            .link_artifact(LinkArtifact {
                project: project.to_owned(),
                task: task.map(str::to_owned),
                kind: kind.to_owned(),
                reference: reference.to_owned(),
                label: label.to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect("link artifact")
    }

    #[test]
    fn link_artifact_project_wide_round_trip() {
        let (_tmp, store) = fresh_corpus();
        let project = seed_project(&store, "alpha");

        let art = link_simple(&store, "alpha", None, "pr", "https://example/pr/7", "PR #7");
        assert!(art.id.starts_with("art_"));
        assert_eq!(art.project, project.id);
        assert!(art.task.is_empty());
        assert_eq!(art.kind, "pr");
        assert_eq!(art.reference, "https://example/pr/7");
        assert_eq!(art.label, "PR #7");
        assert_eq!(art.actor, "human:test");

        let listed = store.list_artifacts("alpha").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, art.id);
    }

    #[test]
    fn link_artifact_anchored_to_task() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "implement-x");

        let art = link_simple(
            &store,
            "alpha",
            Some(&task.id),
            "commit",
            "abc123",
            "first commit",
        );
        assert_eq!(art.task, task.id);

        let listed = store.list_artifacts("alpha").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].task, task.id);
    }

    #[test]
    fn link_artifact_appends_in_order() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let a = link_simple(&store, "alpha", None, "pr", "https://example/pr/1", "PR #1");
        let b = link_simple(&store, "alpha", None, "pr", "https://example/pr/2", "PR #2");
        let c = link_simple(&store, "alpha", None, "pr", "https://example/pr/3", "PR #3");

        let listed = store.list_artifacts("alpha").unwrap();
        let ids: Vec<&str> = listed.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec![a.id.as_str(), b.id.as_str(), c.id.as_str()]);
    }

    #[test]
    fn link_artifact_same_tuple_is_idempotent() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        let first = link_simple(&store, "alpha", None, "pr", "https://example/pr/1", "PR #1");
        let second = link_simple(
            &store,
            "alpha",
            None,
            "pr",
            "https://example/pr/1",
            "different label",
        );

        assert_eq!(first.id, second.id);
        let listed = store.list_artifacts("alpha").unwrap();
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn link_artifact_different_ref_appends() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        link_simple(&store, "alpha", None, "pr", "https://example/pr/1", "PR #1");
        link_simple(&store, "alpha", None, "pr", "https://example/pr/2", "PR #2");

        let listed = store.list_artifacts("alpha").unwrap();
        assert_eq!(listed.len(), 2);
    }

    #[test]
    fn link_artifact_rejects_unknown_project() {
        let (_tmp, store) = fresh_corpus();
        let err = store
            .link_artifact(LinkArtifact {
                project: "ghost".to_owned(),
                task: None,
                kind: "pr".to_owned(),
                reference: "x".to_owned(),
                label: "x".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect_err("unknown project");
        assert!(err.to_string().contains("project not found"), "got: {err}");
    }

    #[test]
    fn link_artifact_rejects_task_in_wrong_project() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let alpha_task = seed_task(&store, "alpha", "owned-by-alpha");

        let err = store
            .link_artifact(LinkArtifact {
                project: "beta".to_owned(),
                task: Some(alpha_task.id),
                kind: "pr".to_owned(),
                reference: "x".to_owned(),
                label: "x".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect_err("cross-project task should be rejected");
        assert!(
            err.to_string().contains("not found in project beta"),
            "got: {err}"
        );
    }

    #[test]
    fn link_artifact_rejects_unknown_task() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let err = store
            .link_artifact(LinkArtifact {
                project: "alpha".to_owned(),
                task: Some("tsk_does_not_exist".to_owned()),
                kind: "pr".to_owned(),
                reference: "x".to_owned(),
                label: "x".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect_err("unknown task should be rejected");
        assert!(
            err.to_string().contains("not found in project alpha"),
            "got: {err}"
        );
    }

    #[test]
    fn link_artifact_rejects_empty_task_string() {
        // Some(\"\") is a caller bug — telling them to omit the field is
        // friendlier than treating it as a project-wide artifact.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let err = store
            .link_artifact(LinkArtifact {
                project: "alpha".to_owned(),
                task: Some(String::new()),
                kind: "pr".to_owned(),
                reference: "x".to_owned(),
                label: "x".to_owned(),
                actor: "human:test".to_owned(),
            })
            .expect_err("Some(empty) task must be rejected");
        assert!(err.to_string().contains("task is empty"), "got: {err}");
    }

    #[test]
    fn link_artifact_rejects_required_field_emptiness() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");

        // Each required field, blanked one at a time. Builds a fresh
        // baseline before each mutation so a single failure doesn't
        // mask later ones.
        let baseline = || LinkArtifact {
            project: "alpha".to_owned(),
            task: None,
            kind: "pr".to_owned(),
            reference: "x".to_owned(),
            label: "x".to_owned(),
            actor: "human:test".to_owned(),
        };
        let mut a = baseline();
        a.actor = String::new();
        assert!(store
            .link_artifact(a)
            .expect_err("empty actor")
            .to_string()
            .contains("actor is required"));
        let mut a = baseline();
        a.project = String::new();
        assert!(store
            .link_artifact(a)
            .expect_err("empty project")
            .to_string()
            .contains("project is required"));
        let mut a = baseline();
        a.kind = String::new();
        assert!(store
            .link_artifact(a)
            .expect_err("empty kind")
            .to_string()
            .contains("kind is required"));
        let mut a = baseline();
        a.reference = String::new();
        assert!(store
            .link_artifact(a)
            .expect_err("empty ref")
            .to_string()
            .contains("ref is required"));
        let mut a = baseline();
        a.label = String::new();
        assert!(store
            .link_artifact(a)
            .expect_err("empty label")
            .to_string()
            .contains("label is required"));
    }

    #[test]
    fn link_artifact_rejects_newlines_in_text_fields() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        for field in ["kind", "ref", "label", "actor"] {
            let mut a = LinkArtifact {
                project: "alpha".to_owned(),
                task: None,
                kind: "pr".to_owned(),
                reference: "x".to_owned(),
                label: "x".to_owned(),
                actor: "human:test".to_owned(),
            };
            match field {
                "kind" => a.kind = "p\nr".to_owned(),
                "ref" => a.reference = "line1\nline2".to_owned(),
                "label" => a.label = "label\r\ninjected".to_owned(),
                "actor" => a.actor = "human\nadmin".to_owned(),
                _ => unreachable!(),
            }
            let err = store
                .link_artifact(a)
                .expect_err("newline must be rejected");
            assert!(
                err.to_string().contains("single-line"),
                "field={field}, got: {err}"
            );
        }
    }

    // =========================================================================
    // Filter expansion: per-predicate, combo, validation, dogfood
    // =========================================================================

    /// Overwrite a single frontmatter field on a task file. Used by
    /// filter tests to seed precise timestamps / assignees / bodies
    /// without driving the full state machine.
    fn set_task_field(store: &FsStore, task_id: &str, field: &str, value: &str) {
        use std::fmt::Write as _;
        let (_proj, path) = store.find_task_path(task_id).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        // Replace `<field>: <whatever>\n` lines in the frontmatter; insert
        // before `---` if the field is absent.
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
        fs::write(&path, new_raw).unwrap();
    }

    /// Overwrite the spec body of a task file (the part between the
    /// closing `---` and any `## Notes` section).
    fn set_task_body(store: &FsStore, task_id: &str, body: &str) {
        let (_proj, path) = store.find_task_path(task_id).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let parts: Vec<&str> = raw.splitn(3, "---").collect();
        assert_eq!(parts.len(), 3, "task file missing frontmatter delimiters");
        let after_front = parts[2];
        // Preserve the `## Notes` section if present.
        let notes = after_front
            .find("## Notes")
            .map_or("", |i| &after_front[i..]);
        let front = parts[1];
        let new_raw = if notes.is_empty() {
            format!("---{front}---\n\n{body}\n")
        } else {
            format!("---{front}---\n\n{body}\n\n{notes}")
        };
        fs::write(&path, new_raw).unwrap();
    }

    /// Seed a phase with a controlled body for `body_contains` tests.
    fn set_phase_body(store: &FsStore, project_slug: &str, phase_slug: &str, body: &str) {
        store
            .update_phase(UpdatePhase {
                project: project_slug.to_owned(),
                slug: phase_slug.to_owned(),
                body: Some(body.to_owned()),
                ..Default::default()
            })
            .unwrap();
    }

    /// Overwrite a frontmatter scalar on project.md (`created_at` /
    /// `updated_at`) for quadrant filter fixtures.
    fn set_project_field(store: &FsStore, slug: &str, field: &str, value: &str) {
        use std::fmt::Write as _;
        let path = store.root().join("projects").join(slug).join("project.md");
        let raw = fs::read_to_string(&path).unwrap();
        let needle = format!("{field}: ");
        let parts: Vec<&str> = raw.splitn(3, "---").collect();
        assert_eq!(
            parts.len(),
            3,
            "project file missing frontmatter delimiters"
        );
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
        fs::write(&path, new_raw).unwrap();
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn stub_task(id: &str, created_at: DateTime<Utc>) -> Task {
        Task {
            id: id.to_owned(),
            project: "alpha".to_owned(),
            project_slug: "alpha".to_owned(),
            phase: String::new(),
            slug: "stub".to_owned(),
            title: "stub".to_owned(),
            body: String::new(),
            status: TaskStatus::Todo,
            assignee: String::new(),
            claimed_at: None,
            completed_at: None,
            created_at,
            updated_at: created_at,
            notes: Vec::new(),
            depends_on: Vec::new(),
        }
    }

    fn stub_phase(id: &str, project_slug: &str, created_at: DateTime<Utc>) -> Phase {
        Phase {
            id: id.to_owned(),
            project: project_slug.to_owned(),
            slug: "stub".to_owned(),
            title: "stub".to_owned(),
            body: String::new(),
            order: 1_i32,
            status: PhaseStatus::Pending,
            created_at,
            updated_at: created_at,
            created_by: "unknown".to_owned(),
            owner: String::new(),
        }
    }

    fn stub_project(id: &str, slug: &str, created_at: DateTime<Utc>) -> Project {
        Project {
            id: id.to_owned(),
            slug: slug.to_owned(),
            title: "stub".to_owned(),
            description: String::new(),
            status: ProjectStatus::Planning,
            created_at,
            updated_at: created_at,
            created_by: String::new(),
        }
    }

    #[test]
    fn sort_tasks_ties_break_on_id_asc() {
        let ts = t("2026-06-06T06:06:06Z");
        let higher_id_first = stub_task("tsk_ZZZZZZZZZZZZZZZZZZZZZZZZ", ts);
        let lower_id_second = stub_task("tsk_AAAAAAAAAAAAAAAAAAAAAAAA", ts);
        let mut rows = vec![higher_id_first, lower_id_second];
        sort_tasks(
            &mut rows,
            &TaskListFilter {
                order_by: Some(TaskOrderField::CreatedAt),
                ..TaskListFilter::default()
            },
        );
        assert_eq!(rows[0].id.as_str(), "tsk_AAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(rows[1].id.as_str(), "tsk_ZZZZZZZZZZZZZZZZZZZZZZZZ");
    }

    #[test]
    fn sort_tasks_ties_break_on_id_desc() {
        let ts = t("2026-06-06T06:06:06Z");
        let higher_id = stub_task("tsk_ZZZZZZZZZZZZZZZZZZZZZZZZ", ts);
        let lower_id = stub_task("tsk_AAAAAAAAAAAAAAAAAAAAAAAA", ts);
        let mut rows = vec![higher_id, lower_id];
        sort_tasks(
            &mut rows,
            &TaskListFilter {
                order_by: Some(TaskOrderField::CreatedAt),
                desc: Some(true),
                ..TaskListFilter::default()
            },
        );
        // DESC reverses the ASC ordering, so the lower id lands last.
        assert_eq!(rows[0].id.as_str(), "tsk_ZZZZZZZZZZZZZZZZZZZZZZZZ");
        assert_eq!(rows[1].id.as_str(), "tsk_AAAAAAAAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn sort_phases_ties_break_on_id_asc() {
        let ts = t("2026-06-06T06:06:06Z");
        let higher_id_first = stub_phase("phs_ZZZZZZZZZZZZZZZZZZZZZZZZ", "alpha", ts);
        let lower_id_second = stub_phase("phs_AAAAAAAAAAAAAAAAAAAAAAAA", "alpha", ts);
        let mut rows = vec![higher_id_first, lower_id_second];
        sort_phases(
            &mut rows,
            &PhaseListFilter {
                order_by: Some(PhaseOrderField::CreatedAt),
                ..PhaseListFilter::default()
            },
        );
        assert_eq!(rows[0].id.as_str(), "phs_AAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(rows[1].id.as_str(), "phs_ZZZZZZZZZZZZZZZZZZZZZZZZ");
    }

    #[test]
    fn sort_projects_ties_break_on_id_asc() {
        let ts = t("2026-06-06T06:06:06Z");
        let higher_id_first = stub_project("prj_ZZZZZZZZZZZZZZZZZZZZZZZZ", "zed", ts);
        let lower_id_second = stub_project("prj_AAAAAAAAAAAAAAAAAAAAAAAA", "alef", ts);
        let mut rows = vec![higher_id_first, lower_id_second];
        sort_projects(
            &mut rows,
            &ProjectListFilter {
                order_by: Some(ProjectOrderField::CreatedAt),
                ..ProjectListFilter::default()
            },
        );
        assert_eq!(rows[0].id.as_str(), "prj_AAAAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(rows[1].id.as_str(), "prj_ZZZZZZZZZZZZZZZZZZZZZZZZ");
    }

    #[test]
    fn in_range_respects_exclusive_before_and_inclusive_after() {
        let t_anchor = DateTime::parse_from_rfc3339("2026-05-05T12:34:56.789Z")
            .unwrap()
            .with_timezone(&Utc);
        let epsilon = chrono::Duration::milliseconds(1);

        assert!(in_range(t_anchor, Some(t_anchor), None));
        assert!(!in_range(t_anchor - epsilon, Some(t_anchor), None));
        assert!(!in_range(t_anchor, None, Some(t_anchor)));
        assert!(in_range(t_anchor - epsilon, None, Some(t_anchor)));
    }

    #[test]
    fn list_tasks_boundary_queries_match_created_at_semantics() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "timed");
        set_task_field(&store, &task.id, "created_at", "2026-04-02T09:09:09Z");

        let boundary = DateTime::parse_from_rfc3339("2026-04-02T09:09:09Z")
            .unwrap()
            .with_timezone(&Utc);

        let after_min = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                created_after: Some(boundary),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(after_min.len(), 1, "`created_after` inclusive on equality");
        assert_eq!(after_min[0].id, task.id);

        let before_max = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                created_before: Some(boundary),
                ..Default::default()
            })
            .unwrap();
        assert!(
            before_max.is_empty(),
            "`created_before` strictly excludes equal boundary rows"
        );
    }

    #[test]
    fn list_projects_predicate_created_and_updated_and_together() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "hit");
        seed_project(&store, "old-new");
        seed_project(&store, "new-old");
        seed_project(&store, "neither");

        let m = t("2026-04-15T15:15:15Z");

        // Only `hit` satisfies BOTH `created_at >= m` and `updated_at >= m`.
        set_project_field(&store, "hit", "created_at", "2026-05-01T00:00:00Z");
        set_project_field(&store, "hit", "updated_at", "2026-08-08T08:08:08Z");

        set_project_field(&store, "old-new", "created_at", "2026-03-03T03:03:03Z");
        set_project_field(&store, "old-new", "updated_at", "2026-06-06T06:06:06Z");

        set_project_field(&store, "new-old", "created_at", "2026-06-06T06:06:06Z");
        set_project_field(&store, "new-old", "updated_at", "2026-02-02T02:02:02Z");

        set_project_field(&store, "neither", "created_at", "2026-01-01T00:00:00Z");
        set_project_field(&store, "neither", "updated_at", "2026-01-02T02:02:02Z");

        let hits = store
            .list_projects(&ProjectListFilter {
                created_after: Some(m),
                updated_after: Some(m),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "hit");
    }

    #[test]
    fn list_phases_matches_status_and_body_contains_intersection() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let _hit = add_phase_simple(&store, "alpha", "hit");
        let _active_plain = add_phase_simple(&store, "alpha", "active-plain");
        let _pending_design = add_phase_simple(&store, "alpha", "pending-design");
        let _cold = add_phase_simple(&store, "alpha", "cold");

        set_phase_body(&store, "alpha", "hit", "RFC for the design subsystem");
        set_phase_body(&store, "alpha", "active-plain", "no keyword here");
        set_phase_body(
            &store,
            "alpha",
            "pending-design",
            "review the design backlog",
        );
        set_phase_body(&store, "alpha", "cold", "nothing");

        store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "hit".to_owned(),
                status: Some(PhaseStatus::Active),
                ..Default::default()
            })
            .unwrap();

        store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "active-plain".to_owned(),
                status: Some(PhaseStatus::Active),
                ..Default::default()
            })
            .unwrap();

        store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "pending-design".to_owned(),
                status: Some(PhaseStatus::Pending),
                ..Default::default()
            })
            .unwrap();

        store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "cold".to_owned(),
                status: Some(PhaseStatus::Pending),
                ..Default::default()
            })
            .unwrap();

        let phases = store
            .list_phases(&PhaseListFilter {
                project: Some("alpha".to_owned()),
                status: Some(vec![PhaseStatus::Active]),
                body_contains: Some("DESIGN".to_owned()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].slug, "hit");
    }

    #[test]
    fn list_projects_matches_status_and_body_contains_intersection() {
        let (_tmp, store) = fresh_corpus();

        store
            .create_project(NewProject {
                slug: "hit".to_owned(),
                title: "Hit".to_owned(),
                description: "team CORE platform rollout".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        store
            .create_project(NewProject {
                slug: "active-no-core".to_owned(),
                title: "Bare".to_owned(),
                description: "edge tooling only".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        store
            .create_project(NewProject {
                slug: "paused-core-text".to_owned(),
                title: "Paused core".to_owned(),
                description: "contains CORE mention but wrong status fixture".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        store
            .create_project(NewProject {
                slug: "miss".to_owned(),
                title: "Miss".to_owned(),
                description: "unrelated backlog".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();

        store
            .update_project(UpdateProject {
                slug: "hit".to_owned(),
                status: Some(ProjectStatus::Active),
                ..Default::default()
            })
            .unwrap();

        store
            .update_project(UpdateProject {
                slug: "active-no-core".to_owned(),
                status: Some(ProjectStatus::Active),
                ..Default::default()
            })
            .unwrap();

        store
            .update_project(UpdateProject {
                slug: "paused-core-text".to_owned(),
                status: Some(ProjectStatus::Paused),
                ..Default::default()
            })
            .unwrap();

        store
            .update_project(UpdateProject {
                slug: "miss".to_owned(),
                status: Some(ProjectStatus::Done),
                ..Default::default()
            })
            .unwrap();

        let hits = store
            .list_projects(&ProjectListFilter {
                status: Some(vec![ProjectStatus::Active]),
                body_contains: Some("core".to_owned()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "hit");
    }

    #[test]
    fn list_tasks_matches_status_and_body_contains_intersection() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let ip_auth = seed_task(&store, "alpha", "ip-auth");
        let ip_plain = seed_task(&store, "alpha", "ip-plain");
        let done_auth = seed_task(&store, "alpha", "done-auth");
        let todo_plain = seed_task(&store, "alpha", "todo-plain");

        set_task_body(&store, &ip_auth.id, "finish auth hardening checklist");
        set_task_body(&store, &ip_plain.id, "tune deploy pipeline");
        set_task_body(&store, &done_auth.id, "close auth regressions batch");
        set_task_body(&store, &todo_plain.id, "stretch goal");

        set_task_field(&store, &ip_auth.id, "status", "in_progress");
        set_task_field(&store, &ip_auth.id, "assignee", "ship");

        set_task_field(&store, &ip_plain.id, "status", "in_progress");
        set_task_field(&store, &ip_plain.id, "assignee", "ship");

        set_task_field(&store, &done_auth.id, "status", "done");
        set_task_field(&store, &todo_plain.id, "status", "todo");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                status: Some(vec![TaskStatus::InProgress]),
                body_contains: Some("auth".to_owned()),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, ip_auth.slug);
    }

    #[test]
    fn list_tasks_completed_range_works_when_only_after_bound_given() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let winner = seed_task(&store, "alpha", "closed");
        set_task_field(&store, &winner.id, "completed_at", "2026-06-06T06:06:06Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                completed_after: Some(t("2026-06-05T06:06:06Z")),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, winner.id);
    }

    #[test]
    fn update_phase_errors_when_exactly_one_key_is_blank() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");

        for (project, slug) in [
            (String::new(), "spec".to_owned()),
            ("alpha".to_owned(), String::new()),
        ] {
            let err = store
                .update_phase(UpdatePhase {
                    project,
                    slug,
                    title: Some("x".to_owned()),
                    ..Default::default()
                })
                .expect_err("requires both keys");
            let msg = err.to_string();
            assert!(
                msg.contains("project and slug are required"),
                "unexpected message: {msg}"
            );
        }
    }

    #[test]
    fn load_task_strips_blank_lines_after_notes_heading() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let task = seed_task(&store, "alpha", "notes-shape");

        let (_proj_slug, path) = store.find_task_path(&task.id).unwrap();
        let mut raw = fs::read_to_string(&path).unwrap();
        raw.push_str("\n\n## Notes\n\n\n\r\n- 2026-01-01T00:00:00Z — actor: real note line\n");
        fs::write(path, raw).unwrap();

        let hits = store.list_tasks(&task_filter_for("alpha")).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].notes.len(),
            1,
            "leading blank noise must not swallow the canonical note row"
        );
        assert_eq!(hits[0].notes[0].body.as_str(), "real note line");
    }

    // ----- task.list per-predicate -----

    #[test]
    fn list_tasks_filter_by_assignee() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "alpha-task");
        let b = seed_task(&store, "alpha", "beta-task");
        set_task_field(&store, &a.id, "assignee", "human:mh");
        set_task_field(&store, &b.id, "assignee", "ship");

        let out = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                assignee: Some("human:mh".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, a.id);
    }

    #[test]
    fn list_tasks_filter_by_body_contains_case_insensitive() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "task-a");
        let b = seed_task(&store, "alpha", "task-b");
        set_task_body(&store, &a.id, "Look into Auth flows for OIDC");
        set_task_body(&store, &b.id, "Tune the build cache");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                body_contains: Some("auth".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);

        // Empty string is a no-op match (returns everything).
        let all = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                body_contains: Some(String::new()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn list_tasks_filter_by_created_range() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "early");
        let b = seed_task(&store, "alpha", "mid");
        let c = seed_task(&store, "alpha", "late");
        set_task_field(&store, &a.id, "created_at", "2026-01-01T00:00:00Z");
        set_task_field(&store, &b.id, "created_at", "2026-03-01T00:00:00Z");
        set_task_field(&store, &c.id, "created_at", "2026-05-01T00:00:00Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                created_after: Some(t("2026-02-01T00:00:00Z")),
                created_before: Some(t("2026-04-01T00:00:00Z")),
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec![b.id.as_str()]);
    }

    #[test]
    fn list_tasks_filter_by_updated_range() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "stale");
        let b = seed_task(&store, "alpha", "fresh");
        set_task_field(&store, &a.id, "updated_at", "2026-01-01T00:00:00Z");
        set_task_field(&store, &b.id, "updated_at", "2026-05-10T00:00:00Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                updated_after: Some(t("2026-05-01T00:00:00Z")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, b.id);
    }

    #[test]
    fn list_tasks_filter_by_completed_range_drops_nulls() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "done");
        let _b = seed_task(&store, "alpha", "still-open");
        set_task_field(&store, &a.id, "completed_at", "2026-05-10T00:00:00Z");
        // _b has no completed_at — should be dropped from a completed-range query.

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                completed_after: Some(t("2026-05-01T00:00:00Z")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);
    }

    #[test]
    fn list_tasks_filter_by_claimed_range_drops_nulls() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "claimed");
        let _b = seed_task(&store, "alpha", "todo");
        set_task_field(&store, &a.id, "claimed_at", "2026-05-10T00:00:00Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                claimed_after: Some(t("2026-05-01T00:00:00Z")),
                claimed_before: Some(t("2026-05-15T00:00:00Z")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);
    }

    #[test]
    fn list_tasks_status_is_or_of_list() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "todo-1");
        let b = seed_task(&store, "alpha", "ip-1");
        let c = seed_task(&store, "alpha", "blocked-1");
        set_task_field(&store, &b.id, "status", "in_progress");
        set_task_field(&store, &b.id, "assignee", "ship");
        set_task_field(&store, &c.id, "status", "blocked");
        set_task_field(&store, &c.id, "assignee", "ship");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                status: Some(vec![TaskStatus::InProgress, TaskStatus::Blocked]),
                ..Default::default()
            })
            .unwrap();
        let mut ids: Vec<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        let mut want = vec![b.id.as_str(), c.id.as_str()];
        want.sort_unstable();
        assert_eq!(ids, want);
        assert!(!ids.contains(&a.id.as_str()));
    }

    #[test]
    fn list_tasks_order_by_updated_at_desc_with_limit() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "old");
        let b = seed_task(&store, "alpha", "mid");
        let c = seed_task(&store, "alpha", "new");
        set_task_field(&store, &a.id, "updated_at", "2026-01-01T00:00:00Z");
        set_task_field(&store, &b.id, "updated_at", "2026-03-01T00:00:00Z");
        set_task_field(&store, &c.id, "updated_at", "2026-05-01T00:00:00Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                order_by: Some(TaskOrderField::UpdatedAt),
                desc: Some(true),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec![c.id.as_str(), b.id.as_str()]);
    }

    #[test]
    fn list_tasks_order_by_completed_at_drops_nulls() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "completed");
        let _b = seed_task(&store, "alpha", "never-completed");
        set_task_field(&store, &a.id, "completed_at", "2026-05-10T00:00:00Z");

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                order_by: Some(TaskOrderField::CompletedAt),
                ..Default::default()
            })
            .unwrap();
        // b is dropped because completed_at is null.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);
    }

    #[test]
    fn list_tasks_cross_project_when_project_is_none() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let a = seed_task(&store, "alpha", "a-task");
        let b = seed_task(&store, "beta", "b-task");

        let hits = store.list_tasks(&TaskListFilter::default()).unwrap();
        let ids: std::collections::HashSet<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(a.id.as_str()));
        assert!(ids.contains(b.id.as_str()));
        assert_eq!(hits.len(), 2);
    }

    // ----- task.list combo -----

    #[test]
    fn list_tasks_combo_assignee_status_date() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "match");
        let b = seed_task(&store, "alpha", "wrong-assignee");
        let c = seed_task(&store, "alpha", "wrong-status");
        let d = seed_task(&store, "alpha", "wrong-date");
        // All four become candidates; only one survives all three filters.
        for (task, status, assignee, completed_at) in [
            (&a, "done", "human:mh", "2026-05-10T00:00:00Z"),
            (&b, "done", "ship", "2026-05-10T00:00:00Z"),
            (&c, "in_progress", "human:mh", "2026-05-10T00:00:00Z"),
            (&d, "done", "human:mh", "2026-01-01T00:00:00Z"),
        ] {
            set_task_field(&store, &task.id, "status", status);
            set_task_field(&store, &task.id, "assignee", assignee);
            set_task_field(&store, &task.id, "completed_at", completed_at);
        }

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                status: Some(vec![TaskStatus::Done]),
                assignee: Some("human:mh".to_owned()),
                completed_after: Some(t("2026-05-01T00:00:00Z")),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);
    }

    #[test]
    fn list_tasks_combo_cross_project_body_contains() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        let a = seed_task(&store, "alpha", "auth-a");
        let b = seed_task(&store, "beta", "auth-b");
        let c = seed_task(&store, "alpha", "unrelated");
        set_task_body(&store, &a.id, "Adds OIDC AUTH flow");
        set_task_body(&store, &b.id, "Migrates auth tokens to new format");
        set_task_body(&store, &c.id, "Spike on caching");

        let hits = store
            .list_tasks(&TaskListFilter {
                body_contains: Some("auth".to_owned()),
                ..Default::default()
            })
            .unwrap();
        let ids: std::collections::HashSet<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(a.id.as_str()));
        assert!(ids.contains(b.id.as_str()));
        assert!(!ids.contains(c.id.as_str()));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn list_tasks_combo_sort_and_limit_interact() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let a = seed_task(&store, "alpha", "first");
        let b = seed_task(&store, "alpha", "second");
        let c = seed_task(&store, "alpha", "third");
        set_task_field(&store, &a.id, "created_at", "2026-01-01T00:00:00Z");
        set_task_field(&store, &b.id, "created_at", "2026-02-01T00:00:00Z");
        set_task_field(&store, &c.id, "created_at", "2026-03-01T00:00:00Z");

        // Default sort ASC, limit 2 → first, second.
        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                limit: Some(2),
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<&str> = hits.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec![a.id.as_str(), b.id.as_str()]);

        // DESC + limit 1 → third only.
        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                desc: Some(true),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, c.id);
    }

    #[test]
    fn list_tasks_filter_by_phase_resolves_slug() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        let spec = add_phase_simple(&store, "alpha", "spec");
        let build = add_phase_simple(&store, "alpha", "build");

        let in_spec = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("spec".to_owned()),
                slug: "draft-spec".to_owned(),
                title: "Draft spec".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .unwrap();
        let _in_build = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("build".to_owned()),
                slug: "wire-up".to_owned(),
                title: "Wire up".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .unwrap();

        let hits = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                phase: Some("spec".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, in_spec.id);
        assert_eq!(hits[0].phase, spec.id);
        assert_ne!(hits[0].phase, build.id);
    }

    #[test]
    fn list_tasks_filter_by_phase_unknown_slug_errors() {
        // A typo in the phase slug must surface as a typed error rather
        // than silently returning every task in the project — the LLM
        // needs to know its slug was wrong.
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");
        store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("spec".to_owned()),
                slug: "draft-spec".to_owned(),
                title: "Draft spec".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
                depends_on: Vec::new(),
            })
            .unwrap();

        let err = store
            .list_tasks(&TaskListFilter {
                project: Some("alpha".to_owned()),
                phase: Some("typo-slug".to_owned()),
                ..Default::default()
            })
            .expect_err("unknown phase slug must error");
        let msg = err.to_string();
        assert!(
            msg.contains("phase not found") && msg.contains("typo-slug"),
            "expected 'phase not found' + slug; got: {msg}"
        );
    }

    // ----- phase.list per-predicate + combo -----

    #[test]
    fn list_phases_filter_by_status_and_body_contains() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");
        add_phase_simple(&store, "alpha", "build");
        set_phase_body(&store, "alpha", "spec", "Design OIDC auth handshake");
        set_phase_body(&store, "alpha", "build", "Implement the cache layer");
        store
            .update_phase(UpdatePhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                status: Some(PhaseStatus::Active),
                ..Default::default()
            })
            .unwrap();

        let hits = store
            .list_phases(&PhaseListFilter {
                project: Some("alpha".to_owned()),
                body_contains: Some("AUTH".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "spec");

        let active = store
            .list_phases(&PhaseListFilter {
                project: Some("alpha".to_owned()),
                status: Some(vec![PhaseStatus::Active]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].slug, "spec");
    }

    #[test]
    fn list_phases_cross_project_when_project_is_none() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        seed_project(&store, "beta");
        add_phase_simple(&store, "alpha", "spec");
        add_phase_simple(&store, "beta", "spec");

        let hits = store.list_phases(&PhaseListFilter::default()).unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn list_phases_order_by_updated_at_desc_limit_1() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        add_phase_simple(&store, "alpha", "spec");
        std::thread::sleep(std::time::Duration::from_millis(10));
        add_phase_simple(&store, "alpha", "build");
        // build is newer.

        let hits = store
            .list_phases(&PhaseListFilter {
                project: Some("alpha".to_owned()),
                order_by: Some(PhaseOrderField::UpdatedAt),
                desc: Some(true),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "build");
    }

    // ----- project.list per-predicate + combo -----

    #[test]
    fn list_projects_filter_by_status_and_body_contains() {
        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: "alpha".to_owned(),
                title: "Alpha".to_owned(),
                description: "auth flows for HYROX dashboards".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();
        store
            .create_project(NewProject {
                slug: "beta".to_owned(),
                title: "Beta".to_owned(),
                description: "unrelated caching tier".to_owned(),
                actor: "human:test".to_owned(),
            })
            .unwrap();
        store
            .update_project(UpdateProject {
                slug: "beta".to_owned(),
                status: Some(ProjectStatus::Paused),
                ..Default::default()
            })
            .unwrap();

        let hits = store
            .list_projects(&ProjectListFilter {
                body_contains: Some("AUTH".to_owned()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "alpha");
        // Even though `body_contains` had to load descriptions to
        // match, the response shape stays metadata-only.
        assert!(
            hits[0].description.is_empty(),
            "list_projects must blank descriptions even when body_contains is set"
        );

        let paused = store
            .list_projects(&ProjectListFilter {
                status: Some(vec![ProjectStatus::Paused]),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(paused.len(), 1);
        assert_eq!(paused[0].slug, "beta");
    }

    #[test]
    fn list_projects_order_by_updated_at_desc_limit_1() {
        let (_tmp, store) = fresh_corpus();
        seed_project(&store, "alpha");
        std::thread::sleep(std::time::Duration::from_millis(10));
        seed_project(&store, "beta");

        let hits = store
            .list_projects(&ProjectListFilter {
                order_by: Some(ProjectOrderField::UpdatedAt),
                desc: Some(true),
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].slug, "beta");
    }

    // ----- Dogfood acceptance queries -----
    //
    // Mirror the six acceptance queries from
    // docs/features/filter-expansion/spec.md against the in-repo
    // projects/dossier/ fixture. The fixture content is fixed (commit-
    // controlled), so the row counts here pin the verbs to the spec.

    #[test]
    fn dogfood_acceptance_queries() {
        let store = FsStore::open(repo_root()).expect("open corpus");

        // Q1: what's open in dossier right now? (no claimed/in_progress
        // in the fixture today — assert it doesn't error and the result
        // shape is sane.)
        let open = store
            .list_tasks(&TaskListFilter {
                project: Some("dossier".to_owned()),
                status: Some(vec![TaskStatus::Claimed, TaskStatus::InProgress]),
                ..Default::default()
            })
            .expect("open tasks query");
        for t in &open {
            assert!(matches!(
                t.status,
                TaskStatus::Claimed | TaskStatus::InProgress
            ));
        }

        // Q2: what did claude-code:michael close? — fixture has 3 done
        // tasks assigned to claude-code:michael.
        let closed = store
            .list_tasks(&TaskListFilter {
                assignee: Some("claude-code:michael".to_owned()),
                status: Some(vec![TaskStatus::Done]),
                completed_after: Some(t("2026-01-01T00:00:00Z")),
                ..Default::default()
            })
            .expect("closed tasks query");
        assert!(
            closed.len() >= 3,
            "expected >=3 done tasks for claude-code:michael, got {}",
            closed.len()
        );
        for t in &closed {
            assert_eq!(t.status, TaskStatus::Done);
            assert_eq!(t.assignee, "claude-code:michael");
        }

        // Q3: design phases mentioning "protocol" — fixture has at least
        // the 01-protocol-spec phase.
        let proto = store
            .list_phases(&PhaseListFilter {
                body_contains: Some("protocol".to_owned()),
                ..Default::default()
            })
            .expect("body_contains phase query");
        assert!(
            !proto.is_empty(),
            "expected >=1 phase mentioning 'protocol'"
        );

        // Q4: latest phase by updated_at, limit 1.
        let latest = store
            .list_phases(&PhaseListFilter {
                order_by: Some(PhaseOrderField::UpdatedAt),
                desc: Some(true),
                limit: Some(1),
                ..Default::default()
            })
            .expect("latest phase query");
        assert_eq!(latest.len(), 1);

        // Q5: paused projects — fixture has zero, query must not error.
        let paused = store
            .list_projects(&ProjectListFilter {
                status: Some(vec![ProjectStatus::Paused]),
                ..Default::default()
            })
            .expect("paused projects query");
        for p in &paused {
            assert_eq!(p.status, ProjectStatus::Paused);
        }

        // Q6: cross-portfolio in-flight tasks.
        let in_flight = store
            .list_tasks(&TaskListFilter {
                status: Some(vec![TaskStatus::Claimed, TaskStatus::InProgress]),
                ..Default::default()
            })
            .expect("in-flight cross-project query");
        for t in &in_flight {
            assert!(matches!(
                t.status,
                TaskStatus::Claimed | TaskStatus::InProgress
            ));
        }
    }

    // ----- phase.add CAS ordering -----

    #[tokio::test]
    async fn add_phase_cas_race_two_independent_writers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(tmp.path().join(".dossier")).expect("mkdir .dossier");
        let path = tmp.path();

        let store_a = FsStore::open(path).expect("writer a");
        store_a
            .create_project(NewProject {
                slug: "race".to_owned(),
                title: "Race".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .expect("create project");
        let store_b = FsStore::open(path).expect("writer b");

        let va = Store::get_project(&store_a, "race")
            .await
            .expect("read v0 from a");
        let vb = Store::get_project(&store_b, "race")
            .await
            .expect("read v0 from b");
        assert_eq!(va.version, vb.version);

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

        let phase_a = store_a.add_phase(&args_a).expect("writer a lands");
        assert_eq!(phase_a.order, 1);

        let mut stale_gate = vb.value;
        stale_gate.updated_at = now_utc();
        let err = store_b
            .put_project(&stale_gate, Some(vb.version))
            .await
            .expect_err("writer b stale project CAS");
        assert!(matches!(err, StoreError::Conflict));

        let phase_b = store_b.add_phase(&args_b).expect("writer b retry lands");
        assert_eq!(phase_b.order, 2);
        assert_ne!(phase_a.order, phase_b.order);

        let mut phases = store_a
            .list_phases(&phase_filter_for("race"))
            .expect("list phases");
        assert_eq!(phases.len(), 2);
        phases.sort_by_key(|p| p.order);
        assert_eq!(phases[0].order, 1);
        assert_eq!(phases[1].order, 2);
    }

    #[tokio::test]
    async fn add_phase_cas_project_gate_conflict_then_retry() {
        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: "cas".to_owned(),
                title: "CAS".to_owned(),
                description: String::new(),
                actor: "human:test".to_owned(),
            })
            .expect("create");

        let Versioned {
            value: project,
            version: v0,
        } = Store::get_project(&store, "cas").await.expect("read v0");

        let mut writer_a = project.clone();
        writer_a.updated_at = now_utc();
        let v1 = store
            .put_project(&writer_a, Some(v0.clone()))
            .await
            .expect("writer a wins project CAS");

        let mut writer_b = project;
        writer_b.updated_at = now_utc();
        let err = store
            .put_project(&writer_b, Some(v0))
            .await
            .expect_err("writer b stale");
        assert!(matches!(err, StoreError::Conflict));

        let mut writer_b_retry = writer_a;
        writer_b_retry.updated_at = now_utc();
        store
            .put_project(&writer_b_retry, Some(v1))
            .await
            .expect("writer b retry on v1");

        let phase_a = store
            .add_phase(&NewPhase {
                project: "cas".to_owned(),
                slug: "a".to_owned(),
                title: "A".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect("add a");
        let phase_b = store
            .add_phase(&NewPhase {
                project: "cas".to_owned(),
                slug: "b".to_owned(),
                title: "B".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
                owner: "human:test".to_owned(),
            })
            .expect("add b");
        assert_ne!(phase_a.order, phase_b.order);

        let phases = store.list_phases(&phase_filter_for("cas")).expect("list");
        assert_eq!(phases.len(), 2);
    }

    /// Exhaustive field comparison after project.md write + read. If a new
    /// `Project` field is added, this list must grow — otherwise drift from
    /// `ProjectFrontmatter` goes uncaught.
    fn assert_project_all_fields_persisted(want: &Project, got: &Project) {
        assert_eq!(
            want.id, got.id,
            "field `id` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.slug, got.slug,
            "field `slug` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.title, got.title,
            "field `title` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.description, got.description,
            "field `description` not persisted (project markdown body round-trip)"
        );
        assert_eq!(
            want.status, got.status,
            "field `status` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.created_at, got.created_at,
            "field `created_at` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.updated_at, got.updated_at,
            "field `updated_at` not persisted (project frontmatter round-trip)"
        );
        assert_eq!(
            want.created_by, got.created_by,
            "field `created_by` not persisted (project frontmatter round-trip)"
        );
    }

    fn assert_phase_all_fields_persisted(want: &Phase, got: &Phase) {
        assert_eq!(
            want.id, got.id,
            "field `id` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.project, got.project,
            "field `project` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.slug, got.slug,
            "field `slug` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.title, got.title,
            "field `title` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.body, got.body,
            "field `body` not persisted (phase markdown body round-trip)"
        );
        assert_eq!(
            want.order, got.order,
            "field `order` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.status, got.status,
            "field `status` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.created_at, got.created_at,
            "field `created_at` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.updated_at, got.updated_at,
            "field `updated_at` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.created_by, got.created_by,
            "field `created_by` not persisted (phase frontmatter round-trip)"
        );
        assert_eq!(
            want.owner, got.owner,
            "field `owner` not persisted (phase frontmatter round-trip)"
        );
    }

    fn assert_task_all_fields_persisted(want: &Task, got: &Task) {
        assert_eq!(
            want.id, got.id,
            "field `id` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.project, got.project,
            "field `project` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.project_slug, got.project_slug,
            "field `project_slug` not stamped (store should derive it from the corpus path on load)"
        );
        assert_eq!(
            want.phase, got.phase,
            "field `phase` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.slug, got.slug,
            "field `slug` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.title, got.title,
            "field `title` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.body, got.body,
            "field `body` not persisted (task markdown body round-trip)"
        );
        assert_eq!(
            want.status, got.status,
            "field `status` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.assignee, got.assignee,
            "field `assignee` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.claimed_at, got.claimed_at,
            "field `claimed_at` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.completed_at, got.completed_at,
            "field `completed_at` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.created_at, got.created_at,
            "field `created_at` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.updated_at, got.updated_at,
            "field `updated_at` not persisted (task frontmatter round-trip)"
        );
        assert_eq!(
            want.notes.len(),
            got.notes.len(),
            "field `notes` count not persisted (`## Notes` section round-trip)"
        );
        assert_eq!(
            want.depends_on, got.depends_on,
            "field `depends_on` not persisted (task frontmatter round-trip)"
        );
        for (i, (wn, gn)) in want.notes.iter().zip(got.notes.iter()).enumerate() {
            assert_eq!(
                wn.actor, gn.actor,
                "field `notes[{i}].actor` not persisted (`## Notes` section round-trip)",
            );
            assert_eq!(
                wn.body, gn.body,
                "field `notes[{i}].body` not persisted (`## Notes` section round-trip)",
            );
            assert_eq!(
                wn.posted_at, gn.posted_at,
                "field `notes[{i}].posted_at` not persisted (`## Notes` section round-trip)",
            );
        }
    }

    #[test]
    fn project_frontmatter_roundtrip_all_fields() {
        let ts_create = DateTime::parse_from_rfc3339("2025-03-03T03:03:03Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_upd = DateTime::parse_from_rfc3339("2025-04-04T04:04:04Z")
            .unwrap()
            .with_timezone(&Utc);

        let (tmp, store) = fresh_corpus();
        let slug = "fm_rt_proj";
        let proj_dir = tmp.path().join("projects").join(slug);
        fs::create_dir_all(&proj_dir).expect("mkdir project");

        let want = Project {
            id: new_id("prj"),
            slug: slug.to_owned(),
            title: "fm-rt:title with spaces & symbols".to_owned(),
            description: "First paragraph persisted.\n\nSecond line `: # yaml-ish` chars."
                .to_owned(),
            status: ProjectStatus::Paused,
            created_at: ts_create,
            updated_at: ts_upd,
            created_by: "human:roundtrip_tester".to_owned(),
        };

        let content = serialize_project_file(&want).expect("serialize");
        write_atomic(&proj_dir.join("project.md"), content.as_bytes()).expect("write");

        let got = store.get_project(slug).expect("read back");
        assert_project_all_fields_persisted(&want, &got);
    }

    #[test]
    fn phase_frontmatter_roundtrip_all_fields() {
        let ts_create = DateTime::parse_from_rfc3339("2025-05-05T05:05:05Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_upd = DateTime::parse_from_rfc3339("2025-06-06T06:06:06Z")
            .unwrap()
            .with_timezone(&Utc);

        let (tmp, store) = fresh_corpus();
        let proj_slug = "fm_rt_phase_proj";
        let phase_slug = "fm_rt_phase";
        let prj_id = new_id("prj");
        let phs_id = new_id("phs");

        let proj_dir = tmp.path().join("projects").join(proj_slug);
        let phases_dir = proj_dir.join("phases");
        fs::create_dir_all(&phases_dir).expect("mkdir phases");

        let want = Phase {
            id: phs_id,
            project: prj_id,
            slug: phase_slug.to_owned(),
            title: "Phase title — persisted".to_owned(),
            body: "Phase body line one.\n\n### Not the notes delimiter".to_owned(),
            order: 7_i32,
            status: PhaseStatus::Skipped,
            created_at: ts_create,
            updated_at: ts_upd,
            created_by: "agent:test-roundtrip".to_owned(),
            owner: "team:platform".to_owned(),
        };

        let path = phases_dir.join(phase_filename(want.order, phase_slug));
        let content = serialize_phase_file(&want).expect("serialize phase");
        write_atomic(&path, content.as_bytes()).expect("write phase");

        let phases = store
            .list_phases(&phase_filter_for(proj_slug))
            .expect("list phases");
        let got = phases
            .iter()
            .find(|p| p.slug == phase_slug)
            .expect("phase readable");
        assert_phase_all_fields_persisted(&want, got);
    }

    #[test]
    fn task_frontmatter_roundtrip_all_fields() {
        let ts_create = DateTime::parse_from_rfc3339("2025-07-07T07:07:07Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_upd = DateTime::parse_from_rfc3339("2025-08-08T08:08:08Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_claim = DateTime::parse_from_rfc3339("2025-09-09T09:09:09Z")
            .unwrap()
            .with_timezone(&Utc);
        let ts_done = DateTime::parse_from_rfc3339("2025-10-10T10:10:10Z")
            .unwrap()
            .with_timezone(&Utc);

        let (tmp, store) = fresh_corpus();
        let proj_slug = "fm_rt_task_proj";
        let task_slug = "fm_rt_task";
        let prj_id = new_id("prj");
        let phs_id = new_id("phs");

        let proj_dir = tmp.path().join("projects").join(proj_slug);
        let tasks_dir = proj_dir.join("tasks");
        fs::create_dir_all(&tasks_dir).expect("mkdir tasks");

        let note = Note {
            actor: "human:memo".to_owned(),
            body: "Note body survives round-trip".to_owned(),
            posted_at: DateTime::parse_from_rfc3339("2025-11-11T11:11:11Z")
                .unwrap()
                .with_timezone(&Utc),
        };
        let notes_lines = vec![format_note_line(
            note.posted_at,
            note.actor.as_str(),
            note.body.as_str(),
        )];

        let tsk_id = new_id("tsk");
        let want = Task {
            id: tsk_id.clone(),
            project: prj_id,
            project_slug: proj_slug.to_owned(),
            phase: phs_id,
            slug: task_slug.to_owned(),
            title: "Task title persisted".to_owned(),
            body: "Spec uses ### Notes not ## Notes.".to_owned(),
            status: TaskStatus::Blocked,
            assignee: "human:blocked_holder".to_owned(),
            claimed_at: Some(ts_claim),
            completed_at: Some(ts_done),
            created_at: ts_create,
            updated_at: ts_upd,
            notes: vec![note],
            depends_on: Vec::new(),
        };

        let path = tasks_dir.join(task_filename(&tsk_id, task_slug));
        let content = serialize_task_file(&want, &notes_lines).expect("serialize task");
        write_atomic(&path, content.as_bytes()).expect("write task");

        let tasks = store
            .list_tasks(&task_filter_for(proj_slug))
            .expect("list tasks");
        let got = tasks
            .iter()
            .find(|t| t.slug == task_slug)
            .expect("task readable");
        assert_task_all_fields_persisted(&want, got);
    }
}

#[cfg(test)]
#[path = "../tests/common/proptest_serialize_roundtrip.rs"]
mod proptest_serialize_roundtrip;
