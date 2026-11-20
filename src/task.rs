//! The task queue: one Markdown file per task in the project's `tasks_dir`.

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Proposed,
    Ready,
    InProgress,
    AgentReview,
    NeedsFix,
    AwaitingHuman,
    Done,
    Blocked,
    /// Dropped (e.g. you closed its GitHub issue). Never scheduled.
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Proposed => "proposed",
            Status::Ready => "ready",
            Status::InProgress => "in_progress",
            Status::AgentReview => "agent_review",
            Status::NeedsFix => "needs_fix",
            Status::AwaitingHuman => "awaiting_human",
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    pub id: u32,
    pub title: String,
    pub milestone: String,
    pub status: Status,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub area: Vec<String>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub branch: Option<String>,
    /// Pull request URL, once polarwave has opened one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
    /// Last time polarwave read the PR's feedback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// GitHub issue number mirroring this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
    /// Last time polarwave read the issue's activity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Hash of the issue body polarwave last wrote or accepted (detects your edits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_body_hash: Option<String>,
    /// Status last mirrored to the issue (labels, comments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_status: Option<Status>,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub meta: Meta,
    pub body: String,
    pub path: PathBuf,
}

impl Task {
    pub fn parse(path: &Path, text: &str) -> anyhow::Result<Self> {
        let Some(rest) = text.strip_prefix("---") else { bail!("{}: missing frontmatter", path.display()) };
        let Some((front, body)) = rest.split_once("\n---") else {
            bail!("{}: unterminated frontmatter", path.display())
        };
        let meta: Meta =
            serde_yaml::from_str(front).with_context(|| format!("{}: bad frontmatter", path.display()))?;
        let body = body.strip_prefix('\n').unwrap_or(body).to_string();
        Ok(Self { meta, body, path: path.to_path_buf() })
    }

    pub fn render(&self) -> anyhow::Result<String> {
        let front = serde_yaml::to_string(&self.meta)?;
        Ok(format!("---\n{front}---\n{}", self.body))
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let tmp = self.path.with_extension("md.tmp");
        std::fs::write(&tmp, self.render()?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Append a dated entry to the `## Log` section (created if missing).
    pub fn log(&mut self, entry: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M");
        if !self.body.contains("\n## Log") && !self.body.starts_with("## Log") {
            if !self.body.ends_with('\n') {
                self.body.push('\n');
            }
            self.body.push_str("\n## Log\n");
        }
        if !self.body.ends_with('\n') {
            self.body.push('\n');
        }
        self.body.push_str(&format!("- {stamp} — {}\n", entry.trim()));
    }

    /// Short slug for branch names, from the title.
    pub fn slug(&self) -> String {
        let mut slug = String::new();
        for c in self.meta.title.to_lowercase().chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c);
            } else if !slug.ends_with('-') && !slug.is_empty() {
                slug.push('-');
            }
            if slug.len() >= 40 {
                break;
            }
        }
        slug.trim_end_matches('-').to_string()
    }

    pub fn branch_name(&self) -> String {
        format!("task/{:04}-{}", self.meta.id, self.slug())
    }
}

pub fn tasks_dir(repo: &Path) -> PathBuf {
    let dir = crate::config::Config::load(repo).map(|c| c.tasks_dir).unwrap_or_else(|_| "tasks".into());
    repo.join(dir)
}

/// A task file that couldn't be read; it is skipped until fixed.
#[derive(Clone, Debug, PartialEq)]
pub struct Invalid {
    pub path: PathBuf,
    pub error: String,
}

/// Load every readable task, sorted by id, plus the files that couldn't be read.
/// Files starting with `_` (templates) are skipped. One bad file never stops the others.
pub fn load_checked(repo: &Path) -> anyhow::Result<(Vec<Task>, Vec<Invalid>)> {
    let dir = tasks_dir(repo);
    let (mut tasks, mut invalid) = (Vec::new(), Vec::new());
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok((tasks, invalid)) };
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if path.extension().is_some_and(|e| e == "md") && !name.starts_with('_') {
            match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| Task::parse(&path, &t))
            {
                Ok(task) => tasks.push(task),
                Err(e) => invalid.push(Invalid { path, error: format!("{e:#}") }),
            }
        }
    }
    tasks.sort_by_key(|t| t.meta.id);
    invalid.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((tasks, invalid))
}

/// Load every readable task, sorted by id (unreadable files are skipped).
pub fn load_all(repo: &Path) -> anyhow::Result<Vec<Task>> {
    load_checked(repo).map(|(tasks, _)| tasks)
}

pub fn find(repo: &Path, id: u32) -> anyhow::Result<Task> {
    load_all(repo)?
        .into_iter()
        .find(|t| t.meta.id == id)
        .with_context(|| format!("no task with id {id} in the tasks folder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nid: 7\ntitle: Entity store with optimistic locking\nmilestone: M1\nstatus: ready\ndepends_on: [5, 6]\nsize: M\narea: [server/core]\nattempts: 0\nbranch: null\n---\n\n## Goal\nStore entities.\n";

    #[test]
    fn parses_and_renders_round_trip() {
        let task = Task::parse(Path::new("0007.md"), SAMPLE).unwrap();
        assert_eq!(task.meta.id, 7);
        assert_eq!(task.meta.status, Status::Ready);
        assert_eq!(task.meta.depends_on, vec![5, 6]);
        let again = Task::parse(Path::new("0007.md"), &task.render().unwrap()).unwrap();
        assert_eq!(again.meta, task.meta);
        assert_eq!(again.body, task.body);
    }

    #[test]
    fn branch_name_uses_id_and_slug() {
        let task = Task::parse(Path::new("0007.md"), SAMPLE).unwrap();
        assert_eq!(task.branch_name(), "task/0007-entity-store-with-optimistic-locking");
    }

    #[test]
    fn log_appends_under_a_log_heading() {
        let mut task = Task::parse(Path::new("0007.md"), SAMPLE).unwrap();
        task.log("implementer started");
        task.log("review passed");
        assert_eq!(task.body.matches("## Log").count(), 1);
        assert!(task.body.contains("implementer started"));
        assert!(task.body.trim_end().ends_with("review passed"));
    }

    #[test]
    fn unreadable_files_are_reported_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(tasks.join("0007-x.md"), SAMPLE).unwrap();
        let bad = "---\nid: 8\ntitle: Activity runs: start, complete\nmilestone: M1\nstatus: ready\n---\n";
        std::fs::write(tasks.join("0008-bad.md"), bad).unwrap();
        let (ok, invalid) = load_checked(dir.path()).unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(invalid.len(), 1);
        assert!(invalid[0].path.ends_with("0008-bad.md"));
        assert_eq!(load_all(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn load_all_skips_templates() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = dir.path().join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(tasks.join("0007-x.md"), SAMPLE).unwrap();
        std::fs::write(tasks.join("_template.md"), "not a task").unwrap();
        let all = load_all(dir.path()).unwrap();
        assert_eq!(all.len(), 1);
    }
}
