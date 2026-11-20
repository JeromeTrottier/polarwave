//! Reading what a headless Claude Code run produced (`claude -p --output-format stream-json --verbose`).
//!
//! The stream's last line is the `result` event; `rate_limit_event` lines report account-wide
//! utilization per window (`five_hour`, `seven_day`), used for the usage cap.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The line agents end their final message with (see the project skills).
pub const MARKER: &str = "POLARWAVE_RESULT:";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Reported {
    /// The status the agent asks for (e.g. `agent_review`, `needs_fix`, `awaiting_human`, `blocked`).
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub findings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl Usage {
    pub fn tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The run finished and reported a result.
    Reported { reported: Reported, usage: Usage },
    /// The run finished without the marker line.
    NoReport { message: String, usage: Usage },
    /// Claude's usage limit, or polarwave's usage cap, was reached; pause new runs until `resume_at`.
    UsageLimit { resume_at: DateTime<Utc>, reason: String },
    /// The run failed (error exit, timeout, unreadable output).
    Failed { message: String, usage: Usage },
}

/// Interpret a finished run's stdout and stderr.
pub fn interpret(stdout: &str, stderr: &str, now: DateTime<Utc>) -> Outcome {
    if let Some(resume_at) = usage_limit(&format!("{stdout}\n{stderr}"), now) {
        return Outcome::UsageLimit { resume_at, reason: "Claude usage limit reached".into() };
    }
    let json: Option<Value> = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l.trim()).ok())
        .or_else(|| serde_json::from_str(stdout.trim()).ok());
    let Some(json) = json else {
        return Outcome::Failed {
            message: format!("unreadable output: {}", tail(stderr, 400)),
            usage: Usage::default(),
        };
    };
    let usage = read_usage(&json);
    let result = json.get("result").and_then(Value::as_str).unwrap_or_default();
    if json.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        let subtype = json.get("subtype").and_then(Value::as_str).unwrap_or("error");
        return Outcome::Failed { message: format!("{subtype}: {}", tail(result, 400)), usage };
    }
    match find_marker(result) {
        Some(reported) => Outcome::Reported { reported, usage },
        None => Outcome::NoReport { message: tail(result, 400), usage },
    }
}

fn read_usage(json: &Value) -> Usage {
    let u = json.get("usage");
    let n = |k: &str| u.and_then(|u| u.get(k)).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens") + n("cache_creation_input_tokens"),
        output_tokens: n("output_tokens"),
        cost_usd: json.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
    }
}

/// One usage window as reported in a `rate_limit_event`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RateWindow {
    /// `five_hour`, `seven_day`, …
    pub name: String,
    /// Fraction of the window used, 0.0–1.0 (account-wide, interactive use included).
    pub utilization: f64,
    pub resets_at: DateTime<Utc>,
    /// `allowed`, `allowed_warning`, `rejected`, …
    pub status: String,
}

/// Parse the usage windows from a `rate_limit_event` line, if it is one.
pub fn rate_windows(line: &str) -> Option<Vec<RateWindow>> {
    let event: Value = serde_json::from_str(line.trim()).ok()?;
    if event.get("type").and_then(Value::as_str) != Some("rate_limit_event") {
        return None;
    }
    let info = event.get("rate_limit_info")?;
    let windows = info.get("unifiedWindows").and_then(Value::as_object)?;
    let mut out: Vec<RateWindow> = windows
        .iter()
        .filter_map(|(name, w)| {
            Some(RateWindow {
                name: name.clone(),
                utilization: w.get("utilization").and_then(Value::as_f64)?,
                resets_at: DateTime::<Utc>::from_timestamp(w.get("resetsAt").and_then(Value::as_i64)?, 0)?,
                status: w.get("status").and_then(Value::as_str).unwrap_or("allowed").to_string(),
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

/// The first window that blocks new work: rejected by Claude, or at/above `cap_percent` of its
/// limit, and not yet reset at `now`.
pub fn over_cap(windows: &[RateWindow], cap_percent: f64, now: DateTime<Utc>) -> Option<&RateWindow> {
    windows
        .iter()
        .filter(|w| w.resets_at > now)
        .find(|w| w.status == "rejected" || w.utilization * 100.0 + 1e-9 >= cap_percent)
}

/// Find the last `POLARWAVE_RESULT: {...}` line in the agent's final message.
pub fn find_marker(text: &str) -> Option<Reported> {
    text.lines().rev().find_map(|line| {
        let json = line.trim().trim_start_matches('`').split_once(MARKER)?.1.trim().trim_end_matches('`');
        serde_json::from_str(json).ok()
    })
}

/// Detect a usage-limit failure and when to resume. Recognises a trailing `|<unix seconds>`
/// reset time (older CLI format); otherwise waits one hour.
pub fn usage_limit(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    let idx = lower.find("usage limit")?;
    let after = &text[idx..];
    let reset = after
        .split('|')
        .nth(1)
        .and_then(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<i64>().ok())
        .and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0))
        .filter(|t| *t > now);
    Some(reset.unwrap_or(now + Duration::hours(1)))
}

fn tail(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let skip = s.chars().count() - max;
        format!("…{}", s.chars().skip(skip).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn reads_the_marker_and_usage() {
        let out = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.\nPOLARWAVE_RESULT: {\"status\":\"agent_review\",\"summary\":\"store added\"}","usage":{"input_tokens":1000,"cache_creation_input_tokens":200,"output_tokens":300},"total_cost_usd":0.12}"#;
        match interpret(out, "", now()) {
            Outcome::Reported { reported, usage } => {
                assert_eq!(reported.status, "agent_review");
                assert_eq!(reported.summary, "store added");
                assert_eq!(usage.tokens(), 1500);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_marker_is_reported() {
        let out = r#"{"is_error":false,"result":"I could not finish."}"#;
        assert!(matches!(interpret(out, "", now()), Outcome::NoReport { .. }));
    }

    #[test]
    fn error_results_fail() {
        let out = r#"{"is_error":true,"subtype":"error_max_turns","result":""}"#;
        assert!(matches!(interpret(out, "", now()), Outcome::Failed { .. }));
    }

    #[test]
    fn usage_limit_with_reset_time() {
        let text = "Claude AI usage limit reached|1800003600";
        assert_eq!(usage_limit(text, now()), DateTime::<Utc>::from_timestamp(1_800_003_600, 0));
    }

    #[test]
    fn usage_limit_without_reset_waits_an_hour() {
        let text = "Error: You've hit your usage limit. Try again later.";
        assert_eq!(usage_limit(text, now()), Some(now() + Duration::hours(1)));
        assert!(matches!(interpret("", text, now()), Outcome::UsageLimit { .. }));
    }

    const RATE_EVENT: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1800018000,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.58,"resetsAt":1800018000,"status":"allowed"},"seven_day":{"utilization":0.13,"resetsAt":1800300000,"status":"allowed"}}}}"#;

    #[test]
    fn parses_rate_limit_events() {
        let windows = rate_windows(RATE_EVENT).unwrap();
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].name, "five_hour");
        assert!((windows[0].utilization - 0.58).abs() < 1e-9);
        assert_eq!(windows[1].name, "seven_day");
        assert!(rate_windows(r#"{"type":"assistant"}"#).is_none());
        assert!(rate_windows("not json").is_none());
    }

    #[test]
    fn cap_blocks_at_or_above_the_percentage() {
        let windows = rate_windows(RATE_EVENT).unwrap();
        assert!(over_cap(&windows, 70.0, now()).is_none(), "58% and 13% are under 70%");
        assert_eq!(over_cap(&windows, 58.0, now()).map(|w| w.name.as_str()), Some("five_hour"));
        assert_eq!(over_cap(&windows, 50.0, now()).map(|w| w.name.as_str()), Some("five_hour"));
    }

    #[test]
    fn cap_ignores_windows_that_already_reset() {
        let windows = rate_windows(RATE_EVENT).unwrap();
        let later = DateTime::<Utc>::from_timestamp(1_800_018_001, 0).unwrap();
        assert!(over_cap(&windows, 50.0, later).is_none(), "five_hour has reset; seven_day is 13%");
    }

    #[test]
    fn rejected_windows_always_block() {
        let mut windows = rate_windows(RATE_EVENT).unwrap();
        windows[1].status = "rejected".into();
        assert_eq!(over_cap(&windows, 100.0, now()).map(|w| w.name.as_str()), Some("seven_day"));
    }

    #[test]
    fn stream_result_line_is_interpreted() {
        let stream = format!(
            "{{\"type\":\"system\",\"subtype\":\"init\"}}\n{RATE_EVENT}\n{}",
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok\nPOLARWAVE_RESULT: {\"status\":\"agent_review\",\"summary\":\"x\"}","usage":{"input_tokens":1,"output_tokens":1}}"#
        );
        assert!(matches!(interpret(&stream, "", now()), Outcome::Reported { .. }));
    }

    #[test]
    fn marker_with_findings() {
        let text = "Review done.\nPOLARWAVE_RESULT: {\"status\":\"needs_fix\",\"summary\":\"2 issues\",\"findings\":[\"no test for conflict\",\"clippy warning\"]}";
        let r = find_marker(text).unwrap();
        assert_eq!(r.findings.len(), 2);
    }
}
