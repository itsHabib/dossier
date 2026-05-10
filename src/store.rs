//! Read-side of the on-disk corpus described in `LAYOUT.md`.
//!
//! Single-writer assumption — concurrent `FsStore` handles against the
//! same root will eventually corrupt it. Write methods are deferred to a
//! later phase.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::domain::{Artifact, Phase, Project, Task};

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
}
