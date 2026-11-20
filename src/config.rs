//! `polarwave.toml`, at the root of the project being developed. Every field is optional.

use anyhow::Context;
use chrono::NaiveTime;
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const FILE: &str = "polarwave.toml";

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    // --- the project
    /// Branch agents start from and work is merged into.
    pub base_branch: String,
    /// Milestone the planner proposes tasks for.
    pub current_milestone: String,
    /// Where task files live (tracked in git).
    pub tasks_dir: String,
    /// Documents that describe milestones, for the planner.
    pub planning_docs: Vec<String>,
    /// Command that must pass before work is handed off and before a PR (empty: agents follow
    /// the project's docs, and polarwave runs no checks itself).
    pub check_command: String,
    /// How agents should produce validation screenshots (empty: a generic hint).
    pub evidence: String,
    /// Extra instructions added to every role prompt.
    pub instructions: String,
    /// Folder with `<role>.md` files overriding the built-in prompts (empty: none).
    pub prompts_dir: String,
    /// Where task worktrees go (empty: `../<project folder>-worktrees`).
    pub worktrees_dir: String,

    // --- agents and limits
    pub claude_bin: String,
    pub max_concurrent: usize,
    pub max_runs_per_day: u32,
    pub max_tokens_per_day: u64,
    /// Stop at this share (percent) of any Claude usage window (5-hour, 7-day), account-wide.
    pub max_usage_percent: f64,
    pub max_turns_per_run: u32,
    pub run_timeout_minutes: u64,
    pub max_fix_attempts: u32,
    pub poll_seconds: u64,
    pub quiet_hours: Vec<String>,

    // --- GitHub
    /// Open pull requests for reviewed tasks (polarwave pushes; agents never do).
    pub pull_requests: bool,
    /// Mirror tasks to GitHub issues; approve, edit, comment and drop tasks there.
    pub issues: bool,
    pub remote: String,
    pub assets_branch: String,
    pub gh_bin: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_branch: "main".into(),
            current_milestone: "M1".into(),
            tasks_dir: "tasks".into(),
            planning_docs: vec!["README.md".into()],
            check_command: String::new(),
            evidence: String::new(),
            instructions: String::new(),
            prompts_dir: String::new(),
            worktrees_dir: String::new(),
            claude_bin: "claude".into(),
            max_concurrent: 2,
            max_runs_per_day: 40,
            max_tokens_per_day: 0,
            max_usage_percent: 70.0,
            max_turns_per_run: 80,
            run_timeout_minutes: 60,
            max_fix_attempts: 3,
            poll_seconds: 120,
            quiet_hours: Vec::new(),
            pull_requests: false,
            issues: false,
            remote: "origin".into(),
            assets_branch: "polarwave-assets".into(),
            gh_bin: "gh".into(),
        }
    }
}

impl Config {
    pub fn load(repo: &Path) -> anyhow::Result<Self> {
        let path = repo.join(FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Folder holding task worktrees.
    pub fn worktrees(&self, repo: &Path) -> PathBuf {
        if self.worktrees_dir.trim().is_empty() {
            let name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("project");
            repo.join("..").join(format!("{name}-worktrees"))
        } else {
            repo.join(&self.worktrees_dir)
        }
    }

    /// Whether `time` falls in one of the configured quiet ranges ("HH:MM-HH:MM", may wrap midnight).
    pub fn is_quiet(&self, time: NaiveTime) -> bool {
        self.quiet_hours.iter().filter_map(|r| parse_range(r)).any(|(start, end)| {
            if start <= end { time >= start && time < end } else { time >= start || time < end }
        })
    }
}

fn parse_range(range: &str) -> Option<(NaiveTime, NaiveTime)> {
    let (a, b) = range.split_once('-')?;
    let parse = |s: &str| NaiveTime::parse_from_str(s.trim(), "%H:%M").ok();
    Some((parse(a)?, parse(b)?))
}

/// The file `polarwave init` writes: every option, commented.
pub const TEMPLATE: &str = include_str!("../polarwave.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn quiet_hours_within_a_day() {
        let c = Config { quiet_hours: vec!["09:00-17:00".into()], ..Config::default() };
        assert!(c.is_quiet(at(9, 0)));
        assert!(c.is_quiet(at(16, 59)));
        assert!(!c.is_quiet(at(17, 0)));
        assert!(!c.is_quiet(at(8, 59)));
    }

    #[test]
    fn quiet_hours_across_midnight() {
        let c = Config { quiet_hours: vec!["22:00-06:00".into()], ..Config::default() };
        assert!(c.is_quiet(at(23, 0)));
        assert!(c.is_quiet(at(5, 59)));
        assert!(!c.is_quiet(at(12, 0)));
    }

    #[test]
    fn template_parses_and_matches_defaults() {
        let c: Config = toml::from_str(TEMPLATE).unwrap();
        assert_eq!(c.base_branch, "main");
        assert_eq!(c.tasks_dir, "tasks");
        assert!((c.max_usage_percent - 70.0).abs() < f64::EPSILON);
        assert!(!c.pull_requests && !c.issues);
    }

    #[test]
    fn worktrees_default_next_to_the_project() {
        let c = Config::default();
        assert_eq!(c.worktrees(Path::new("/p/myapp")), Path::new("/p/myapp/../myapp-worktrees"));
        let c = Config { worktrees_dir: "../wt".into(), ..Config::default() };
        assert_eq!(c.worktrees(Path::new("/p/myapp")), Path::new("/p/myapp/../wt"));
    }
}
