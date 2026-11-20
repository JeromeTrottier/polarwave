//! polarwave's own files in `.polarwave/`: daily usage, pause-until, running agents, control files.

use crate::outcome::{RateWindow, Usage};
use anyhow::Context;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub fn dir(repo: &Path) -> PathBuf {
    repo.join(".polarwave")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DayUsage {
    pub runs: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl DayUsage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

fn usage_path(repo: &Path, day: &str) -> PathBuf {
    dir(repo).join("usage").join(format!("{day}.json"))
}

pub fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

pub fn load_usage(repo: &Path, day: &str) -> DayUsage {
    std::fs::read_to_string(usage_path(repo, day))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn record_usage(repo: &Path, usage: &Usage) -> anyhow::Result<DayUsage> {
    let day = today();
    let mut total = load_usage(repo, &day);
    total.runs += 1;
    total.input_tokens += usage.input_tokens;
    total.output_tokens += usage.output_tokens;
    total.cost_usd += usage.cost_usd;
    write_json(&usage_path(repo, &day), &total)?;
    Ok(total)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoopState {
    /// No new runs before this time (set when Claude's usage limit is hit).
    pub paused_until: Option<DateTime<Utc>>,
    /// Agents currently running: (role, task id or milestone, started at).
    pub running: Vec<Running>,
    pub last_cycle: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Running {
    pub role: String,
    pub target: String,
    pub started_at: DateTime<Utc>,
    /// Live event stream of the run (`.jsonl`), followed by `polarwave watch`.
    #[serde(default)]
    pub stream: Option<PathBuf>,
}

/// The latest usage windows reported by any run (account-wide utilization).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Limits {
    pub windows: Vec<RateWindow>,
    pub seen_at: Option<DateTime<Utc>>,
}

fn limits_path(repo: &Path) -> PathBuf {
    dir(repo).join("limits.json")
}

pub fn load_limits(repo: &Path) -> Limits {
    std::fs::read_to_string(limits_path(repo))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_limits(repo: &Path, windows: &[RateWindow]) -> anyhow::Result<()> {
    write_json(&limits_path(repo), &Limits { windows: windows.to_vec(), seen_at: Some(Utc::now()) })
}

fn state_path(repo: &Path) -> PathBuf {
    dir(repo).join("state.json")
}

pub fn load_state(repo: &Path) -> LoopState {
    std::fs::read_to_string(state_path(repo))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_state(repo: &Path, state: &LoopState) -> anyhow::Result<()> {
    write_json(&state_path(repo), state)
}

pub fn stop_requested(repo: &Path) -> bool {
    dir(repo).join("STOP").exists()
}

pub fn pause_requested(repo: &Path) -> bool {
    dir(repo).join("PAUSE").exists()
}

pub fn set_flag(repo: &Path, name: &str, on: bool) -> anyhow::Result<()> {
    let path = dir(repo).join(name);
    if on {
        std::fs::create_dir_all(dir(repo))?;
        std::fs::write(&path, Local::now().to_rfc3339())?;
    } else if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Live stream file for a run: same name as its final record, with `.jsonl`.
pub fn stream_path(run_log: &Path) -> PathBuf {
    run_log.with_extension("jsonl")
}

/// Folder name under `.polarwave/runs/` for an action's target.
pub fn run_target(task_id: Option<u32>) -> String {
    task_id.map(|id| format!("{id:04}")).unwrap_or_else(|| "planner".into())
}

pub fn run_log_path(repo: &Path, target: &str, role: &str) -> PathBuf {
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    dir(repo).join("runs").join(target).join(format!("{stamp}-{role}.json"))
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_accumulates_per_day() {
        let repo = tempfile::tempdir().unwrap();
        let run = Usage { input_tokens: 100, output_tokens: 50, cost_usd: 0.01 };
        record_usage(repo.path(), &run).unwrap();
        let total = record_usage(repo.path(), &run).unwrap();
        assert_eq!(total.runs, 2);
        assert_eq!(total.tokens(), 300);
        assert_eq!(load_usage(repo.path(), &today()), total);
    }

    #[test]
    fn flags_toggle() {
        let repo = tempfile::tempdir().unwrap();
        assert!(!pause_requested(repo.path()));
        set_flag(repo.path(), "PAUSE", true).unwrap();
        assert!(pause_requested(repo.path()));
        set_flag(repo.path(), "PAUSE", false).unwrap();
        assert!(!pause_requested(repo.path()));
    }
}
