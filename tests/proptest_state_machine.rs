//! Model-based property test for the task state machine.
//!
//! The model is the enum `Op` mirroring the legal MCP-facing verbs:
//! `task.claim`, `task.update`, `task.complete`. We generate a short
//! sequence of `Op`s and assert invariants after every step — once
//! against a real `MeshService` and once against the pure `domain` transition
//! fns (no I/O). Shrinking finds the smallest violating sequence.
//!
//! Invariants:
//!
//!  1. **Terminal absorption.** Once `Done` or `Cancelled`, the status
//!     never changes again, and `completed_at` (when set) never changes.
//!  2. **Assignee/status coupling.** Either status is `Todo` with empty
//!     assignee, OR status ∈ {`Claimed`, `InProgress`, `Blocked`} with a
//!     non-empty assignee, OR status ∈ {`Done`, `Cancelled`} (terminal —
//!     either assignee state is acceptable).
//!  3. **`update_task` may not reach `Claimed` or `Done`.** Those targets
//!     belong to `claim_task` and `complete_task`; `update_task` with
//!     either target must error.
//!  4. **`complete_task` succeeds from `Todo`, `Claimed` (same actor), or
//!     `InProgress`.** Cross-actor `Claimed`, `Blocked`, and terminal
//!     sources error without mutation.
//!  5. **Timestamp monotonicity.** `updated_at` never moves backwards
//!     across successful operations.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use chrono::{DateTime, TimeDelta, Utc};
use dossier::domain::{
    apply_claim_task, apply_complete_task, apply_task_body_update, apply_task_complete,
    apply_task_status_update, validate_task_update_transition, ClaimTask, CompleteTask, NewProject,
    NewTask, Task, TaskListFilter, TaskStatus, UpdateTask,
};
use dossier::server::MeshService;
use dossier::store::FsStore;
use proptest::prelude::*;

mod common;
use common::{block_on, create_project, fresh_service};

const PROJECT_SLUG: &str = "model-test";
const TASK_SLUG: &str = "subject";

/// One verb invocation in the model. `actor` and `body` are small
/// domains so that collisions and identical bodies appear often enough
/// to exercise idempotent paths. Every variant carries its own actor so
/// the model never falls back to a placeholder name.
#[derive(Debug, Clone)]
enum Op {
    Claim { actor: String },
    UpdateStatus { to: TaskStatus, actor: String },
    Complete { actor: String },
    UpdateBody { body: String, actor: String },
}

fn actor_strategy() -> impl Strategy<Value = String> {
    prop_oneof![Just("alice".to_owned()), Just("bob".to_owned())]
}

fn status_strategy() -> impl Strategy<Value = TaskStatus> {
    prop_oneof![
        Just(TaskStatus::Todo),
        Just(TaskStatus::Claimed),
        Just(TaskStatus::InProgress),
        Just(TaskStatus::Blocked),
        Just(TaskStatus::Done),
        Just(TaskStatus::Cancelled),
    ]
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        actor_strategy().prop_map(|actor| Op::Claim { actor }),
        (status_strategy(), actor_strategy())
            .prop_map(|(to, actor)| Op::UpdateStatus { to, actor }),
        actor_strategy().prop_map(|actor| Op::Complete { actor }),
        (
            proptest::string::string_regex("[a-z0-9 ]{0,16}").expect("body regex"),
            actor_strategy()
        )
            .prop_map(|(body, actor)| Op::UpdateBody { body, actor }),
    ]
}

fn apply_service(svc: &MeshService, op: Op, task_id: &str) {
    let _ = match op {
        Op::Claim { actor } => block_on(svc.claim_task(&ClaimTask {
            id: task_id.to_owned(),
            actor,
        })),
        Op::UpdateStatus { to, actor } => block_on(svc.update_task(UpdateTask {
            id: task_id.to_owned(),
            body: None,
            status: Some(to),
            note: None,
            actor,
            depends_on: None,
            blocked_by: None,
        })),
        Op::Complete { actor } => block_on(svc.complete_task(CompleteTask {
            id: task_id.to_owned(),
            note: None,
            actor,
        })),
        Op::UpdateBody { body, actor } => block_on(svc.update_task(UpdateTask {
            id: task_id.to_owned(),
            body: Some(body),
            status: None,
            note: None,
            actor,
            depends_on: None,
            blocked_by: None,
        })),
    };
}

fn fresh_task() -> Task {
    let now = Utc::now();
    Task {
        id: "tsk_01TEST000000000000000000".to_owned(),
        project: "prj_test".to_owned(),
        project_slug: PROJECT_SLUG.to_owned(),
        phase: String::new(),
        slug: TASK_SLUG.to_owned(),
        title: "subject".to_owned(),
        body: String::new(),
        status: TaskStatus::Todo,
        assignee: String::new(),
        claimed_at: None,
        completed_at: None,
        created_at: now,
        updated_at: now,
        notes: Vec::new(),
        depends_on: Vec::new(),
        blocked_by: Vec::new(),
    }
}

fn tick(now: &mut DateTime<Utc>) {
    *now += TimeDelta::seconds(1);
}

fn apply_pure(task: &mut Task, op: Op, now: &mut DateTime<Utc>) -> Result<(), anyhow::Error> {
    tick(now);
    match op {
        Op::Claim { actor } => {
            *task = apply_claim_task(task.clone(), &actor, *now)?;
        }
        Op::UpdateStatus { to, actor: _ } => {
            *task = apply_task_status_update(task.clone(), to, *now)?;
        }
        Op::Complete { actor } => {
            *task = apply_task_complete(task.clone(), &actor, *now)?;
        }
        Op::UpdateBody { body, actor: _ } => {
            *task = apply_task_body_update(task.clone(), body, *now)?;
        }
    }
    Ok(())
}

fn current_state(store: &FsStore) -> Task {
    let tasks = store
        .list_tasks(&TaskListFilter {
            project: Some(PROJECT_SLUG.to_owned()),
            ..Default::default()
        })
        .expect("list_tasks");
    tasks
        .into_iter()
        .find(|t| t.slug == TASK_SLUG)
        .expect("subject task present")
}

const fn is_terminal(s: TaskStatus) -> bool {
    matches!(s, TaskStatus::Done | TaskStatus::Cancelled)
}

const fn is_held(s: TaskStatus) -> bool {
    matches!(
        s,
        TaskStatus::Claimed | TaskStatus::InProgress | TaskStatus::Blocked
    )
}

fn check_assignee_status_coupling(status: TaskStatus, assignee: &str) -> Option<String> {
    match status {
        TaskStatus::Todo if !assignee.is_empty() => {
            Some(format!("Todo with assignee {assignee} (corrupt state)"))
        }
        s if is_held(s) && assignee.is_empty() => {
            Some(format!("{s:?} with no assignee (corrupt state)"))
        }
        _ => None,
    }
}

fn explicit_three_call_complete(mut task: Task, actor: &str, now: DateTime<Utc>) -> Task {
    if task.status == TaskStatus::Todo {
        task = apply_claim_task(task, actor, now).expect("claim");
    }
    if task.status == TaskStatus::Claimed {
        task = apply_task_status_update(task, TaskStatus::InProgress, now).expect("in_progress");
    }
    apply_complete_task(task, now).expect("complete")
}

fn complete_should_succeed(pre: &Task, actor: &str) -> bool {
    match pre.status {
        TaskStatus::InProgress => true,
        TaskStatus::Todo => pre.assignee.is_empty(),
        TaskStatus::Claimed => pre.assignee == actor,
        TaskStatus::Done | TaskStatus::Cancelled | TaskStatus::Blocked => false,
    }
}

fn assert_invariants_after_step(
    pre: &Task,
    post: &Task,
    op: &Op,
    once_terminal: &mut Option<TaskStatus>,
    frozen_completed_at: &mut Option<DateTime<Utc>>,
    last_updated_at: &mut DateTime<Utc>,
) -> Result<(), TestCaseError> {
    if let Op::UpdateStatus { to, .. } = op {
        if matches!(*to, TaskStatus::Claimed | TaskStatus::Done) {
            prop_assert_eq!(post.status, pre.status);
            prop_assert_eq!(&post.assignee, &pre.assignee);
            prop_assert_eq!(post.updated_at, pre.updated_at);
            return Ok(());
        }
    }
    if let Op::Complete { actor } = op {
        if !complete_should_succeed(pre, actor) {
            prop_assert_eq!(post.status, pre.status);
            prop_assert_eq!(post.updated_at, pre.updated_at);
            return Ok(());
        }
    }

    let was_complete_success =
        matches!(op, Op::Complete { actor } if complete_should_succeed(pre, actor));
    if was_complete_success {
        prop_assert_eq!(post.status, TaskStatus::Done);
        prop_assert!(post.completed_at.is_some());
        prop_assert!(!post.assignee.is_empty());
    }

    if let Some(t) = *once_terminal {
        prop_assert_eq!(
            post.status,
            t,
            "invariant 1 violated: status changed after entering terminal {:?}",
            t,
        );
        prop_assert_eq!(
            post.completed_at,
            *frozen_completed_at,
            "invariant 1 violated: completed_at changed after terminal",
        );
    } else if is_terminal(post.status) {
        *once_terminal = Some(post.status);
        *frozen_completed_at = post.completed_at;
    }

    if let Some(reason) = check_assignee_status_coupling(post.status, &post.assignee) {
        prop_assert!(false, "invariant 2 violated: {}", reason);
    }

    prop_assert!(
        post.updated_at >= *last_updated_at,
        "invariant 5 violated: updated_at moved backwards ({} -> {})",
        *last_updated_at,
        post.updated_at,
    );
    *last_updated_at = post.updated_at;
    Ok(())
}

proptest! {
    #![proptest_config(proptest::test_runner::Config { cases: 64, ..proptest::test_runner::Config::default() })]

    #[test]
    fn state_machine_invariants(ops in proptest::collection::vec(op_strategy(), 0..=8)) {
        let (tmp, svc) = fresh_service();
        let store = FsStore::open(tmp.path()).expect("reopen for reads");
        create_project(
            &svc,
            NewProject {
                slug: PROJECT_SLUG.into(),
                title: "M".into(),
                description: String::new(),
                actor: "human:michael".into(),
            },
        );
        let task = block_on(svc.create_task(NewTask {
            project: PROJECT_SLUG.into(),
            phase: None,
            slug: TASK_SLUG.into(),
            title: "subject".into(),
            body: String::new(),
            actor: "human:michael".into(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })).expect("create_task");
        let task_id = task.id.clone();

        prop_assert_eq!(task.status, TaskStatus::Todo);
        prop_assert!(task.assignee.is_empty());

        let mut last_updated_at = task.updated_at;
        let mut once_terminal: Option<TaskStatus> = None;
        let mut frozen_completed_at: Option<DateTime<Utc>> = task.completed_at;

        for op in &ops {
            let pre = current_state(&store);
            if let Op::UpdateStatus { to, actor } = op.clone() {
                if matches!(to, TaskStatus::Claimed | TaskStatus::Done) {
                    let result = block_on(svc.update_task(UpdateTask {
                        id: task_id.clone(),
                        body: None,
                        status: Some(to),
                        note: None,
                        actor,
                        depends_on: None,
                        blocked_by: None,
                    }));
                    prop_assert!(result.is_err(), "invariant 3 violated: update accepted {:?}", to);
                    let post = current_state(&store);
                    assert_invariants_after_step(
                        &pre, &post, op, &mut once_terminal, &mut frozen_completed_at, &mut last_updated_at,
                    )?;
                    continue;
                }
            }
            if let Op::Complete { actor } = op.clone() {
                if !complete_should_succeed(&pre, &actor) {
                    let result = block_on(svc.complete_task(CompleteTask {
                        id: task_id.clone(),
                        note: None,
                        actor,
                    }));
                    prop_assert!(result.is_err());
                    let post = current_state(&store);
                    assert_invariants_after_step(
                        &pre, &post, op, &mut once_terminal, &mut frozen_completed_at,
                        &mut last_updated_at,
                    )?;
                    continue;
                }
            }
            apply_service(&svc, op.clone(), &task_id);
            let post = current_state(&store);
            assert_invariants_after_step(
                &pre, &post, op, &mut once_terminal, &mut frozen_completed_at, &mut last_updated_at,
            )?;
        }
    }

    #[test]
    fn state_machine_invariants_pure(ops in proptest::collection::vec(op_strategy(), 0..=8)) {
        let mut task = fresh_task();
        let mut now = task.updated_at;
        let mut last_updated_at = task.updated_at;
        let mut once_terminal: Option<TaskStatus> = None;
        let mut frozen_completed_at: Option<DateTime<Utc>> = task.completed_at;

        for op in ops {
            let pre = task.clone();
            if let Op::UpdateStatus { to, .. } = &op {
                if matches!(*to, TaskStatus::Claimed | TaskStatus::Done) {
                    prop_assert!(validate_task_update_transition(pre.status, *to).is_err());
                    prop_assert_eq!(task.status, pre.status);
                    continue;
                }
            }
            if let Op::Complete { actor } = &op {
                if !complete_should_succeed(&pre, actor) {
                    let err = apply_pure(&mut task, op.clone(), &mut now);
                    prop_assert!(err.is_err());
                    prop_assert_eq!(task.status, pre.status);
                    continue;
                }
            }

            if apply_pure(&mut task, op.clone(), &mut now).is_err() {
                prop_assert_eq!(task.status, pre.status);
                continue;
            }
            assert_invariants_after_step(
                &pre, &task, &op, &mut once_terminal, &mut frozen_completed_at, &mut last_updated_at,
            )?;
        }
    }

    #[test]
    fn complete_compound_matches_explicit_path(
        actor in actor_strategy(),
        start in prop_oneof![Just(TaskStatus::Todo), Just(TaskStatus::Claimed)],
    ) {
        let now = Utc::now();
        let mut start_task = fresh_task();
        start_task.status = start;
        if start == TaskStatus::Claimed {
            actor.clone_into(&mut start_task.assignee);
            start_task.claimed_at = Some(now - TimeDelta::hours(1));
        }

        let explicit = explicit_three_call_complete(start_task.clone(), &actor, now);
        let compound = apply_task_complete(start_task, &actor, now).expect("compound path");

        prop_assert_eq!(compound.status, explicit.status);
        prop_assert_eq!(&compound.assignee, &explicit.assignee);
        prop_assert_eq!(compound.claimed_at, explicit.claimed_at);
        prop_assert_eq!(compound.completed_at, explicit.completed_at);
        prop_assert_eq!(compound.notes, explicit.notes);
    }

    #[test]
    fn claim_is_idempotent_for_same_actor(actor in actor_strategy()) {
        let (tmp, svc) = fresh_service();
        let _store = FsStore::open(tmp.path()).expect("reopen for project create");
        create_project(
            &svc,
            NewProject {
                slug: PROJECT_SLUG.into(),
                title: "M".into(),
                description: String::new(),
                actor: "human:michael".into(),
            },
        );
        let task = block_on(svc.create_task(NewTask {
            project: PROJECT_SLUG.into(),
            phase: None,
            slug: TASK_SLUG.into(),
            title: "subject".into(),
            body: String::new(),
            actor: "human:michael".into(),
            depends_on: Vec::new(),
            blocked_by: Vec::new(),
        })).expect("create_task");

        let first = block_on(svc.claim_task(&ClaimTask {
            id: task.id.clone(),
            actor: actor.clone(),
        })).expect("first claim");
        let second = block_on(svc.claim_task(&ClaimTask {
            id: task.id,
            actor,
        })).expect("second claim (no-op)");

        prop_assert_eq!(first.status, second.status);
        prop_assert_eq!(&first.assignee, &second.assignee);
        prop_assert_eq!(first.claimed_at, second.claimed_at);
        prop_assert_eq!(first.updated_at, second.updated_at);
    }
}

// --- Explicit claim matrix (every branch; no blind spots) ---

fn corrupt_held_empty_assignee() -> Task {
    let mut t = fresh_task();
    t.status = TaskStatus::Claimed;
    t
}

fn corrupt_todo_with_assignee(holder: &str) -> Task {
    let mut t = fresh_task();
    holder.clone_into(&mut t.assignee);
    t
}

#[test]
fn claim_matrix_todo_empty_assignee_claims() {
    let now = Utc::now();
    let out = apply_claim_task(fresh_task(), "alice", now).expect("claim");
    assert_eq!(out.status, TaskStatus::Claimed);
    assert_eq!(out.assignee, "alice");
    assert_eq!(out.claimed_at, Some(now));
}

#[test]
fn claim_matrix_same_actor_noop() {
    let now = Utc::now();
    let held = apply_claim_task(fresh_task(), "alice", now).expect("claim");
    let second =
        apply_claim_task(held.clone(), "alice", now + TimeDelta::hours(1)).expect("re-claim");
    assert_eq!(held.status, second.status);
    assert_eq!(held.assignee, second.assignee);
    assert_eq!(held.claimed_at, second.claimed_at);
    assert_eq!(held.updated_at, second.updated_at);
}

#[test]
fn claim_matrix_different_actor_rejects() {
    let now = Utc::now();
    let held = apply_claim_task(fresh_task(), "alice", now).expect("claim");
    let err = apply_claim_task(held, "bob", now).unwrap_err();
    assert!(err.to_string().contains("task already claimed by alice"));
}

#[test]
fn claim_matrix_terminal_rejects() {
    let now = Utc::now();
    for status in [TaskStatus::Done, TaskStatus::Cancelled] {
        let mut t = fresh_task();
        t.status = status;
        let err = apply_claim_task(t, "alice", now).unwrap_err();
        assert!(err
            .to_string()
            .contains("cannot claim task in terminal state"));
    }
}

#[test]
fn claim_matrix_corrupt_todo_with_assignee_rejects() {
    let now = Utc::now();
    let err = apply_claim_task(corrupt_todo_with_assignee("alice"), "alice", now).unwrap_err();
    assert!(err.to_string().contains("corrupt state"));
    let err = apply_claim_task(corrupt_todo_with_assignee("alice"), "bob", now).unwrap_err();
    assert!(err.to_string().contains("corrupt state"));
}

#[test]
fn claim_matrix_corrupt_held_empty_assignee_rejects() {
    let now = Utc::now();
    let err = apply_claim_task(corrupt_held_empty_assignee(), "alice", now).unwrap_err();
    assert!(err.to_string().contains("corrupt state"));
}
