//! Property tests for serialize/parse byte parity on the shared corpus format.
//! Included from `store::proptest_serialize_roundtrip` for `pub(crate)` access.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use chrono::{DateTime, Utc};
use proptest::prelude::*;

use crate::domain::{Note, Phase, PhaseStatus, Project, ProjectStatus, Task, TaskStatus};
use crate::store::{
    notes_lines_for_task, parse_phase, parse_project, parse_task, serialize_phase_file,
    serialize_project_file, serialize_task_file,
};

fn slug() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[a-z0-9_-]{1,16}").expect("slug regex")
}

fn title() -> impl Strategy<Value = String> {
    proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 :#\-_,.!?]{0,32}[A-Za-z0-9]|[A-Za-z0-9]")
        .expect("title regex")
}

fn body() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 \-_,.!?\n]{0,80}[A-Za-z0-9]")
            .expect("body regex"),
    ]
}

fn utc_time() -> impl Strategy<Value = DateTime<Utc>> {
    (0i64..4_000_000_000)
        .prop_map(|secs| DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now))
}

fn project_status() -> impl Strategy<Value = ProjectStatus> {
    prop_oneof![
        Just(ProjectStatus::Planning),
        Just(ProjectStatus::Active),
        Just(ProjectStatus::Paused),
        Just(ProjectStatus::Done),
        Just(ProjectStatus::Abandoned),
    ]
}

fn phase_status() -> impl Strategy<Value = PhaseStatus> {
    prop_oneof![
        Just(PhaseStatus::Pending),
        Just(PhaseStatus::Active),
        Just(PhaseStatus::Done),
        Just(PhaseStatus::Skipped),
    ]
}

fn task_status() -> impl Strategy<Value = TaskStatus> {
    prop_oneof![
        Just(TaskStatus::Todo),
        Just(TaskStatus::Claimed),
        Just(TaskStatus::InProgress),
        Just(TaskStatus::Blocked),
        Just(TaskStatus::Done),
        Just(TaskStatus::Cancelled),
    ]
}

fn project_strategy() -> impl Strategy<Value = Project> {
    (
        proptest::string::string_regex("prj_[A-Z0-9]{26}").expect("id"),
        slug(),
        title(),
        body(),
        project_status(),
        utc_time(),
        utc_time(),
        proptest::string::string_regex("human:[a-z0-9_-]{1,16}").expect("actor"),
    )
        .prop_map(
            |(id, slug, title, description, status, created_at, updated_at, created_by)| Project {
                id,
                slug,
                title,
                description,
                status,
                created_at,
                updated_at,
                created_by,
            },
        )
}

fn phase_strategy() -> impl Strategy<Value = Phase> {
    (
        proptest::string::string_regex("prj_[A-Z0-9]{26}").expect("project id"),
        proptest::string::string_regex("phs_[A-Z0-9]{26}").expect("id"),
        slug(),
        title(),
        body(),
        0i32..100,
        phase_status(),
        utc_time(),
        utc_time(),
        proptest::string::string_regex("human:[a-z0-9_-]{1,16}").expect("actor"),
        proptest::string::string_regex("human:[a-z0-9_-]{1,16}").expect("owner"),
    )
        .prop_map(
            |(
                project,
                id,
                slug,
                title,
                body,
                order,
                status,
                created_at,
                updated_at,
                created_by,
                owner,
            )| Phase {
                id,
                project,
                slug,
                title,
                body,
                order,
                status,
                created_at,
                updated_at,
                created_by,
                owner,
            },
        )
}

fn note_strategy() -> impl Strategy<Value = Note> {
    (
        proptest::string::string_regex("human:[a-z0-9_-]{1,16}").expect("actor"),
        proptest::string::string_regex(r"[A-Za-z0-9][A-Za-z0-9 \-_,.!?]{0,32}").expect("body"),
        utc_time(),
    )
        .prop_map(|(actor, body, posted_at)| Note {
            actor,
            body: body.trim().to_owned(),
            posted_at,
        })
        .prop_filter("note body non-empty after trim", |n| !n.body.is_empty())
}

fn task_strategy() -> impl Strategy<Value = Task> {
    (
        proptest::string::string_regex("prj_[A-Z0-9]{26}").expect("project id"),
        slug(),
        proptest::string::string_regex("tsk_[A-Z0-9]{26}").expect("id"),
        proptest::string::string_regex("phs_[A-Z0-9]{26}").expect("phase id"),
        slug(),
        title(),
        body(),
        task_status(),
        utc_time(),
        utc_time(),
        proptest::option::of(proptest::collection::vec(note_strategy(), 0..3)),
    )
        .prop_map(
            |(
                project,
                project_slug,
                id,
                phase,
                slug,
                title,
                body,
                status,
                created_at,
                updated_at,
                notes,
            )| Task {
                id,
                project,
                project_slug,
                phase,
                slug,
                title,
                body,
                status,
                assignee: String::new(),
                claimed_at: None,
                completed_at: None,
                created_at,
                updated_at,
                notes: notes.unwrap_or_default(),
                depends_on: Vec::new(),
            },
        )
}

proptest! {
    #[test]
    fn project_serialize_parse_roundtrip(project in project_strategy()) {
        let raw = serialize_project_file(&project).expect("serialize project");
        let parsed = parse_project(&raw, true).expect("parse project");
        prop_assert_eq!(&parsed.id, &project.id);
        prop_assert_eq!(&parsed.slug, &project.slug);
        prop_assert_eq!(&parsed.title, &project.title);
        prop_assert_eq!(&parsed.description, &project.description);
        prop_assert_eq!(parsed.status, project.status);
        prop_assert_eq!(parsed.created_at, project.created_at);
        prop_assert_eq!(parsed.updated_at, project.updated_at);
        prop_assert_eq!(&parsed.created_by, &project.created_by);
    }

    #[test]
    fn phase_serialize_parse_roundtrip(phase in phase_strategy()) {
        let raw = serialize_phase_file(&phase).expect("serialize phase");
        let parsed = parse_phase(&raw).expect("parse phase");
        prop_assert_eq!(&parsed.id, &phase.id);
        prop_assert_eq!(&parsed.project, &phase.project);
        prop_assert_eq!(&parsed.slug, &phase.slug);
        prop_assert_eq!(&parsed.title, &phase.title);
        prop_assert_eq!(&parsed.body, &phase.body);
        prop_assert_eq!(parsed.order, phase.order);
        prop_assert_eq!(parsed.status, phase.status);
        prop_assert_eq!(parsed.created_at, phase.created_at);
        prop_assert_eq!(parsed.updated_at, phase.updated_at);
        prop_assert_eq!(&parsed.created_by, &phase.created_by);
        prop_assert_eq!(&parsed.owner, &phase.owner);
    }

    #[test]
    fn task_serialize_parse_roundtrip(task in task_strategy()) {
        let notes_lines = notes_lines_for_task(&task);
        let raw = serialize_task_file(&task, &notes_lines).expect("serialize task");
        let (parsed, _) = parse_task(&raw, &task.project_slug).expect("parse task");
        prop_assert_eq!(&parsed.id, &task.id);
        prop_assert_eq!(&parsed.project, &task.project);
        prop_assert_eq!(&parsed.project_slug, &task.project_slug);
        prop_assert_eq!(&parsed.phase, &task.phase);
        prop_assert_eq!(&parsed.slug, &task.slug);
        prop_assert_eq!(&parsed.title, &task.title);
        prop_assert_eq!(&parsed.body, &task.body);
        prop_assert_eq!(parsed.status, task.status);
        prop_assert_eq!(&parsed.assignee, &task.assignee);
        prop_assert_eq!(parsed.claimed_at, task.claimed_at);
        prop_assert_eq!(parsed.completed_at, task.completed_at);
        prop_assert_eq!(parsed.created_at, task.created_at);
        prop_assert_eq!(parsed.updated_at, task.updated_at);
        prop_assert_eq!(&parsed.depends_on, &task.depends_on);
        prop_assert_eq!(parsed.notes.len(), task.notes.len());
        for (got, want) in parsed.notes.iter().zip(task.notes.iter()) {
            prop_assert_eq!(&got.actor, &want.actor);
            prop_assert_eq!(&got.body, &want.body);
            prop_assert_eq!(got.posted_at, want.posted_at);
        }
    }
}
