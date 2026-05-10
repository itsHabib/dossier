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

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use ulid::Ulid;

use crate::domain::{Artifact, Phase, PhaseStatus, Project, ProjectStatus, Task, TaskStatus};

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

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// List every project, sorted by slug. Description bodies are not
    /// loaded — call `get_project` for the full record.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
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
            let slug = entry.file_name().to_string_lossy().into_owned();
            let proj = self
                .load_project(&slug, false)
                .with_context(|| format!("load project {slug}"))?;
            out.push(proj);
        }
        out.sort_by(|a, b| a.slug.cmp(&b.slug));
        Ok(out)
    }

    /// Get one project by slug, including the description body.
    pub fn get_project(&self, slug: &str) -> Result<Project> {
        self.load_project(slug, true)
    }

    /// Phases of a project, ordered by their `order` field.
    pub fn list_phases(&self, project_slug: &str) -> Result<Vec<Phase>> {
        let dir = self.root.join("projects").join(project_slug).join("phases");
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
        out.sort_by_key(|p| p.order);
        Ok(out)
    }

    /// Tasks of a project, ordered by creation time.
    pub fn list_tasks(&self, project_slug: &str) -> Result<Vec<Task>> {
        let dir = self.root.join("projects").join(project_slug).join("tasks");
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
            let task = load_task(&path).with_context(|| format!("load task {}", path.display()))?;
            out.push(task);
        }
        out.sort_by_key(|t| t.created_at);
        Ok(out)
    }

    /// Artifacts linked to a project. JSONL on disk; the file may be
    /// missing or empty (both yield an empty vec).
    pub fn list_artifacts(&self, project_slug: &str) -> Result<Vec<Artifact>> {
        let path = self
            .root
            .join("projects")
            .join(project_slug)
            .join("artifacts.jsonl");
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
        let path = self.root.join("projects").join(slug).join("project.md");
        let (front, body) = read_frontmatter(&path)?;
        let mut p: Project = serde_yml::from_str(&front)
            .with_context(|| format!("parse project frontmatter {}", path.display()))?;
        if with_body {
            body.trim().clone_into(&mut p.description);
        }
        Ok(p)
    }
}

fn load_phase(path: &Path) -> Result<Phase> {
    let (front, body) = read_frontmatter(path)?;
    let mut p: Phase = serde_yml::from_str(&front)
        .with_context(|| format!("parse phase frontmatter {}", path.display()))?;
    body.trim().clone_into(&mut p.body);
    Ok(p)
}

fn load_task(path: &Path) -> Result<Task> {
    let (t, _notes) = load_task_with_notes(path)?;
    Ok(t)
}

/// Read a task file and split its body into the spec section (above
/// `## Notes`) and the raw note lines. `Task.body` holds the spec
/// section only; `Task.notes` is populated by parsing each line —
/// unparseable lines are skipped from the struct but kept verbatim in
/// the returned `Vec<String>`, so the disk round-trip stays lossless.
fn load_task_with_notes(path: &Path) -> Result<(Task, Vec<String>)> {
    let (front, body) = read_frontmatter(path)?;
    let mut t: Task = serde_yml::from_str(&front)
        .with_context(|| format!("parse task frontmatter {}", path.display()))?;
    let (spec, notes_lines) = split_task_body(&body);
    t.body = spec;
    t.notes = notes_lines
        .iter()
        .filter_map(|l| parse_note_line(l))
        .collect();
    Ok((t, notes_lines))
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
    while notes_lines.first().is_some_and(|l| l.trim().is_empty()) {
        notes_lines.remove(0);
    }
    while notes_lines.last().is_some_and(|l| l.trim().is_empty()) {
        notes_lines.pop();
    }
    (spec, notes_lines)
}

fn read_frontmatter(path: &Path) -> Result<(String, String)> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let after_open = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| anyhow!("{}: missing frontmatter delimiter", path.display()))?;
    let close_idx = after_open
        .find("\n---")
        .ok_or_else(|| anyhow!("{}: unterminated frontmatter", path.display()))?;
    let front = after_open[..close_idx].to_owned();
    let after = &after_open[close_idx + 4..];
    let body = after.trim_start_matches(['\r', '\n']).to_owned();
    Ok((front, body))
}

// =============================================================================
// Write side
// =============================================================================

/// Arguments for `FsStore::create_project`. Slug must be unique within
/// the corpus.
#[derive(Debug, Clone)]
pub struct NewProject {
    pub slug: String,
    pub title: String,
    pub description: String,
    pub actor: String,
}

/// Arguments for `FsStore::update_project`. Slug is the addressing key;
/// every other field is optional. `id`, `created_at`, and `slug` itself
/// are immutable.
#[derive(Debug, Clone, Default)]
pub struct UpdateProject {
    pub slug: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<ProjectStatus>,
}

/// Arguments for `FsStore::add_phase`. Slug must be unique within the
/// project. `after_phase` (also a slug) inserts in order; default appends.
#[derive(Debug, Clone)]
pub struct NewPhase {
    pub project: String,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub after_phase: Option<String>,
    pub actor: String,
}

/// Arguments for `FsStore::update_phase`. (project, slug) is the
/// addressing key. Order is managed via `add_phase` only — bulk reorder
/// is out of scope for v0.
#[derive(Debug, Clone, Default)]
pub struct UpdatePhase {
    pub project: String,
    pub slug: String,
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<PhaseStatus>,
}

/// Arguments for `FsStore::create_task`. Slug must be unique within the
/// project. `phase` (optional, a slug) anchors the task to a phase;
/// omit for project-wide tasks.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub project: String,
    pub phase: Option<String>,
    pub slug: String,
    pub title: String,
    pub body: String,
    pub actor: String,
}

/// Arguments for `FsStore::claim_task`. Same-actor re-claim on a
/// non-terminal task is a no-op (no `updated_at` bump).
#[derive(Debug, Clone)]
pub struct ClaimTask {
    pub id: String,
    pub actor: String,
}

/// Arguments for `FsStore::update_task`.
///
/// `status=claimed` and `status=done` are rejected — use `claim_task`
/// or `complete_task` instead. `note`, when supplied, appends a
/// timestamped line to the task's `## Notes` section.
#[derive(Debug, Clone, Default)]
pub struct UpdateTask {
    pub id: String,
    pub body: Option<String>,
    pub status: Option<TaskStatus>,
    pub note: Option<String>,
    pub actor: String,
}

/// Arguments for `FsStore::complete_task`. Errors unless the task is in
/// `InProgress`.
#[derive(Debug, Clone)]
pub struct CompleteTask {
    pub id: String,
    pub note: Option<String>,
    pub actor: String,
}

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
        if !is_valid_slug(&args.slug) {
            bail!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            );
        }
        let dir = self.root.join("projects").join(&args.slug);
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

        let path = self
            .root
            .join("projects")
            .join(&project.slug)
            .join("project.md");
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
    pub fn add_phase(&self, args: NewPhase) -> Result<Phase> {
        if args.project.is_empty() {
            bail!("project is required");
        }
        if !is_valid_slug(&args.slug) {
            bail!(
                "slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.slug
            );
        }
        let project_dir = self.root.join("projects").join(&args.project);
        if !project_dir.exists() {
            bail!("project not found: {}", args.project);
        }
        let project = self.load_project(&args.project, false)?;

        let mut existing = self.list_phases(&args.project)?;
        if existing.iter().any(|p| p.slug == args.slug) {
            bail!("phase slug already exists in project: {}", args.slug);
        }

        let new_order = match &args.after_phase {
            Some(after_slug) => {
                let after = existing
                    .iter()
                    .find(|p| &p.slug == after_slug)
                    .ok_or_else(|| anyhow!("after_phase not found: {after_slug}"))?;
                after.order + 1
            }
            None => existing.iter().map(|p| p.order).max().unwrap_or(0) + 1,
        };

        let phases_dir = self
            .root
            .join("projects")
            .join(&args.project)
            .join("phases");
        fs::create_dir_all(&phases_dir)
            .with_context(|| format!("create {}", phases_dir.display()))?;

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
                .with_context(|| format!("rename {old_name} -> {new_name}"))?;
            p.order += 1;
            let path = phases_dir.join(&new_name);
            let content = serialize_phase_file(p)?;
            write_atomic(&path, content.as_bytes())?;
        }

        let now = now_utc();
        let phase = Phase {
            id: new_id("phs"),
            project: project.id,
            slug: args.slug.clone(),
            title: args.title,
            body: args.body,
            order: new_order,
            status: PhaseStatus::Pending,
            created_at: now,
            updated_at: now,
        };
        let _ = args.actor; // recorded in commit history; not persisted on the phase row in v0
        let path = phases_dir.join(phase_filename(new_order, &args.slug));
        let content = serialize_phase_file(&phase)?;
        write_atomic(&path, content.as_bytes())?;
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
        phase.updated_at = now_utc();
        let content = serialize_phase_file(&phase)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(phase)
    }

    fn find_phase_path(&self, project_slug: &str, phase_slug: &str) -> Result<PathBuf> {
        let phases_dir = self.root.join("projects").join(project_slug).join("phases");
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
}

impl FsStore {
    /// Create a new task in a project. Optionally anchored to a phase.
    ///
    /// Errors on duplicate slug within the project, invalid slug, unknown
    /// project, or unknown phase. Server-stamps id, timestamps, and
    /// `Todo` status.
    pub fn create_task(&self, args: NewTask) -> Result<Task> {
        if !is_valid_slug(&args.project) {
            bail!(
                "project slug must be lowercase ascii (a-z, 0-9, -, _): {}",
                args.project
            );
        }
        if let Some(phase) = &args.phase {
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
        let project_dir = self.root.join("projects").join(&args.project);
        if !project_dir.exists() {
            bail!("project not found: {}", args.project);
        }
        let project = self.load_project(&args.project, false)?;

        let phase_id = match &args.phase {
            Some(phase_slug) => {
                let phases = self.list_phases(&args.project)?;
                let phase = phases
                    .iter()
                    .find(|p| &p.slug == phase_slug)
                    .ok_or_else(|| anyhow!("phase not found: {phase_slug}"))?;
                phase.id.clone()
            }
            None => String::new(),
        };

        let existing = self.list_tasks(&args.project)?;
        if existing.iter().any(|t| t.slug == args.slug) {
            bail!("task slug already exists in project: {}", args.slug);
        }

        let tasks_dir = project_dir.join("tasks");
        fs::create_dir_all(&tasks_dir)
            .with_context(|| format!("create {}", tasks_dir.display()))?;

        let now = now_utc();
        let id = new_id("tsk");
        let task = Task {
            id: id.clone(),
            project: project.id,
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
    pub fn claim_task(&self, args: ClaimTask) -> Result<Task> {
        if args.actor.is_empty() {
            bail!("actor is required to claim a task");
        }
        let (_project_slug, path) = self.find_task_path(&args.id)?;
        let (task, notes_lines) = load_task_with_notes(&path)?;

        if matches!(task.status, TaskStatus::Done | TaskStatus::Cancelled) {
            bail!(
                "cannot claim task in terminal state: {}",
                task_status_str(task.status)
            );
        }
        // Same-actor re-claim only no-ops on legitimate held states.
        // `Todo` with a non-empty assignee is a corrupt state (the
        // assignee is set by claim, which also moves status off Todo)
        // and should surface, not be swallowed silently.
        if task.assignee == args.actor {
            if matches!(
                task.status,
                TaskStatus::Claimed | TaskStatus::InProgress | TaskStatus::Blocked
            ) {
                return Ok(task);
            }
            bail!(
                "task in state {} has assignee {} but isn't held (corrupt state)",
                task_status_str(task.status),
                task.assignee
            );
        }
        if !task.assignee.is_empty() {
            bail!("task already claimed by {}", task.assignee);
        }
        if !matches!(task.status, TaskStatus::Todo) {
            bail!(
                "task in state {} has no assignee (corrupt state)",
                task_status_str(task.status)
            );
        }

        let now = now_utc();
        let mut task = task;
        task.assignee = args.actor;
        task.status = TaskStatus::Claimed;
        task.claimed_at = Some(now);
        task.updated_at = now;

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
        let (_project_slug, path) = self.find_task_path(&args.id)?;
        let (mut task, mut notes_lines) = load_task_with_notes(&path)?;

        if let Some(target) = args.status {
            validate_task_update_transition(task.status, target)?;
            task.status = target;
        }
        if let Some(body) = args.body {
            task.body = body;
        }
        let now = now_utc();
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
        let (_project_slug, path) = self.find_task_path(&args.id)?;
        let (mut task, mut notes_lines) = load_task_with_notes(&path)?;

        if !matches!(task.status, TaskStatus::InProgress) {
            bail!(
                "task must be in_progress to complete (got {})",
                task_status_str(task.status)
            );
        }

        let now = now_utc();
        task.status = TaskStatus::Done;
        task.completed_at = Some(now);
        task.updated_at = now;
        if let Some(note) = args.note {
            append_note(&mut task, &mut notes_lines, now, &args.actor, &note)?;
        }

        let content = serialize_task_file(&task, &notes_lines)?;
        write_atomic(&path, content.as_bytes())?;
        Ok(task)
    }

    /// Locate a task file by id by walking each project's `tasks/` dir.
    /// O(projects × tasks) — cheap at v0 corpus sizes; an index lands
    /// when it actually matters.
    fn find_task_path(&self, task_id: &str) -> Result<(String, PathBuf)> {
        let projects_dir = self.root.join("projects");
        if !projects_dir.exists() {
            bail!("task not found: {task_id}");
        }
        let entries = fs::read_dir(&projects_dir)
            .with_context(|| format!("read {}", projects_dir.display()))?;
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
                        return Ok((project_slug, path));
                    }
                }
            }
        }
        bail!("task not found: {task_id}")
    }
}

/// Filename for a task: ULID + slug. ULID never contains a `-` so the
/// split on the first `-` is unambiguous.
fn task_filename(id: &str, slug: &str) -> String {
    format!("{id}-{slug}.md")
}

/// Filename for a phase: zero-padded order + slug. The order prefix
/// gives stable sort in directory listings AND a human-readable hint.
fn phase_filename(order: i32, slug: &str) -> String {
    format!("{order:02}-{slug}.md")
}

/// Snake-case wire name for a status, for error messages that need to
/// match the JSON enum form rather than the Rust `Debug` `PascalCase`.
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

/// Guard the subset of transitions reachable via `task.update`. The
/// `claimed` and `done` targets are sole-property of `task.claim` and
/// `task.complete` respectively; terminal states accept nothing.
fn validate_task_update_transition(from: TaskStatus, to: TaskStatus) -> Result<()> {
    // Order matters. claimed/done targets are owned by task.claim and
    // task.complete; terminal sources reject *every* status write
    // (including idempotent same-state). Both gates must fire before
    // the `from == to` no-op short-circuit.
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

/// Generate a new prefixed ULID. Type prefix matches `LAYOUT.md`
/// conventions: `prj_`, `phs_`, `tsk_`, `art_`.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Ulid::new())
}

/// Current UTC timestamp. Wrapper for testability — callers that want
/// deterministic time can stub at the boundary.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

/// Slugs are lowercase ASCII with `-` and `_` allowed. Keeps
/// cross-platform filesystem behavior predictable (case-sensitivity
/// differs between NTFS, APFS, ext4).
fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
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
fn serialize_project_file(project: &Project) -> Result<String> {
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
        }
    }
}

fn serialize_phase_file(phase: &Phase) -> Result<String> {
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
        }
    }
}

/// Serialize a task to its on-disk form: YAML frontmatter, blank line,
/// spec body (if any), then a `## Notes` section reconstructed from
/// `notes_lines`. Each note line is emitted verbatim with one trailing
/// newline.
fn serialize_task_file(task: &Task, notes_lines: &[String]) -> Result<String> {
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

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn read_dogfood_corpus() {
        let store = FsStore::open(repo_root()).expect("open corpus");

        let projects = store.list_projects().expect("list projects");
        assert_eq!(projects.len(), 1, "want 1 project, got {}", projects.len());
        assert_eq!(projects[0].slug, "dossier");
        assert!(
            projects[0].description.is_empty(),
            "list_projects returned a body — should be metadata only"
        );

        let p = store.get_project("dossier").expect("get project");
        assert!(
            p.description.contains("Agent-native project management"),
            "description body missing or wrong: {:?}",
            p.description
        );

        let phases = store.list_phases("dossier").expect("list phases");
        assert_eq!(phases.len(), 4);
        for (i, ph) in phases.iter().enumerate() {
            assert_eq!(ph.order, i as i32 + 1, "phase[{i}] order");
        }

        let tasks = store.list_tasks("dossier").expect("list tasks");
        assert_eq!(tasks.len(), 3);

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
        let listed = store.list_projects().unwrap();
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
        assert!(err.to_string().contains("lowercase ascii"), "got: {err}");
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
            .add_phase(NewPhase {
                project: project.to_owned(),
                slug: slug.to_owned(),
                title: slug.to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
            })
            .expect("add phase")
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

        let listed = store.list_phases("alpha").unwrap();
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
            .add_phase(NewPhase {
                project: "alpha".to_owned(),
                slug: "design".to_owned(),
                title: "design".to_owned(),
                body: String::new(),
                after_phase: Some("spec".to_owned()),
                actor: "human:test".to_owned(),
            })
            .unwrap();
        assert_eq!(inserted.order, 2);

        let listed = store.list_phases("alpha").unwrap();
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
            .add_phase(NewPhase {
                project: "alpha".to_owned(),
                slug: "spec".to_owned(),
                title: "Another spec".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
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
            .add_phase(NewPhase {
                project: "alpha".to_owned(),
                slug: "design".to_owned(),
                title: "design".to_owned(),
                body: String::new(),
                after_phase: Some("nonexistent".to_owned()),
                actor: "human:test".to_owned(),
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
            .add_phase(NewPhase {
                project: "ghost".to_owned(),
                slug: "spec".to_owned(),
                title: "spec".to_owned(),
                body: String::new(),
                after_phase: None,
                actor: "human:test".to_owned(),
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
        let listed = store.list_phases("alpha").unwrap();
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
            })
            .expect("create task")
    }

    fn claim(store: &FsStore, id: &str, actor: &str) -> Task {
        store
            .claim_task(ClaimTask {
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

        let listed = store.list_tasks("alpha").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, task.id);
        assert!(listed[0].body.contains("spec body"));
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
            })
            .expect_err("project slug must be validated");
        assert!(
            err.to_string().contains("project slug must be lowercase"),
            "got: {err}"
        );

        let err = store
            .create_task(NewTask {
                project: "alpha".to_owned(),
                phase: Some("../sneaky".to_owned()),
                slug: "x".to_owned(),
                title: "x".to_owned(),
                body: String::new(),
                actor: "human:test".to_owned(),
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

        let tasks = store.list_tasks("alpha").unwrap();
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
            .claim_task(ClaimTask {
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
            .claim_task(ClaimTask {
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
            .claim_task(ClaimTask {
                id: task.id,
                actor: "ship".to_owned(),
            })
            .expect_err("todo+assignee should surface as corrupt");
        assert!(err.to_string().contains("corrupt state"), "got: {err}");
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
}
