//! Phase verbs: DTOs and service-layer policy for `phase.*`.
//!
//! The `#[tool]` wrappers stay in the parent module's single
//! `#[tool_router(server_handler)]` block (rmcp's macro scans one impl
//! block); this module owns the argument/result DTOs, the ordered-insert
//! CAS policy, and the phase tests.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::domain::{
    compute_new_phase_order, is_valid_slug, new_id, resolve_status, Phase, PhaseListFilter,
    PhaseOrderField, PhaseStatus,
};
use crate::store::{now_utc, NewPhase, StoreError, UpdatePhase, Versioned};

use super::{invalid_msg, MeshService};

/// Bounded retry budget for concurrent `phase.add` writers racing on one project.
const PHASE_ADD_MAX_RETRIES: u32 = 8;

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
    use crate::server::testutil::{
        assert_rejects_invalid_project_slug, block_on, fresh_service, seed_project,
        INVALID_PROJECT_SLUG,
    };
    use crate::store::{FsStore, NewProject};
    use rmcp::handler::server::wrapper::{Json, Parameters};

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
}
