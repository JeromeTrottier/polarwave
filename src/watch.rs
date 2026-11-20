//! `polarwave watch`: a readable live feed of running agents, from their `.jsonl` streams.

use crate::state;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX: usize = 160;

fn short(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        format!("{}…", one_line.chars().take(max - 1).collect::<String>())
    }
}

/// Shorten paths to be relative to the worktree or repository.
fn rel(path: &str) -> String {
    // …/<project>-worktrees/<task>/<path> → <path>
    if let Some(i) = path.find("-worktrees/") {
        return path[i..].splitn(3, '/').nth(2).unwrap_or(path).to_string();
    }
    path.to_string()
}

/// One line summarising a tool call's input.
fn tool_summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    let detail = match name {
        "Bash" => s("command").to_string(),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => rel(s("file_path")),
        "Grep" => format!("{} {}", s("pattern"), rel(s("path"))),
        "Glob" => s("pattern").to_string(),
        "Task" | "Agent" => format!("{} — {}", s("subagent_type"), s("description")),
        "Skill" => s("skill").to_string(),
        "WebFetch" => s("url").to_string(),
        "WebSearch" => s("query").to_string(),
        "TodoWrite" => "update todo list".to_string(),
        _ => input.to_string(),
    };
    short(&format!("{name} {detail}"), MAX)
}

/// Turn one stream-json event into zero or more readable lines.
pub fn describe(line: &str) -> Vec<String> {
    let Ok(event) = serde_json::from_str::<Value>(line.trim()) else { return Vec::new() };
    let content = || event.pointer("/message/content").and_then(Value::as_array).cloned().unwrap_or_default();
    match event.get("type").and_then(Value::as_str) {
        Some("system") if event.get("subtype").and_then(Value::as_str) == Some("init") => {
            let model = event.get("model").and_then(Value::as_str).unwrap_or("?");
            vec![format!("start   session started ({model})")]
        }
        Some("assistant") => content()
            .iter()
            .filter_map(|c| match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let text = c.get("text").and_then(Value::as_str).unwrap_or_default();
                    (!text.trim().is_empty()).then(|| format!("say     {}", short(text, MAX)))
                }
                Some("tool_use") => {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                    Some(format!("tool    {}", tool_summary(name, c.get("input").unwrap_or(&Value::Null))))
                }
                _ => None,
            })
            .collect(),
        Some("user") => content()
            .iter()
            .filter(|c| {
                c.get("type").and_then(Value::as_str) == Some("tool_result")
                    && c.get("is_error").and_then(Value::as_bool) == Some(true)
            })
            .map(|c| {
                let text = match c.get("content") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::new(),
                };
                format!("error   {}", short(&text, MAX))
            })
            .collect(),
        Some("rate_limit_event") => crate::outcome::rate_windows(line)
            .map(|ws| {
                let parts: Vec<String> = ws
                    .iter()
                    .map(|w| format!("{} {:.0}%", w.name.replace('_', "-"), w.utilization * 100.0))
                    .collect();
                vec![format!("usage   {}", parts.join(", "))]
            })
            .unwrap_or_default(),
        Some("result") => {
            let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("done");
            let turns = event.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
            let result = event.get("result").and_then(Value::as_str).unwrap_or_default();
            let status = crate::outcome::find_marker(result)
                .map(|r| format!(" → {}: {}", r.status, r.summary))
                .unwrap_or_default();
            vec![format!("done    {subtype}, {turns} turns{status}")]
        }
        _ => Vec::new(),
    }
}

/// Reads only what was appended to files since the last call.
#[derive(Default)]
struct Follower {
    offsets: BTreeMap<PathBuf, u64>,
    partial: BTreeMap<PathBuf, String>,
}

impl Follower {
    /// New complete lines appended to `path` since the last call. On first sight of a file,
    /// starts at the beginning when `from_start`, otherwise at its current end.
    fn read_new(&mut self, path: &Path, from_start: bool) -> Vec<String> {
        let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let offset = *self.offsets.entry(path.to_path_buf()).or_insert(if from_start { 0 } else { len });
        if len <= offset {
            return Vec::new();
        }
        let mut buf = String::new();
        if file.seek(SeekFrom::Start(offset)).is_err() || file.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        self.offsets.insert(path.to_path_buf(), len);
        let mut text = self.partial.remove(path).unwrap_or_default();
        text.push_str(&buf);
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        let rest = lines.pop().unwrap_or_default();
        if !rest.is_empty() {
            self.partial.insert(path.to_path_buf(), rest);
        }
        lines
    }
}

fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Strip the date and time polarwave puts at the start of its log lines.
fn without_timestamp(line: &str) -> &str {
    line.splitn(3, ' ').nth(2).unwrap_or(line)
}

/// Follow running agents (optionally only task `only`) until interrupted. With `from_start`,
/// replay each running agent's feed from its beginning.
pub fn run(repo: &Path, only: Option<u32>, from_start: bool) -> anyhow::Result<()> {
    let only_target = only.map(|id| format!("#{id}"));
    let mut follower = Follower::default();
    let mut known: HashSet<String> = HashSet::new();
    let mut streams: BTreeMap<String, PathBuf> = BTreeMap::new();
    let loop_log = state::dir(repo).join("polarwave.log");
    let mut first = true;
    println!("watching {} (Ctrl-C to stop)", repo.display());

    loop {
        let running = state::load_state(repo).running;
        let mut current = HashSet::new();
        for r in &running {
            if only_target.as_ref().is_some_and(|t| t != &r.target) {
                continue;
            }
            let label = format!("[{} {}]", r.target, r.role);
            current.insert(label.clone());
            if known.insert(label.clone()) {
                let since = r.started_at.with_timezone(&chrono::Local).format("%H:%M");
                println!("{} {label} running since {since}", stamp());
                if r.stream.is_none() {
                    println!("{} {label} started before live feeds existed; no feed for this run", stamp());
                }
            }
            if let Some(stream) = &r.stream {
                streams.insert(label.clone(), stream.clone());
                for line in follower.read_new(stream, from_start || !first) {
                    for text in describe(&line) {
                        println!("{} {label} {text}", stamp());
                    }
                }
            }
        }
        for gone in known.difference(&current).cloned().collect::<Vec<_>>() {
            // Print whatever the run wrote after our last read (usually its result).
            if let Some(stream) = streams.remove(&gone) {
                for line in follower.read_new(&stream, true) {
                    for text in describe(&line) {
                        println!("{} {gone} {text}", stamp());
                    }
                }
            }
            println!("{} {gone} finished", stamp());
            known.remove(&gone);
        }
        if only_target.is_none() {
            for line in follower.read_new(&loop_log, false) {
                if !line.trim().is_empty() {
                    println!("{} [polarwave] {}", stamp(), without_timestamp(&line));
                }
            }
        }
        if first && current.is_empty() {
            println!("{} no agents running right now; waiting…", stamp());
        }
        first = false;
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn describes_text_and_tool_calls() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Reading the task\nfile now."},
            {"type":"tool_use","name":"Bash","input":{"command":"make test"}},
            {"type":"tool_use","name":"Edit","input":{"file_path":"/Users/x/code/myapp-worktrees/0007/src/core/lib.rs"}}
        ]}}"#;
        let lines = describe(line);
        assert_eq!(lines[0], "say     Reading the task file now.");
        assert_eq!(lines[1], "tool    Bash make test");
        assert_eq!(lines[2], "tool    Edit src/core/lib.rs");
    }

    #[test]
    fn describes_only_failing_tool_results() {
        let ok = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"fine"}]}}"#;
        assert!(describe(ok).is_empty());
        let bad = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"error[E0308]: mismatched types"}]}}"#;
        assert_eq!(describe(bad), vec!["error   error[E0308]: mismatched types"]);
    }

    #[test]
    fn describes_usage_and_result() {
        let usage = r#"{"type":"rate_limit_event","rate_limit_info":{"unifiedWindows":{"five_hour":{"utilization":0.66,"resetsAt":1800000000,"status":"allowed"}}}}"#;
        assert_eq!(describe(usage), vec!["usage   five-hour 66%"]);
        let result = r#"{"type":"result","subtype":"success","num_turns":12,"result":"ok\nPOLARWAVE_RESULT: {\"status\":\"agent_review\",\"summary\":\"store added\"}"}"#;
        assert_eq!(describe(result), vec!["done    success, 12 turns → agent_review: store added"]);
    }

    #[test]
    fn ignores_noise() {
        assert!(describe("not json").is_empty());
        assert!(describe(r#"{"type":"system","subtype":"post_turn_summary"}"#).is_empty());
    }

    #[test]
    fn follower_reads_only_complete_new_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "a\nb\n").unwrap();
        let mut f = Follower::default();
        assert_eq!(f.read_new(&path, true), vec!["a", "b"]);
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "c\npart").unwrap();
        assert_eq!(f.read_new(&path, true), vec!["c"]);
        writeln!(file, "ial").unwrap();
        assert_eq!(f.read_new(&path, true), vec!["partial"]);
    }

    #[test]
    fn follower_can_start_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, "old\n").unwrap();
        let mut f = Follower::default();
        assert!(f.read_new(&path, false).is_empty());
    }

    #[test]
    fn strips_log_timestamps() {
        assert_eq!(without_timestamp("2026-11-18 22:13:39 starting planner"), "starting planner");
    }
}
