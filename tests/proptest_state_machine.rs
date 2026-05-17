//! Model-based property test for the task state machine.
//!
//! The model is the enum `Op` mirroring the legal MCP-facing verbs:
//! `task.claim`, `task.update`, `task.complete`. We generate a short
//! sequence of `Op`s, execute each against a real `FsStore` (success or
//! failure), and after every step assert the invariants the state
//! machine is required to uphold — independent of which sequence got us
//! here. Shrinking finds the smallest sequence that violates an
//! invariant when one fails.
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
//!  4. **`complete_task` requires `InProgress`.** Any other source errors.
//!  5. **Timestamp monotonicity.** `updated_at` never moves backwards
//!     across successful operations.

#![allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    reason = "test module"
)]

use chrono::{DateTime, Utc};
use dossier::domain::TaskStatus;
use dossier::store::{ClaimTask, CompleteTask, FsStore, NewProject, NewTask, UpdateTask};
use proptest::prelude::*;

mod common;
use common::fresh_corpus;

const PROJECT_SLUG: &str = "model-test";
const TASK_SLUG: &str = "subject";

/// One verb invocation in the model. `actor` and `body` are small
/// domains so that collisions and identical bodies appear often enough
/// to exercise idempotent paths.
#[derive(Debug, Clone)]
enum Op {
    Claim { actor: String },
    UpdateStatus { to: TaskStatus },
    Complete { actor: String },
    UpdateBody { body: String, actor: String },
}

fn actor_strategy() -> impl Strategy<Value = String> {
    // Two actors so same-actor re-claim and different-actor claim both
    // appear with reasonable frequency in any sequence of length >= 2.
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
        status_strategy().prop_map(|to| Op::UpdateStatus { to }),
        actor_strategy().prop_map(|actor| Op::Complete { actor }),
        (
            proptest::string::string_regex("[a-z0-9 ]{0,16}").expect("body regex"),
            actor_strategy()
        )
            .prop_map(|(body, actor)| Op::UpdateBody { body, actor }),
    ]
}

/// Apply one op against the store. Ignore the Result — we only care
/// about the *post-state* visible via `list_tasks`. (The Result is
/// indirectly verified by the invariants: e.g. invariant 3 says that
/// any successful `update_task` to `Claimed` is a bug.)
fn apply(store: &FsStore, op: Op, task_id: &str) {
    let actor = "ignored-by-this-call".to_owned();
    let _ = match op {
        Op::Claim { actor } => store.claim_task(ClaimTask {
            id: task_id.to_owned(),
            actor,
        }),
        Op::UpdateStatus { to } => store.update_task(UpdateTask {
            id: task_id.to_owned(),
            body: None,
            status: Some(to),
            note: None,
            actor,
        }),
        Op::Complete { actor } => store.complete_task(CompleteTask {
            id: task_id.to_owned(),
            note: None,
            actor,
        }),
        Op::UpdateBody { body, actor } => store.update_task(UpdateTask {
            id: task_id.to_owned(),
            body: Some(body),
            status: None,
            note: None,
            actor,
        }),
    };
}

/// Reload the task after each op. Returns the up-to-date state of the
/// single task we're tracking.
fn current_state(store: &FsStore) -> dossier::domain::Task {
    let tasks = store.list_tasks(PROJECT_SLUG).expect("list_tasks");
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

/// Invariant 2 in one place so a failing assertion points at the right
/// rule. `Some(reason)` indicates a violation; `None` means OK.
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

proptest! {
    // 64 cases × up to 8 ops × real-filesystem corpus is ~4s wall-clock;
    // the default 256 took 16s. Override locally via `PROPTEST_CASES=N`
    // when you want a thorough check (e.g. before a release).
    #![proptest_config(proptest::test_runner::Config { cases: 64, ..proptest::test_runner::Config::default() })]

    /// Replay an arbitrary sequence of operations against the real
    /// state machine and assert all five invariants after each step.
    #[test]
    fn state_machine_invariants(ops in proptest::collection::vec(op_strategy(), 0..=8)) {
        let (_tmp, store) = fresh_corpus();
        store.create_project(NewProject {
            slug: PROJECT_SLUG.into(),
            title: "M".into(),
            description: String::new(),
            actor: "human:michael".into(),
        }).expect("create_project");
        let task = store.create_task(NewTask {
            project: PROJECT_SLUG.into(),
            phase: None,
            slug: TASK_SLUG.into(),
            title: "subject".into(),
            body: String::new(),
            actor: "human:michael".into(),
        }).expect("create_task");
        let task_id = task.id.clone();

        // Sanity: a freshly-created task is `Todo`, has no assignee,
        // has no claimed_at / completed_at, and updated_at is set.
        prop_assert_eq!(task.status, TaskStatus::Todo);
        prop_assert!(task.assignee.is_empty());
        prop_assert!(task.claimed_at.is_none());
        prop_assert!(task.completed_at.is_none());

        let mut last_updated_at: DateTime<Utc> = task.updated_at;
        let mut once_terminal: Option<TaskStatus> = None;
        let mut frozen_completed_at: Option<DateTime<Utc>> = task.completed_at;

        for op in ops {
            // Probe invariants 3 and 4 BEFORE applying the op so we can
            // assert that disallowed verbs error.
            let pre = current_state(&store);
            if let Op::UpdateStatus { to } = op.clone() {
                if matches!(to, TaskStatus::Claimed | TaskStatus::Done) {
                    let result = store.update_task(UpdateTask {
                        id: task_id.clone(),
                        body: None,
                        status: Some(to),
                        note: None,
                        actor: "alice".into(),
                    });
                    prop_assert!(
                        result.is_err(),
                        "invariant 3 violated: update_task accepted target {:?}",
                        to,
                    );
                    // The post-state from a rejected call must equal pre-state.
                    let post = current_state(&store);
                    prop_assert_eq!(post.status, pre.status);
                    prop_assert_eq!(post.assignee, pre.assignee);
                    prop_assert_eq!(post.updated_at, pre.updated_at);
                    continue;
                }
            }
            if matches!(op, Op::Complete { .. }) && pre.status != TaskStatus::InProgress {
                // Invariant 4: complete from any non-InProgress source errors.
                apply(&store, op.clone(), &task_id);
                let post = current_state(&store);
                prop_assert_eq!(post.status, pre.status);
                prop_assert_eq!(post.updated_at, pre.updated_at);
                continue;
            }

            // Normal apply path.
            apply(&store, op, &task_id);
            let post = current_state(&store);

            // Invariant 1: terminal absorption.
            if let Some(t) = once_terminal {
                prop_assert_eq!(
                    post.status,
                    t,
                    "invariant 1 violated: status changed after entering terminal {:?}",
                    t,
                );
                prop_assert_eq!(
                    post.completed_at, frozen_completed_at,
                    "invariant 1 violated: completed_at changed after terminal",
                );
            } else if is_terminal(post.status) {
                once_terminal = Some(post.status);
                frozen_completed_at = post.completed_at;
            }

            // Invariant 2: assignee/status coupling.
            if let Some(reason) = check_assignee_status_coupling(post.status, &post.assignee) {
                prop_assert!(false, "invariant 2 violated: {}", reason);
            }

            // Invariant 5: updated_at monotonic.
            prop_assert!(
                post.updated_at >= last_updated_at,
                "invariant 5 violated: updated_at moved backwards ({} -> {})",
                last_updated_at,
                post.updated_at,
            );
            last_updated_at = post.updated_at;
        }
    }
}
