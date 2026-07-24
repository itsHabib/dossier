//! Artifact verbs: DTOs and service-layer policy for `artifact.*`.
//!
//! The `#[tool]` wrappers stay in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the argument/result DTOs, the link policy,
//! and the artifact tests.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{
    is_valid_slug, new_id, validate_artifact_meta, validate_single_line, Artifact, TaskListFilter,
};
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
    /// if non-empty, only artifacts whose ref exactly matches (canonical PR
    /// URL, gate audit ref, etc.) — no substring/prefix matching
    #[serde(default, rename = "ref")]
    pub reference: String,
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
    /// optional flat string map (≤16 keys; caps enforced at link time)
    #[serde(default)]
    pub meta: BTreeMap<String, String>,
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
        validate_artifact_meta(&args.meta).map_err(|e| domain_err(&e))?;

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
                reference: None,
            })
            .await?;
        if let Some(artifact) = existing
            .into_iter()
            .find(|a| a.task == task_id && a.kind == args.kind && a.reference == args.reference)
        {
            if artifact.meta != args.meta {
                return Err(invalid_msg(
                    "meta is immutable for an existing (task, kind, ref); supersede instead",
                ));
            }
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
            meta: args.meta,
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
    use rmcp::model::ErrorCode;

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
                meta: BTreeMap::new(),
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
                meta: BTreeMap::new(),
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
                meta: BTreeMap::new(),
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
            reference: String::new(),
        });
        all.sort_unstable();
        assert_eq!(all.len(), 3);

        let mut hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: tid1.clone(),
            kind: String::new(),
            reference: String::new(),
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
            reference: String::new(),
        });
        hit.sort_unstable();
        assert_eq!(hit, {
            let mut v = vec![id_a1.clone(), id_a2];
            v.sort_unstable();
            v
        },);

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: tid1,
            kind: "commit".to_owned(),
            reference: String::new(),
        });
        assert_eq!(hit, vec![id_a3]);

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: "tsk_does_not_exist".to_owned(),
            kind: String::new(),
            reference: String::new(),
        });
        assert!(hit.is_empty());

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: String::new(),
            reference: "https://example/pr/1".to_owned(),
        });
        assert_eq!(hit, vec![id_a1.clone()], "ref filter hits exact match");

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: String::new(),
            reference: "https://example/pr/does-not-exist".to_owned(),
        });
        assert!(hit.is_empty(), "ref filter misses non-matching ref");

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: "commit".to_owned(),
            reference: "https://example/pr/1".to_owned(),
        });
        assert!(
            hit.is_empty(),
            "ref filter AND-composes with kind: pr's ref under a commit-kind filter is empty"
        );

        hit = ids!(ArtifactListArgs {
            project: "corp".to_owned(),
            task: String::new(),
            kind: "pr".to_owned(),
            reference: "https://example/pr/1".to_owned(),
        });
        assert_eq!(
            hit,
            vec![id_a1],
            "ref filter AND-composes with kind: matching kind + ref both hit"
        );
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
            meta: BTreeMap::new(),
        }))
        .expect_err("invalid project slug must reject");
        assert_rejects_invalid_project_slug(err, INVALID_PROJECT_SLUG);
    }

    fn link_with_meta(
        svc: &MeshService,
        project: &str,
        kind: &str,
        reference: &str,
        meta: BTreeMap<String, String>,
    ) -> Artifact {
        block_on(svc.link_artifact(LinkArtifact {
            project: project.to_owned(),
            task: None,
            kind: kind.to_owned(),
            reference: reference.to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta,
        }))
        .expect("link artifact")
    }

    fn setup_project(svc: &MeshService) {
        block_on(svc.project_create(Parameters(ProjectCreateArgs {
            slug: "meta-proj".to_owned(),
            title: "Meta".to_owned(),
            description: String::new(),
            actor: "human:test".to_owned(),
        })))
        .expect("create project");
    }

    fn assert_invalid_meta(err: StoreError, key: &str) {
        let StoreError::Invalid(msg) = err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(
            msg.contains(key),
            "error should name failing key {key:?}, got: {msg}"
        );
    }

    #[test]
    fn link_artifact_meta_cap_key_count() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta = BTreeMap::new();
        for i in 0..16 {
            meta.insert(format!("k{i:02}"), "v".to_owned());
        }
        link_with_meta(&svc, "meta-proj", "verdict", "ref-ok", meta.clone());

        meta.insert("k16".to_owned(), "v".to_owned());
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: "meta-proj".to_owned(),
            task: None,
            kind: "verdict".to_owned(),
            reference: "ref-over".to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta,
        }))
        .expect_err("17 keys should reject");
        assert_invalid_meta(err, "k16");
    }

    #[test]
    fn link_artifact_meta_cap_key_length() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let ok_key = "a".repeat(64);
        let mut meta = BTreeMap::new();
        meta.insert(ok_key, "v".to_owned());
        link_with_meta(&svc, "meta-proj", "verdict", "ref-key-ok", meta);

        let bad_key = "a".repeat(65);
        let mut meta = BTreeMap::new();
        meta.insert(bad_key.clone(), "v".to_owned());
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: "meta-proj".to_owned(),
            task: None,
            kind: "verdict".to_owned(),
            reference: "ref-key-bad".to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta,
        }))
        .expect_err("65-byte key should reject");
        assert_invalid_meta(err, &bad_key);
    }

    #[test]
    fn link_artifact_meta_cap_value_length() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta = BTreeMap::new();
        meta.insert("k".to_owned(), "a".repeat(512));
        link_with_meta(&svc, "meta-proj", "verdict", "ref-val-ok", meta);

        let mut meta = BTreeMap::new();
        meta.insert("k".to_owned(), "a".repeat(513));
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: "meta-proj".to_owned(),
            task: None,
            kind: "verdict".to_owned(),
            reference: "ref-val-bad".to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta,
        }))
        .expect_err("513-byte value should reject");
        assert_invalid_meta(err, "k");
    }

    #[test]
    fn link_artifact_meta_cap_total_serialized() {
        // 15 keys (under the 16-key cap) each with a 265-byte value (under the
        // 512-byte value cap) sits exactly at the 4 KiB total-serialized cap;
        // bumping one value by a single byte must trip only the total-size
        // check, isolating it from the per-key/per-value caps.
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);

        let mut meta = BTreeMap::new();
        for i in 0..15 {
            meta.insert(format!("{i:02}"), "x".repeat(265));
        }
        let serialized = serde_json::to_string(&meta).expect("serialize");
        assert_eq!(serialized.len(), 4096, "fixture should sit at the cap");
        link_with_meta(&svc, "meta-proj", "verdict", "ref-total-ok", meta);

        let mut meta = BTreeMap::new();
        for i in 0..15 {
            meta.insert(format!("{i:02}"), "x".repeat(265));
        }
        meta.insert("14".to_owned(), "x".repeat(266));
        let serialized = serde_json::to_string(&meta).expect("serialize");
        assert_eq!(
            serialized.len(),
            4097,
            "fixture should exceed by exactly one byte: {serialized}"
        );
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: "meta-proj".to_owned(),
            task: None,
            kind: "verdict".to_owned(),
            reference: "ref-total-bad".to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta,
        }))
        .expect_err("oversized meta should reject");
        assert_invalid_meta(err, "14");
    }

    #[test]
    fn link_artifact_empty_meta_omitted_on_serialize() {
        let artifact = Artifact {
            id: "art_test".to_owned(),
            project: "prj_test".to_owned(),
            task: String::new(),
            kind: "commit".to_owned(),
            reference: "abc".to_owned(),
            label: "lbl".to_owned(),
            linked_at: now_utc(),
            actor: "human:test".to_owned(),
            meta: BTreeMap::new(),
        };
        let json = serde_json::to_string(&artifact).expect("serialize");
        assert!(
            !json.contains("\"meta\""),
            "empty meta should be omitted: {json}"
        );
    }

    #[test]
    fn link_artifact_unknown_kind_meta_round_trips() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta = BTreeMap::new();
        meta.insert("custom".to_owned(), "value".to_owned());
        let linked = link_with_meta(&svc, "meta-proj", "custom-kind", "ref-1", meta.clone());
        assert_eq!(linked.meta, meta);

        let Json(ArtifactListResult { artifacts: listed }) =
            block_on(svc.artifact_list(Parameters(ArtifactListArgs {
                project: "meta-proj".to_owned(),
                task: String::new(),
                kind: String::new(),
                reference: String::new(),
            })))
            .expect("list");
        let got = listed
            .into_iter()
            .find(|a| a.id == linked.id)
            .expect("artifact present");
        assert_eq!(got.kind, "custom-kind");
        assert_eq!(got.meta, meta, "meta must round-trip through artifact.list");

        // ref exact-match also surfaces meta, confirming the two filters compose.
        let Json(ArtifactListResult { artifacts: by_ref }) =
            block_on(svc.artifact_list(Parameters(ArtifactListArgs {
                project: "meta-proj".to_owned(),
                task: String::new(),
                kind: String::new(),
                reference: "ref-1".to_owned(),
            })))
            .expect("list by ref");
        assert_eq!(by_ref.len(), 1);
        assert_eq!(by_ref[0].id, linked.id);
        assert_eq!(by_ref[0].meta, meta);
    }

    #[test]
    fn link_artifact_identical_meta_is_idempotent() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta = BTreeMap::new();
        meta.insert("outcome".to_owned(), "pass".to_owned());
        let first = link_with_meta(&svc, "meta-proj", "verdict", "ref-dedup", meta.clone());
        let second = link_with_meta(&svc, "meta-proj", "verdict", "ref-dedup", meta);
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn link_artifact_differing_meta_rejects() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta1 = BTreeMap::new();
        meta1.insert("outcome".to_owned(), "pass".to_owned());
        link_with_meta(&svc, "meta-proj", "verdict", "ref-immut", meta1);

        let mut meta2 = BTreeMap::new();
        meta2.insert("outcome".to_owned(), "blocked".to_owned());
        let err = block_on(svc.link_artifact(LinkArtifact {
            project: "meta-proj".to_owned(),
            task: None,
            kind: "verdict".to_owned(),
            reference: "ref-immut".to_owned(),
            label: "label".to_owned(),
            actor: "human:test".to_owned(),
            meta: meta2,
        }))
        .expect_err("differing meta should reject");
        let StoreError::Invalid(msg) = err else {
            panic!("expected Invalid, got {err:?}");
        };
        assert!(msg.contains("meta is immutable"), "got: {msg}");
    }

    // Regression: the MCP tool method (`artifact_link`, returning
    // `Result<Json<_>, ErrorData>`) must map meta cap + immutability
    // violations to `invalid_params` — not `internal_error`. The inner
    // `link_artifact` returning `StoreError::Invalid` isn't enough; the
    // classification happens in `store_err_to_invalid` against
    // `USER_ERROR_MARKERS`, so this exercises the real MCP surface.
    fn link_args_with_meta(
        project: &str,
        kind: &str,
        reference: &str,
        meta: BTreeMap<String, String>,
    ) -> ArtifactLinkArgs {
        ArtifactLinkArgs {
            project: project.to_owned(),
            task: None,
            kind: kind.to_owned(),
            reference: reference.to_owned(),
            label: "label".to_owned(),
            meta,
            actor: "human:test".to_owned(),
        }
    }

    #[test]
    fn artifact_link_tool_maps_over_cap_meta_to_invalid_params() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta = BTreeMap::new();
        meta.insert("a".repeat(65), "v".to_owned());
        // `Json<Artifact>` isn't `Debug`, so drop the Ok payload before asserting.
        let err = block_on(svc.artifact_link(Parameters(link_args_with_meta(
            "meta-proj",
            "verdict",
            "ref-tool-cap",
            meta,
        ))))
        .map(|_| ())
        .expect_err("over-cap meta must be rejected");
        assert_eq!(
            err.code,
            ErrorCode::INVALID_PARAMS,
            "over-cap meta must surface as invalid_params, not internal_error: {err:?}"
        );
    }

    #[test]
    fn artifact_link_tool_maps_immutable_meta_to_invalid_params() {
        let (_tmp, svc) = fresh_service();
        setup_project(&svc);
        let mut meta1 = BTreeMap::new();
        meta1.insert("outcome".to_owned(), "pass".to_owned());
        block_on(svc.artifact_link(Parameters(link_args_with_meta(
            "meta-proj",
            "verdict",
            "ref-tool-immut",
            meta1,
        ))))
        .expect("first link");

        let mut meta2 = BTreeMap::new();
        meta2.insert("outcome".to_owned(), "blocked".to_owned());
        let err = block_on(svc.artifact_link(Parameters(link_args_with_meta(
            "meta-proj",
            "verdict",
            "ref-tool-immut",
            meta2,
        ))))
        .map(|_| ())
        .expect_err("differing meta re-link must be rejected");
        assert_eq!(
            err.code,
            ErrorCode::INVALID_PARAMS,
            "immutable-meta violation must surface as invalid_params, not internal_error: {err:?}"
        );
    }
}
