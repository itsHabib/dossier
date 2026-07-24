//! Artifact verbs: DTOs and service-layer policy for `artifact.*`.
//!
//! The `#[tool]` wrappers stay in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the argument/result DTOs, the link policy,
//! and the artifact tests.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{is_valid_slug, new_id, validate_single_line, Artifact, TaskListFilter};
use crate::store::{now_utc, ArtifactListFilter, LinkArtifact, StoreError, Versioned};

use super::{domain_err, invalid_msg, MeshService};

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
    use crate::server::task::TaskCreateArgs;
    use crate::server::testutil::{
        assert_rejects_invalid_project_slug, block_on, fresh_service, INVALID_PROJECT_SLUG,
    };
    use crate::server::ProjectCreateArgs;
    use rmcp::handler::server::wrapper::{Json, Parameters};

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
