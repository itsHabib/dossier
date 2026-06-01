//! Round-trip properties for the on-disk markdown corpus. Each property
//! generates a domain entity, asks a write verb to persist it, asks a
//! read verb to load it back, and asserts equality of the user-supplied
//! fields. The point is to surface platform-specific or serializer-
//! specific bugs (CRLF handling, YAML quoting edge cases) that a
//! hand-written test would have to anticipate to find.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use dossier::domain::{new_id, PhaseListFilter, TaskListFilter};
use dossier::store::{FsStore, NewPhase, NewProject, NewTask};
use proptest::prelude::*;

mod common;
use common::{block_on, fresh_corpus, fresh_service};

// ---------- generators ---------------------------------------------------

fn slug() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9_-]{1,16}").expect("slug regex")
}

fn dep_id() -> impl Strategy<Value = String> {
    proptest::string::string_regex("tsk_[A-Z0-9]{26}").expect("dep id regex")
}

/// Title generator: punctuation + whitespace chars that exercise YAML
/// quoting (`:`, `#`, `-`, `!`, `?`, `,`) but no control chars or
/// leading/trailing whitespace.
fn title() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 :#\-_,.!?]{0,32}[A-Za-z0-9]|[A-Za-z0-9]")
        .expect("title regex")
}

/// Body generator: paragraph-shaped text. Excludes `#` to keep heading
/// patterns out (in particular `## Notes` is a reserved delimiter on
/// task bodies). Excludes leading/trailing whitespace so the round-trip
/// doesn't have to model the file-write trim.
fn body() -> impl Strategy<Value = String> {
    // Either empty body, or a non-empty body with no leading/trailing whitespace.
    prop_oneof![
        Just(String::new()),
        proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 \-_,.!?\n]{0,80}[A-Za-z0-9]")
            .expect("body regex"),
    ]
}

// ---------- properties ----------------------------------------------------

proptest! {
    /// Project create → get round-trip. Every field supplied by the
    /// caller must come back identical; server-stamped fields must
    /// match the value returned by `create_project`.
    #[test]
    fn project_round_trip(
        slug in slug(),
        title in title(),
        description in body(),
    ) {
        let (_tmp, store) = fresh_corpus();
        let created = store
            .create_project(NewProject {
                slug: slug.clone(),
                title: title.clone(),
                description: description.clone(),
                actor: "human:michael".into(),
            })
            .expect("create_project");
        let fetched = store.get_project(&slug).expect("get_project");
        prop_assert_eq!(&fetched.slug, &slug);
        prop_assert_eq!(&fetched.title, &title);
        prop_assert_eq!(&fetched.description, &description);
        prop_assert_eq!(&fetched.id, &created.id);
        prop_assert_eq!(fetched.created_at, created.created_at);
        prop_assert_eq!(fetched.status, created.status);
    }

    /// Phase create → list-and-find round-trip. Phase order is
    /// server-managed but stable per insertion: the first phase added
    /// to a fresh project gets `order = 1`.
    #[test]
    fn phase_round_trip(
        proj_slug in slug(),
        phase_slug in slug(),
        title in title(),
        body in body(),
    ) {
        prop_assume!(proj_slug != phase_slug);
        let (_tmp, store) = fresh_corpus();
        store
            .create_project(NewProject {
                slug: proj_slug.clone(),
                title: "P".into(),
                description: String::new(),
                actor: "human:michael".into(),
            })
            .expect("create_project");
        let owner = "team:frontend".to_owned();
        let created = store
            .add_phase(&NewPhase {
                project: proj_slug.clone(),
                slug: phase_slug.clone(),
                title: title.clone(),
                body: body.clone(),
                after_phase: None,
                actor: "human:michael".into(),
                owner: owner.clone(),
            })
            .expect("add_phase");
        let phases = store
            .list_phases(&PhaseListFilter {
                project: Some(proj_slug),
                ..Default::default()
            })
            .expect("list_phases");
        let fetched = phases
            .iter()
            .find(|p| p.slug == phase_slug)
            .expect("phase present after create");
        prop_assert_eq!(&fetched.slug, &phase_slug);
        prop_assert_eq!(&fetched.title, &title);
        prop_assert_eq!(&fetched.body, &body);
        prop_assert_eq!(fetched.order, created.order);
        prop_assert_eq!(&fetched.id, &created.id);
        prop_assert_eq!(fetched.status, created.status);
        prop_assert_eq!(&fetched.created_by, "human:michael");
        prop_assert_eq!(&fetched.owner, &owner);
    }

    /// Task create → list-and-find round-trip. Task body must not
    /// contain `## Notes` — the generator filters this out by
    /// excluding `#` from the body charset.
    #[test]
    fn task_round_trip(
        proj_slug in slug(),
        task_slug in slug(),
        title in title(),
        body in body(),
        use_deps in any::<bool>(),
        dep_id in dep_id(),
    ) {
        prop_assume!(proj_slug != task_slug);
        let (tmp, svc) = fresh_service();
        let store = FsStore::open(tmp.path()).expect("reopen");
        store
            .create_project(NewProject {
                slug: proj_slug.clone(),
                title: "P".into(),
                description: String::new(),
                actor: "human:michael".into(),
            })
            .expect("create_project");
        let depends_on = if use_deps {
            vec![dep_id]
        } else {
            vec![]
        };
        let created = block_on(svc.create_task(NewTask {
            project: proj_slug.clone(),
            phase: None,
            slug: task_slug.clone(),
            title: title.clone(),
            body: body.clone(),
            actor: "human:michael".into(),
            depends_on: depends_on.clone(),
        }))
        .expect("create_task");
        let tasks = store
            .list_tasks(&TaskListFilter {
                project: Some(proj_slug),
                ..Default::default()
            })
            .expect("list_tasks");
        let fetched = tasks
            .iter()
            .find(|t| t.slug == task_slug)
            .expect("task present after create");
        prop_assert_eq!(&fetched.slug, &task_slug);
        prop_assert_eq!(&fetched.title, &title);
        prop_assert_eq!(&fetched.body, &body);
        prop_assert_eq!(&fetched.id, &created.id);
        prop_assert_eq!(fetched.status, created.status);
        prop_assert_eq!(&fetched.depends_on, &depends_on);
    }
}

// ULID ID format property. `new_id` is `pub` so it can be exercised
// directly without a corpus.
//
// "For any prefix, the output is `<prefix>_<26 chars in Crockford
// base32>` and the alphabet contains none of `I L O U`."
proptest! {
    #[test]
    fn new_id_format(prefix in proptest::string::string_regex("[a-z]{1,4}").expect("prefix regex")) {
        let id = new_id(&prefix);
        let mut parts = id.splitn(2, '_');
        let p = parts.next().expect("prefix segment");
        let ulid = parts.next().expect("ulid segment");
        prop_assert_eq!(p, &prefix);
        prop_assert_eq!(ulid.len(), 26);
        // Crockford base32: uppercase A-Z and digits 0-9 only, minus I/L/O/U.
        prop_assert!(
            ulid.chars()
                .all(|c| (c.is_ascii_uppercase() || c.is_ascii_digit())
                    && !"ILOU".contains(c)),
            "ulid alphabet violation: {}",
            ulid
        );
    }
}
