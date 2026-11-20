//! Deciding what to start next. Pure logic, fully unit-tested.

use crate::task::{Status, Task};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Review(u32),
    Fix(u32),
    Implement(u32),
    Plan(String),
}

impl Action {
    /// The task this action occupies (planner runs occupy no task).
    pub fn task_id(&self) -> Option<u32> {
        match self {
            Action::Review(id) | Action::Fix(id) | Action::Implement(id) => Some(*id),
            Action::Plan(_) => None,
        }
    }

    pub fn role(&self) -> &'static str {
        match self {
            Action::Review(_) => "reviewer",
            Action::Fix(_) => "fixer",
            Action::Implement(_) => "implementer",
            Action::Plan(_) => "planner",
        }
    }
}

pub struct Inputs<'a> {
    pub tasks: &'a [Task],
    /// Task ids with an agent currently running.
    pub busy: &'a HashSet<u32>,
    pub planner_running: bool,
    /// Free agent slots right now.
    pub slots: usize,
    pub milestone: &'a str,
    pub max_fix_attempts: u32,
}

/// Choose up to `slots` actions, by priority: review, fix, implement, then plan.
pub fn next_actions(input: &Inputs) -> Vec<Action> {
    let status: HashMap<u32, Status> = input.tasks.iter().map(|t| (t.meta.id, t.meta.status)).collect();
    let free = |t: &&Task| !input.busy.contains(&t.meta.id);
    let deps_done = |t: &Task| t.meta.depends_on.iter().all(|d| status.get(d) == Some(&Status::Done));

    let mut actions = Vec::new();
    actions.extend(
        input
            .tasks
            .iter()
            .filter(free)
            .filter(|t| t.meta.status == Status::AgentReview)
            .map(|t| Action::Review(t.meta.id)),
    );
    actions.extend(
        input
            .tasks
            .iter()
            .filter(free)
            .filter(|t| t.meta.status == Status::NeedsFix && t.meta.attempts < input.max_fix_attempts)
            .map(|t| Action::Fix(t.meta.id)),
    );
    actions.extend(
        input
            .tasks
            .iter()
            .filter(free)
            .filter(|t| t.meta.status == Status::Ready && deps_done(t))
            .map(|t| Action::Implement(t.meta.id)),
    );

    let milestone_has_open_work = input.tasks.iter().any(|t| {
        t.meta.milestone == input.milestone
            && matches!(
                t.meta.status,
                Status::Proposed
                    | Status::Ready
                    | Status::InProgress
                    | Status::AgentReview
                    | Status::NeedsFix
            )
    });
    let milestone_has_any = input.tasks.iter().any(|t| t.meta.milestone == input.milestone);
    if !input.planner_running && !milestone_has_open_work && !milestone_has_any {
        actions.push(Action::Plan(input.milestone.to_string()));
    }

    actions.truncate(input.slots);
    actions
}

/// Tasks that reached the fix-attempt limit and should be blocked.
pub fn exhausted(tasks: &[Task], max_fix_attempts: u32) -> Vec<u32> {
    tasks
        .iter()
        .filter(|t| t.meta.status == Status::NeedsFix && t.meta.attempts >= max_fix_attempts)
        .map(|t| t.meta.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Meta;
    use std::path::PathBuf;

    fn task(id: u32, status: Status, deps: &[u32]) -> Task {
        Task {
            meta: Meta {
                id,
                title: format!("Task {id}"),
                milestone: "M1".into(),
                status,
                depends_on: deps.to_vec(),
                size: None,
                area: vec![],
                attempts: 0,
                branch: None,
                pr: None,
                issue: None,
                issue_synced_at: None,
                issue_body_hash: None,
                issue_status: None,
                pr_synced_at: None,
            },
            body: String::new(),
            path: PathBuf::from(format!("{id}.md")),
        }
    }

    fn run(tasks: &[Task], busy: &[u32], slots: usize) -> Vec<Action> {
        let busy: HashSet<u32> = busy.iter().copied().collect();
        next_actions(&Inputs {
            tasks,
            busy: &busy,
            planner_running: false,
            slots,
            milestone: "M1",
            max_fix_attempts: 3,
        })
    }

    #[test]
    fn reviews_come_before_fixes_before_new_work() {
        let tasks =
            [task(1, Status::Ready, &[]), task(2, Status::NeedsFix, &[]), task(3, Status::AgentReview, &[])];
        assert_eq!(run(&tasks, &[], 3), vec![Action::Review(3), Action::Fix(2), Action::Implement(1)]);
    }

    #[test]
    fn respects_free_slots() {
        let tasks = [task(1, Status::Ready, &[]), task(2, Status::Ready, &[])];
        assert_eq!(run(&tasks, &[], 1), vec![Action::Implement(1)]);
        assert!(run(&tasks, &[], 0).is_empty());
    }

    #[test]
    fn waits_for_dependencies() {
        let tasks = [task(1, Status::AwaitingHuman, &[]), task(2, Status::Ready, &[1])];
        assert!(run(&tasks, &[], 2).is_empty());
        let tasks = [task(1, Status::Done, &[]), task(2, Status::Ready, &[1])];
        assert_eq!(run(&tasks, &[], 2), vec![Action::Implement(2)]);
    }

    #[test]
    fn skips_busy_tasks() {
        let tasks = [task(1, Status::AgentReview, &[])];
        assert!(run(&tasks, &[1], 2).is_empty());
    }

    #[test]
    fn plans_only_when_the_milestone_has_no_tasks() {
        assert_eq!(run(&[], &[], 2), vec![Action::Plan("M1".into())]);
        let proposed = [task(1, Status::Proposed, &[])];
        assert!(run(&proposed, &[], 2).is_empty(), "proposed tasks wait for approval, no re-planning");
    }

    #[test]
    fn exhausted_fixes_are_reported() {
        let mut t = task(4, Status::NeedsFix, &[]);
        t.meta.attempts = 3;
        assert_eq!(exhausted(&[t], 3), vec![4]);
    }
}
