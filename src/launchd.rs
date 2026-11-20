//! Run polarwave at login with launchd.

use anyhow::Context;
use std::path::{Path, PathBuf};

/// One launchd job per project: `dev.polarwave.<project folder>`.
pub fn label(repo: &Path) -> String {
    let name: String = repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    format!("dev.polarwave.{name}")
}

pub fn plist_path(repo: &Path) -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{}.plist", label(repo))))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The launchd property list. PATH includes the usual install folders so `claude`, `git`,
/// `cargo` and `just` are found.
pub fn plist(exe: &Path, repo: &Path, home: &str) -> String {
    let path = format!(
        "{home}/.local/bin:{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
    );
    let log = repo.join(".polarwave/polarwave.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--repo</string>
    <string>{repo}</string>
    <string>run</string>
  </array>
  <key>WorkingDirectory</key><string>{repo}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>60</integer>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{path}</string></dict>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        label = label(repo),
        exe = xml_escape(&exe.display().to_string()),
        repo = xml_escape(&repo.display().to_string()),
        path = xml_escape(&path),
        log = xml_escape(&log.display().to_string()),
    )
}

fn uid() -> anyhow::Result<String> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn install(repo: &Path) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let home = std::env::var("HOME")?;
    let path = plist_path(repo)?;
    std::fs::create_dir_all(path.parent().expect("plist has a parent"))?;
    std::fs::create_dir_all(repo.join(".polarwave"))?;
    std::fs::write(&path, plist(&exe, repo, &home))?;
    let domain = format!("gui/{}", uid()?);
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{}", label(repo))])
        .output();
    let status = std::process::Command::new("launchctl").args(["bootstrap", &domain]).arg(&path).status()?;
    anyhow::ensure!(status.success(), "launchctl bootstrap failed");
    Ok(path)
}

pub fn uninstall(repo: &Path) -> anyhow::Result<()> {
    let domain = format!("gui/{}", uid()?);
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{}", label(repo))])
        .status();
    let path = plist_path(repo)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_polarwave_for_the_project() {
        let text = plist(Path::new("/x/polarwave"), Path::new("/p/My App"), "/Users/me");
        assert!(text.contains("<string>dev.polarwave.my-app</string>"));
        assert!(text.contains("<string>/x/polarwave</string>"));
        assert!(text.contains("<string>run</string>"));
        assert!(text.contains("/p/My App/.polarwave/polarwave.log"));
        assert!(text.contains("/Users/me/.local/bin"));
    }
}
