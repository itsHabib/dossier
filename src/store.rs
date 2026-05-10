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

use crate::domain::{Artifact, Phase, PhaseStatus, Project, ProjectStatus, Task};

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
    let (front, body) = read_frontmatter(path)?;
    let mut t: Task = serde_yml::from_str(&front)
        .with_context(|| format!("parse task frontmatter {}", path.display()))?;
    body.trim().clone_into(&mut t.body);
    Ok(t)
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

/// Filename for a phase: zero-padded order + slug. The order prefix
/// gives stable sort in directory listings AND a human-readable hint.
fn phase_filename(order: i32, slug: &str) -> String {
    format!("{order:02}-{slug}.md")
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
}
