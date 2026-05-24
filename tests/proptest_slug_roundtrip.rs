//! Property tests for slug validation and round-trip on the create
//! verbs. The slug rule per LAYOUT.md: lowercase ASCII letters, digits,
//! dashes, and underscores; non-empty. These tests check that the verbs
//! enforce the rule and that valid slugs survive a create-then-read
//! round-trip unchanged.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use dossier::store::{NewPhase, NewProject, NewTask};
use proptest::prelude::*;

mod common;
use common::fresh_corpus;

/// Oracle predicate mirroring `is_valid_slug` in src/store.rs. Replicated
/// here so the property test catches divergence — if the production
/// predicate ever drifts (e.g. allows uppercase, accepts an empty
/// string), the round-trip / rejection property will start failing.
fn looks_like_a_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Strategy for *valid* slugs: 2..=32 characters from the allowed set.
/// The 2-char lower bound keeps the collision rate <0.01% when three
/// independent slugs are drawn for the phase/task test — at 1 char,
/// the alphabet only has 37 values and `prop_assume!` discards ~15%
/// of cases.
fn valid_slug_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9_-]{2,32}").expect("slug regex")
}

/// Strategy for *arbitrary* strings, used to probe the rejection path.
/// We then assert about whether each string is or is not a valid slug.
fn arbitrary_string_strategy() -> impl Strategy<Value = String> {
    // Bounded length keeps the property fast; the interesting failure
    // modes (case, whitespace, punctuation, control chars) all surface
    // in short strings.
    proptest::string::string_regex(".{0,16}").expect("arbitrary string regex")
}

proptest! {
    /// Any string passing the slug oracle must create a project, and the
    /// returned project's slug must equal the input verbatim.
    #[test]
    fn project_create_round_trips_valid_slug(slug in valid_slug_strategy()) {
        let (_tmp, store) = fresh_corpus();
        let project = store
            .create_project(NewProject {
                slug: slug.clone(),
                title: "T".into(),
                description: String::new(),
                actor: "human:michael".into(),
            })
            .expect("create_project on valid slug");
        prop_assert_eq!(&project.slug, &slug);

        // get_project by the same slug returns the same record.
        let fetched = store.get_project(&slug).expect("get_project");
        prop_assert_eq!(&fetched.slug, &slug);
    }

    /// Phase and task creation must accept the same slug alphabet as
    /// project creation. Property: a valid slug accepted by
    /// `create_project` is also accepted by `add_phase` and
    /// `create_task` under that project.
    #[test]
    fn phase_and_task_accept_valid_slugs(
        proj_slug in valid_slug_strategy(),
        phase_slug in valid_slug_strategy(),
        task_slug in valid_slug_strategy(),
    ) {
        // Slugs must be distinct or the unique-constraint check kicks
        // in instead of the validation check we're probing.
        prop_assume!(proj_slug != phase_slug && proj_slug != task_slug && phase_slug != task_slug);

        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: proj_slug.clone(),
                title: "T".into(),
                description: String::new(),
                actor: "human:michael".into(),
            })
            .expect("create_project");
        let phase = store
            .add_phase(NewPhase {
                project: proj_slug.clone(),
                slug: phase_slug.clone(),
                title: "P".into(),
                body: String::new(),
                after_phase: None,
                actor: "human:michael".into(),
                            owner: "human:test".to_owned(),
            })
            .expect("add_phase on valid slug");
        prop_assert_eq!(phase.slug, phase_slug);

        let task = store
            .create_task(NewTask {
                project: proj_slug,
                phase: None,
                slug: task_slug.clone(),
                title: "Task".into(),
                body: String::new(),
                actor: "human:michael".into(),
                            depends_on: Vec::new(),
            })
            .expect("create_task on valid slug");
        prop_assert_eq!(task.slug, task_slug);
    }

    /// Any string that fails the oracle must be rejected by
    /// `create_project`. This is the dual of the round-trip property:
    /// together they assert that the verb's accept-set equals the
    /// oracle's accept-set.
    #[test]
    fn project_create_rejects_invalid_slug(s in arbitrary_string_strategy()) {
        prop_assume!(!looks_like_a_slug(&s));
        let (_tmp, store) = fresh_corpus();
        let result = store.create_project(NewProject {
            slug: s,
            title: "T".into(),
            description: String::new(),
            actor: "human:michael".into(),
        });
        prop_assert!(result.is_err());
    }

    /// `add_phase` calls `is_valid_slug` independently — verify it
    /// rejects the same set as `create_project`. Without this, a
    /// copy-paste error dropping the phase slug check would go
    /// undetected.
    #[test]
    fn phase_add_rejects_invalid_slug(s in arbitrary_string_strategy()) {
        prop_assume!(!looks_like_a_slug(&s));
        let (_tmp, store) = fresh_corpus();
        // Anchor project with a valid slug so the phase validation is
        // the only thing being probed.
        store.create_project(NewProject {
            slug: "host".into(),
            title: "T".into(),
            description: String::new(),
            actor: "human:michael".into(),
        }).expect("create_project");
        let result = store.add_phase(NewPhase {
            project: "host".into(),
            slug: s,
            title: "P".into(),
            body: String::new(),
            after_phase: None,
            actor: "human:michael".into(),
                        owner: "human:test".to_owned(),
        });
        prop_assert!(result.is_err());
    }

    /// `create_task` also has its own slug-validation call site
    /// (separate from `add_phase`). Same dual property as the other
    /// rejection tests.
    #[test]
    fn task_create_rejects_invalid_slug(s in arbitrary_string_strategy()) {
        prop_assume!(!looks_like_a_slug(&s));
        let (_tmp, store) = fresh_corpus();
        store.create_project(NewProject {
            slug: "host".into(),
            title: "T".into(),
            description: String::new(),
            actor: "human:michael".into(),
        }).expect("create_project");
        let result = store.create_task(NewTask {
            project: "host".into(),
            phase: None,
            slug: s,
            title: "Task".into(),
            body: String::new(),
            actor: "human:michael".into(),
                        depends_on: Vec::new(),
        });
        prop_assert!(result.is_err());
    }
}
