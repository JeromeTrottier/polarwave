//! polarwave: runs Claude Code agents on a project's task queue while the Mac is on.
//! Project-agnostic: everything project-specific comes from `polarwave.toml`.

mod config;
mod issues;
mod launchd;
mod outcome;
mod pr;
mod prompts;
mod runner;
mod schedule;
mod state;
mod task;
mod watch;

use anyhow::{Context, bail};
use chrono::{Local, Utc};
use clap::{Parser, Subcommand};
use config::Config;
use outcome::Outcome;
use schedule::Action;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use task::Status;
use tokio::task::JoinSet;

#[derive(Parser)]
#[command(name = "polarwave", version, about = "Local dispatcher for Claude Code development agents")]
struct Cli {
    /// Repository root (default: the git repository containing the current folder).
    #[arg(long, global = true)]
    repo: Option<PathBuf>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Set up the current project: polarwave.toml, the tasks folder, .gitignore entries.
    Init,
    /// Run the loop (what launchd runs).
    Run,
    /// Run one cycle and wait for the agents it started.
    Once,
    /// Show tasks, running agents and today's usage.
    Status,
    /// Live, readable feed of running agents (text, tool calls, errors, usage, results).
    Watch {
        /// Only follow this task.
        id: Option<u32>,
        /// Replay running agents' feeds from their start instead of from now.
        #[arg(long)]
        from_start: bool,
    },
    /// Approve proposed tasks (proposed → ready).
    Approve { ids: Vec<u32> },
    /// Merge finished work: the task's PR (if any) or the task branch, then mark done.
    Accept { id: u32 },
    /// Open or update the task's pull request now (e.g. after an error).
    Publish { id: u32 },
    /// Send finished work back with a note (awaiting_human → needs_fix).
    Reject { id: u32, note: String },
    /// Stop starting new runs (running agents finish).
    Pause,
    /// Resume after `pause`.
    Resume,
    /// Stop the loop after running agents finish.
    Stop,
    /// Start at login with launchd.
    Install,
    /// Remove the launchd agent.
    Uninstall,
}

fn log(msg: &str) {
    println!("{} {msg}", Local::now().format("%Y-%m-%d %H:%M:%S"));
}

fn find_repo(given: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let start = match given {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let root =
        runner::git(&start, &["rev-parse", "--show-toplevel"]).context("not inside a git repository")?;
    // The main checkout, not a task worktree.
    let common = runner::git(&start, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
    let common = PathBuf::from(common);
    Ok(if common.ends_with(".git") {
        common.parent().map(Path::to_path_buf).unwrap_or(PathBuf::from(root))
    } else {
        PathBuf::from(root)
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let repo = find_repo(cli.repo)?;
    match cli.command {
        Cmd::Run => run_loop(&repo, false).await,
        Cmd::Once => run_loop(&repo, true).await,
        Cmd::Status => status(&repo),
        Cmd::Watch { id, from_start } => watch::run(&repo, id, from_start),
        Cmd::Approve { ids } => approve(&repo, &ids),
        Cmd::Accept { id } => accept(&repo, id),
        Cmd::Publish { id } => {
            let config = Config::load(&repo)?;
            let mut t = task::find(&repo, id)?;
            if t.meta.status != Status::AwaitingHuman {
                bail!("#{id} is {}, not awaiting_human", t.meta.status.as_str());
            }
            let url = pr::publish(&repo, &config, &mut t, &pr::Review::default())?;
            t.log(&format!("pull request: {url}"));
            t.save()?;
            println!("#{id}: {url}");
            Ok(())
        }
        Cmd::Reject { id, note } => reject(&repo, id, &note),
        Cmd::Pause => state::set_flag(&repo, "PAUSE", true),
        Cmd::Resume => {
            state::set_flag(&repo, "PAUSE", false)?;
            let mut s = state::load_state(&repo);
            s.paused_until = None;
            state::save_state(&repo, &s)
        }
        Cmd::Stop => state::set_flag(&repo, "STOP", true),
        Cmd::Init => init(&repo),
        Cmd::Install => {
            let path = launchd::install(&repo)?;
            println!("installed {}; polarwave now starts at login", path.display());
            Ok(())
        }
        Cmd::Uninstall => launchd::uninstall(&repo),
    }
}

// ---- the loop ----------------------------------------------------------------

struct Lock(PathBuf);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn acquire_lock(repo: &Path) -> anyhow::Result<Lock> {
    let path = state::dir(repo).join("polarwave.pid");
    if let Ok(pid) = std::fs::read_to_string(&path) {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if alive {
            bail!("polarwave is already running (pid {})", pid.trim());
        }
    }
    std::fs::create_dir_all(state::dir(repo))?;
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(Lock(path))
}

type Finished = (Action, Outcome, PathBuf);

async fn run_loop(repo: &Path, once: bool) -> anyhow::Result<()> {
    let _lock = acquire_lock(repo)?;
    state::set_flag(repo, "STOP", false)?;
    // Agents of a previous polarwave process aren't tracked by this one; clear stale entries.
    let mut fresh = state::load_state(repo);
    fresh.running.clear();
    state::save_state(repo, &fresh)?;
    log(&format!("polarwave started for {}", repo.display()));
    let mut running: JoinSet<Finished> = JoinSet::new();
    let mut busy: HashSet<u32> = HashSet::new();
    let mut planner_running = false;
    let mut runs: Vec<state::Running> = Vec::new();

    loop {
        let config = Config::load(repo)?;

        // Reap finished agents.
        while let Some(done) = running.try_join_next() {
            match done {
                Ok((action, outcome, log_path)) => {
                    if let Some(id) = action.task_id() {
                        busy.remove(&id);
                    } else {
                        planner_running = false;
                    }
                    runs.retain(|r| r.role != action.role() || r.target != target_of(&action));
                    if let Err(e) = apply_outcome(repo, &config, &action, outcome, &log_path) {
                        log(&format!("error applying outcome of {action:?}: {e:#}"));
                    }
                }
                Err(e) => log(&format!("agent task panicked: {e}")),
            }
        }

        let stopping = state::stop_requested(repo);
        if stopping && running.is_empty() {
            log("stop requested; exiting");
            break;
        }

        let started = if stopping {
            0
        } else {
            start_work(repo, &config, &mut running, &mut busy, &mut planner_running, &mut runs)?
        };

        let mut s = state::load_state(repo);
        s.running = runs.clone();
        s.last_cycle = Some(Utc::now());
        state::save_state(repo, &s)?;

        if once {
            if started == 0 && running.is_empty() {
                break;
            }
            if let Some(done) = running.join_next().await {
                // Put it back through the reaping path on the next iteration.
                let (action, outcome, path) = done?;
                if let Some(id) = action.task_id() {
                    busy.remove(&id);
                } else {
                    planner_running = false;
                }
                runs.retain(|r| r.role != action.role() || r.target != target_of(&action));
                apply_outcome(repo, &config, &action, outcome, &path)?;
            }
            if running.is_empty() {
                break;
            }
            continue;
        }

        tokio::time::sleep(Duration::from_secs(config.poll_seconds.max(10))).await;
    }
    let mut s = state::load_state(repo);
    s.running.clear();
    state::save_state(repo, &s)?;
    Ok(())
}

/// A local time, with the day when it isn't today.
fn when(t: chrono::DateTime<Utc>) -> String {
    let local = t.with_timezone(&Local);
    if local.date_naive() == Local::now().date_naive() {
        format!("at {}", local.format("%H:%M"))
    } else {
        format!("{}", local.format("%a %d %b %H:%M"))
    }
}

fn target_of(action: &Action) -> String {
    match action {
        Action::Plan(m) => format!("plan {m}"),
        other => format!("#{}", other.task_id().unwrap_or_default()),
    }
}

/// Why no new run may start right now, if any.
fn blocked_reason(repo: &Path, config: &Config) -> Option<String> {
    if state::pause_requested(repo) {
        return Some("paused".into());
    }
    if let Some(until) = state::load_state(repo).paused_until
        && until > Utc::now()
    {
        return Some(format!("usage limit or cap; resuming {}", when(until)));
    }
    let limits = state::load_limits(repo);
    if let Some(w) = outcome::over_cap(&limits.windows, config.max_usage_percent, Utc::now()) {
        return Some(format!(
            "{} usage at {:.0}% (cap {:.0}%); resuming {}",
            w.name.replace('_', "-"),
            w.utilization * 100.0,
            config.max_usage_percent,
            when(w.resets_at)
        ));
    }
    if config.is_quiet(Local::now().time()) {
        return Some("quiet hours".into());
    }
    let today = state::load_usage(repo, &state::today());
    if today.runs >= config.max_runs_per_day {
        return Some(format!("daily run budget reached ({})", config.max_runs_per_day));
    }
    if config.max_tokens_per_day > 0 && today.tokens() >= config.max_tokens_per_day {
        return Some(format!("daily token budget reached ({})", config.max_tokens_per_day));
    }
    None
}

fn start_work(
    repo: &Path,
    config: &Config,
    running: &mut JoinSet<Finished>,
    busy: &mut HashSet<u32>,
    planner_running: &mut bool,
    runs: &mut Vec<state::Running>,
) -> anyhow::Result<usize> {
    let (mut tasks, invalid) = task::load_checked(repo)?;
    report_invalid(&invalid);
    if config.issues
        && let Err(e) = sync_issues(repo, config, &mut tasks)
    {
        log(&format!("issue sync failed: {e:#}"));
    }
    if config.pull_requests {
        sync_prs(repo, config, &mut tasks)?;
    }

    for id in schedule::exhausted(&tasks, config.max_fix_attempts) {
        if let Some(t) = tasks.iter_mut().find(|t| t.meta.id == id) {
            t.meta.status = Status::Blocked;
            t.log(&format!("blocked: {} fix attempts used", config.max_fix_attempts));
            t.save()?;
            runner::notify(&format!("Task #{id} blocked after {} fix attempts", config.max_fix_attempts));
        }
    }

    if let Some(reason) = blocked_reason(repo, config) {
        log(&format!("not starting new runs: {reason}"));
        return Ok(0);
    }

    let slots = config.max_concurrent.saturating_sub(running.len());
    let actions = schedule::next_actions(&schedule::Inputs {
        tasks: &tasks,
        busy,
        planner_running: *planner_running,
        slots,
        milestone: &config.current_milestone,
        max_fix_attempts: config.max_fix_attempts,
    });

    let mut started = 0;
    for action in actions {
        let (cwd, task_path) = match &action {
            Action::Plan(_) => (repo.to_path_buf(), None),
            other => {
                let id = other.task_id().expect("task action");
                let t = tasks.iter_mut().find(|t| t.meta.id == id).expect("task exists");
                let cwd = match runner::ensure_worktree(repo, config, t) {
                    Ok(p) => p,
                    Err(e) => {
                        t.meta.status = Status::Blocked;
                        t.log(&format!("blocked: could not prepare worktree: {e:#}"));
                        t.save()?;
                        runner::notify(&format!("Task #{id} blocked: worktree error"));
                        continue;
                    }
                };
                match other {
                    Action::Implement(_) => t.meta.status = Status::InProgress,
                    Action::Fix(_) => {
                        t.meta.status = Status::InProgress;
                        t.meta.attempts += 1;
                    }
                    _ => {}
                }
                t.log(&format!("{} started", other.role()));
                t.save()?;
                busy.insert(id);
                (cwd, Some(t.path.clone()))
            }
        };
        if matches!(action, Action::Plan(_)) {
            *planner_running = true;
        }
        log(&format!("starting {} for {}", action.role(), target_of(&action)));
        let log_path = state::run_log_path(repo, &state::run_target(action.task_id()), action.role());
        runs.push(state::Running {
            role: action.role().into(),
            target: target_of(&action),
            started_at: Utc::now(),
            stream: Some(state::stream_path(&log_path)),
        });
        let (repo, config, action2) = (repo.to_path_buf(), config.clone(), action.clone());
        running.spawn(async move {
            let (outcome, path) =
                runner::run_agent(&repo, &config, &action2, &cwd, task_path.as_deref(), log_path).await;
            (action2, outcome, path)
        });
        started += 1;
    }
    Ok(started)
}

/// Update the task (and loop state) from a finished run.
fn apply_outcome(
    repo: &Path,
    config: &Config,
    action: &Action,
    outcome: Outcome,
    log_path: &Path,
) -> anyhow::Result<()> {
    let run_log = log_path.strip_prefix(repo).unwrap_or(log_path).display().to_string();

    if let Outcome::UsageLimit { resume_at, reason } = &outcome {
        let mut s = state::load_state(repo);
        s.paused_until = Some(s.paused_until.map_or(*resume_at, |p| p.max(*resume_at)));
        state::save_state(repo, &s)?;
        runner::notify(&format!("{reason}; pausing, resuming {}", when(*resume_at)));
        if let Some(id) = action.task_id() {
            let mut t = task::find(repo, id)?;
            match action {
                Action::Implement(_) => t.meta.status = Status::Ready,
                Action::Fix(_) => {
                    t.meta.status = Status::NeedsFix;
                    t.meta.attempts = t.meta.attempts.saturating_sub(1);
                }
                _ => {}
            }
            t.log(&format!("{} stopped: {reason}; will retry", action.role()));
            t.save()?;
        }
        return Ok(());
    }

    let usage = match &outcome {
        Outcome::Reported { usage, .. } | Outcome::NoReport { usage, .. } | Outcome::Failed { usage, .. } => {
            usage.clone()
        }
        Outcome::UsageLimit { .. } => unreachable!(),
    };
    let today = state::record_usage(repo, &usage)?;
    log(&format!(
        "{} for {} finished ({} tokens; today {} runs, {} tokens)",
        action.role(),
        target_of(action),
        usage.tokens(),
        today.runs,
        today.tokens()
    ));

    let Some(id) = action.task_id() else {
        let proposed = task::load_all(repo)?.iter().filter(|t| t.meta.status == Status::Proposed).count();
        match outcome {
            Outcome::Reported { reported, .. } => {
                runner::notify(&format!("Planner: {} ({proposed} tasks to approve)", reported.summary))
            }
            _ => runner::notify(&format!(
                "Planner finished without a report; {proposed} tasks proposed. Log: {run_log}"
            )),
        }
        return Ok(());
    };

    let mut t = task::find(repo, id)?;
    let allowed: &[Status] = match action {
        Action::Review(_) => &[Status::AwaitingHuman, Status::NeedsFix, Status::Blocked],
        _ => &[Status::AgentReview, Status::Blocked],
    };
    let mut review = None;
    match outcome {
        Outcome::Reported { reported, .. } => {
            review =
                Some(pr::Review { summary: reported.summary.clone(), findings: reported.findings.clone() });
            let wanted =
                serde_yaml::from_str::<Status>(&reported.status).ok().filter(|s| allowed.contains(s));
            let status = wanted.unwrap_or(Status::Blocked);
            t.meta.status = status;
            let mut entry = format!("{} → {}: {}", action.role(), status.as_str(), reported.summary);
            if wanted.is_none() {
                entry.push_str(&format!(
                    " (reported status `{}` not allowed for {})",
                    reported.status,
                    action.role()
                ));
            }
            for f in &reported.findings {
                entry.push_str(&format!("\n  - {f}"));
            }
            entry.push_str(&format!(" [{run_log}]"));
            t.log(&entry);
        }
        Outcome::NoReport { message, .. } => {
            t.meta.status = Status::Blocked;
            t.log(&format!(
                "{} finished without a POLARWAVE_RESULT line: {message} [{run_log}]",
                action.role()
            ));
        }
        Outcome::Failed { message, .. } => {
            t.meta.status = Status::Blocked;
            t.log(&format!("{} failed: {message} [{run_log}]", action.role()));
        }
        Outcome::UsageLimit { .. } => unreachable!(),
    }
    t.save()?;
    if t.meta.status == Status::AwaitingHuman && config.pull_requests {
        publish_or_report(repo, config, &mut t, &review.unwrap_or_default())?;
        return Ok(());
    }
    match t.meta.status {
        Status::AwaitingHuman => {
            runner::notify(&format!("Task #{id} is ready for your review: {}", t.meta.title))
        }
        Status::Blocked => runner::notify(&format!("Task #{id} is blocked: {}", t.meta.title)),
        _ => {}
    }
    Ok(())
}

/// Log unreadable task files every cycle; notify once per file version.
fn report_invalid(invalid: &[task::Invalid]) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    for bad in invalid {
        let name = bad.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        log(&format!("skipping unreadable task file {name}: {}", bad.error.replace('\n', " ")));
        let mtime = std::fs::metadata(&bad.path).and_then(|m| m.modified()).ok();
        let key = format!("{}@{mtime:?}", bad.path.display());
        let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
        if !seen.contains(&key) {
            seen.push(key);
            runner::notify(&format!("Task file {name} can't be read; it's skipped until fixed"));
        }
    }
}

// ---- pull requests ----------------------------------------------------------------------

/// Open or update the PR for a reviewed task; failing checks send it back to the fixer.
fn publish_or_report(
    repo: &Path,
    config: &Config,
    t: &mut task::Task,
    review: &pr::Review,
) -> anyhow::Result<()> {
    let id = t.meta.id;
    match pr::publish(repo, config, t, review) {
        Ok(url) => {
            t.log(&format!("pull request ready: {url}"));
            t.save()?;
            runner::notify(&format!("Task #{id} has a pull request to review: {}", t.meta.title));
        }
        Err(e) if e.to_string().contains("not publishing") => {
            t.meta.status = Status::NeedsFix;
            t.log(&format!("checks failed before opening the PR: {e:#}"));
            t.save()?;
        }
        Err(e) => {
            t.log(&format!("could not open the pull request: {e:#} (retry: polarwave publish {id})"));
            t.save()?;
            runner::notify(&format!("Task #{id}: could not open its pull request"));
        }
    }
    Ok(())
}

static OWNER: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

fn owner(repo: &Path, config: &Config) -> Option<&'static str> {
    OWNER
        .get_or_init(|| match pr::owner_login(repo, config) {
            Ok(login) => Some(login),
            Err(e) => {
                log(&format!("cannot read the GitHub login (is `gh auth login` done?): {e:#}"));
                None
            }
        })
        .as_deref()
}

/// Mirror tasks to GitHub issues and apply your actions there.
fn sync_issues(repo: &Path, config: &Config, tasks: &mut [task::Task]) -> anyhow::Result<()> {
    use std::collections::HashMap;
    static SETUP: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let Some(owner) = owner(repo, config) else { return Ok(()) };
    let slug = issues::slug(repo, config)?;

    let mut milestones: Vec<String> = tasks.iter().map(|t| t.meta.milestone.clone()).collect();
    milestones.sort();
    milestones.dedup();
    for m in milestones {
        let mut done = SETUP.lock().unwrap_or_else(|p| p.into_inner());
        if !done.contains(&m) {
            issues::ensure_setup(repo, config, &slug, &m)?;
            done.push(m);
        }
    }

    let snaps: HashMap<u64, issues::Snapshot> =
        issues::list(repo, config, &slug)?.into_iter().map(|s| (s.number, s)).collect();
    let mut issue_of: HashMap<u32, u64> =
        tasks.iter().filter_map(|t| t.meta.issue.map(|n| (t.meta.id, n))).collect();

    for t in tasks.iter_mut() {
        if t.meta.issue.is_some() || matches!(t.meta.status, Status::Done | Status::Cancelled) {
            continue;
        }
        let body = issues::issue_body(t, &issue_of);
        let number = issues::create(repo, config, &slug, t, &body)?;
        t.meta.issue = Some(number);
        t.meta.issue_body_hash = Some(issues::body_hash(&body));
        t.meta.issue_status = Some(t.meta.status);
        t.meta.issue_synced_at = Some(Utc::now());
        t.log(&format!("issue #{number} opened"));
        t.save()?;
        issue_of.insert(t.meta.id, number);
        log(&format!("#{}: opened issue #{number}", t.meta.id));
    }

    for t in tasks.iter_mut() {
        let Some(number) = t.meta.issue else { continue };
        let Some(snap) = snaps.get(&number) else { continue };
        let mut changed = false;

        if t.meta.issue_synced_at.is_none_or(|s| snap.updated_at > s) {
            let events = issues::events(repo, config, &slug, number)?;
            let comments = issues::comments(repo, config, &slug, number)?;
            let d = issues::decide(t, snap, &events, &comments, owner);
            if let Some(body) = &d.edited {
                t.body = issues::apply_edited_sections(&t.body, body);
                t.meta.issue_body_hash = Some(issues::body_hash(body));
                t.log("goal / criteria / notes updated from your edit of the issue");
            }
            if d.late_edit {
                t.meta.issue_body_hash = Some(issues::body_hash(&snap.body));
                issues::comment(
                    repo,
                    config,
                    &slug,
                    number,
                    "Work on this task has started, so edits to the issue text aren't applied. Add a comment instead; agents read comments as notes.",
                )?;
            }
            if d.approve {
                t.meta.status = Status::Ready;
                t.log("approved on GitHub");
            }
            for note in &d.notes {
                t.log(&format!("note from you on issue #{number}: {}", note.replace('\n', " ")));
            }
            if !d.notes.is_empty() && t.meta.status == Status::Blocked {
                t.meta.status = if t.meta.branch.is_some() { Status::NeedsFix } else { Status::Ready };
                t.meta.attempts = 0;
                t.log("unblocked by your answer on the issue");
            }
            if d.cancel {
                t.meta.status = Status::Cancelled;
                t.log("cancelled: you closed the issue");
            }
            t.meta.issue_synced_at = Some(Utc::now());
            changed = true;
        }

        let label = issues::status_label(t.meta.status);
        if t.meta.issue_status != Some(t.meta.status) || !snap.labels.iter().any(|l| l == label) {
            issues::set_status(repo, config, &slug, number, &snap.labels, t.meta.status)?;
            if t.meta.issue_status != Some(t.meta.status) {
                match t.meta.status {
                    Status::Blocked => {
                        let last = t
                            .body
                            .lines()
                            .rev()
                            .find(|l| l.starts_with("- "))
                            .unwrap_or("")
                            .trim_start_matches("- ");
                        issues::comment(
                            repo,
                            config,
                            &slug,
                            number,
                            &format!("**Blocked.** {last}\n\nReply here to unblock it."),
                        )?;
                    }
                    Status::AwaitingHuman => {
                        if let Some(pr_url) = &t.meta.pr {
                            issues::comment(
                                repo,
                                config,
                                &slug,
                                number,
                                &format!("Pull request ready for your review: {pr_url}"),
                            )?;
                        }
                    }
                    Status::Done if snap.open => issues::close(repo, config, &slug, number, "completed")?,
                    Status::Cancelled if snap.open => {
                        issues::close(repo, config, &slug, number, "not planned")?
                    }
                    _ => {}
                }
            }
            t.meta.issue_status = Some(t.meta.status);
            t.meta.issue_synced_at = Some(Utc::now());
            changed = true;
        }
        if changed {
            t.save()?;
        }
    }
    Ok(())
}

/// Follow open PRs: merged → done, closed → blocked, owner feedback → needs_fix.
fn sync_prs(repo: &Path, config: &Config, tasks: &mut [task::Task]) -> anyhow::Result<()> {
    if !tasks.iter().any(|t| t.meta.pr.is_some() && t.meta.status == Status::AwaitingHuman) {
        return Ok(());
    }
    let Some(owner) = owner(repo, config) else { return Ok(()) };
    for t in tasks.iter_mut().filter(|t| t.meta.pr.is_some() && t.meta.status == Status::AwaitingHuman) {
        match pr::check(repo, config, t, owner) {
            Ok(pr::Change::None) => {}
            Ok(pr::Change::Merged) => finish_merged(repo, config, t)?,
            Ok(pr::Change::Closed) => {
                t.meta.status = Status::Blocked;
                t.log("pull request was closed without merging");
                t.save()?;
                runner::notify(&format!("Task #{}: pull request closed", t.meta.id));
            }
            Ok(pr::Change::Feedback(items)) => {
                t.meta.status = Status::NeedsFix;
                t.meta.attempts = 0;
                t.meta.pr_synced_at = items.last().map(|f| f.at);
                let mut entry = "sent back by you on the pull request:".to_string();
                for f in &items {
                    entry.push_str(&format!("\n  - [{}] {}", f.kind, f.body.replace('\n', " ")));
                }
                t.log(&entry);
                t.save()?;
                log(&format!("#{}: {} PR comment(s) → needs_fix", t.meta.id, items.len()));
            }
            Err(e) => log(&format!("#{}: could not read its pull request: {e:#}", t.meta.id)),
        }
    }
    Ok(())
}

/// A task's PR was merged: mark done, remove its worktree, fast-forward the main checkout.
fn finish_merged(repo: &Path, config: &Config, t: &mut task::Task) -> anyhow::Result<()> {
    t.meta.status = Status::Done;
    t.log("pull request merged");
    t.save()?;
    let worktree = runner::worktree_path(repo, config, t);
    if worktree.exists() {
        let _ = runner::git(repo, &["worktree", "remove", "--force", &worktree.to_string_lossy()]);
    }
    if runner::git(repo, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().as_deref()
        == Some(config.base_branch.as_str())
        && let Err(e) = runner::git(repo, &["pull", "-q", "--ff-only", &config.remote, &config.base_branch])
    {
        log(&format!("could not fast-forward {}: {e:#}", config.base_branch));
    }
    runner::notify(&format!("Task #{} merged: {}", t.meta.id, t.meta.title));
    Ok(())
}

// ---- commands ----------------------------------------------------------------------

fn init(repo: &Path) -> anyhow::Result<()> {
    let config_path = repo.join(config::FILE);
    if config_path.exists() {
        println!("{} already exists; leaving it", config::FILE);
    } else {
        std::fs::write(&config_path, config::TEMPLATE)?;
        println!("wrote {}", config::FILE);
    }
    let config = Config::load(repo)?;
    let tasks = repo.join(&config.tasks_dir);
    std::fs::create_dir_all(&tasks)?;
    let template = tasks.join("_template.md");
    if !template.exists() {
        std::fs::write(&template, TASK_TEMPLATE)?;
        println!("wrote {}", template.strip_prefix(repo).unwrap_or(&template).display());
    }
    let ignore = repo.join(".gitignore");
    let current = std::fs::read_to_string(&ignore).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == ".polarwave/") {
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&ignore, format!("{current}{sep}.polarwave/\n"))?;
        println!("added .polarwave/ to .gitignore");
    }
    println!(
        "\nNext: review {}, describe milestones in your planning docs, then `polarwave install`.",
        config::FILE
    );
    Ok(())
}

const TASK_TEMPLATE: &str = "---\nid: 0\ntitle: \"Short imperative title (always quoted)\"\nmilestone: M1\nstatus: proposed\ndepends_on: []\nsize: M\narea: []\nattempts: 0\nbranch: null\n---\n\n## Goal\nWhat this task delivers and why.\n\n## Acceptance criteria\n- [ ] A checkable behaviour, with the test that proves it.\n\n## Notes\nConstraints, hints. Out of scope: …\n\n## Log\n";

fn status(repo: &Path) -> anyhow::Result<()> {
    let config = Config::load(repo)?;
    let (tasks, invalid) = task::load_checked(repo)?;
    let s = state::load_state(repo);
    let today = state::load_usage(repo, &state::today());
    println!(
        "milestone {}  ·  today: {} runs, {} tokens, ${:.2}",
        config.current_milestone,
        today.runs,
        today.tokens(),
        today.cost_usd
    );
    if let Some(reason) = blocked_reason(repo, &config) {
        println!("new runs: on hold ({reason})");
    }
    let limits = state::load_limits(repo);
    if let Some(seen) = limits.seen_at {
        let windows: Vec<String> = limits
            .windows
            .iter()
            .map(|w| {
                format!(
                    "{} {:.0}% (resets {})",
                    w.name.replace('_', "-"),
                    w.utilization * 100.0,
                    when(w.resets_at)
                )
            })
            .collect();
        println!(
            "Claude usage (cap {:.0}%): {}  ·  as of {}",
            config.max_usage_percent,
            windows.join(", "),
            seen.with_timezone(&Local).format("%H:%M")
        );
    }
    if let Some(at) = s.last_cycle {
        println!("last cycle: {}", at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"));
    }
    for r in &s.running {
        println!(
            "running: {} {} since {}",
            r.role,
            r.target,
            r.started_at.with_timezone(&Local).format("%H:%M")
        );
    }
    println!();
    for t in &tasks {
        let deps = if t.meta.depends_on.is_empty() {
            String::new()
        } else {
            format!("  after {:?}", t.meta.depends_on)
        };
        println!(
            "#{:<4} {:<15} {:<4} {}{deps}",
            t.meta.id,
            t.meta.status.as_str(),
            t.meta.milestone,
            t.meta.title
        );
    }
    if tasks.is_empty() {
        println!("(no tasks yet; the planner proposes tasks for {})", config.current_milestone);
    }
    for bad in &invalid {
        println!("unreadable: {} — {}", bad.path.display(), bad.error.replace('\n', " "));
    }
    Ok(())
}

fn approve(repo: &Path, ids: &[u32]) -> anyhow::Result<()> {
    for &id in ids {
        let mut t = task::find(repo, id)?;
        if t.meta.status != Status::Proposed {
            println!("#{id} is {}, not proposed; skipped", t.meta.status.as_str());
            continue;
        }
        t.meta.status = Status::Ready;
        t.log("approved");
        t.save()?;
        println!("#{id} ready");
    }
    Ok(())
}

fn reject(repo: &Path, id: u32, note: &str) -> anyhow::Result<()> {
    let mut t = task::find(repo, id)?;
    if !matches!(t.meta.status, Status::AwaitingHuman | Status::Blocked) {
        bail!("#{id} is {}; only awaiting_human or blocked tasks can be sent back", t.meta.status.as_str());
    }
    t.meta.status = Status::NeedsFix;
    t.meta.attempts = 0;
    t.log(&format!("sent back by you: {note}"));
    t.save()?;
    println!("#{id} needs_fix");
    Ok(())
}

fn accept(repo: &Path, id: u32) -> anyhow::Result<()> {
    let config = Config::load(repo)?;
    let mut t = task::find(repo, id)?;
    if t.meta.status != Status::AwaitingHuman {
        bail!("#{id} is {}, not awaiting_human", t.meta.status.as_str());
    }
    if let Some(url) = t.meta.pr.clone() {
        pr::merge(repo, &config, &url)?;
        finish_merged(repo, &config, &mut t)?;
        println!("#{id}: merged {url}");
        return Ok(());
    }
    let branch = t.meta.branch.clone().context("task has no branch")?;
    let current = runner::git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current != config.base_branch {
        bail!("the main checkout is on `{current}`; switch to `{}` first", config.base_branch);
    }
    let worktree = runner::worktree_path(repo, &config, &t);
    if worktree.exists() && !config.check_command.trim().is_empty() {
        println!("running `{}` in {}", config.check_command, worktree.display());
        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(&config.check_command)
            .current_dir(&worktree)
            .status()?
            .success();
        if !ok {
            bail!("checks failed on {branch}; not merging");
        }
    }
    runner::git(repo, &["merge", "--no-ff", &branch, "-m", &format!("Merge task #{id}: {}", t.meta.title)])?;
    if worktree.exists() {
        runner::git(repo, &["worktree", "remove", &worktree.to_string_lossy()])?;
    }
    t.meta.status = Status::Done;
    t.log(&format!("accepted and merged into {}", config.base_branch));
    t.save()?;
    println!("#{id} merged into {} and marked done", config.base_branch);
    Ok(())
}
