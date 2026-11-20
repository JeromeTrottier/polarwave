//! Role prompts. polarwave ships one prompt per role (planner, implementer, reviewer, fixer);
//! a project can override any of them with `<prompts_dir>/<role>.md`. Placeholders are filled
//! from `polarwave.toml`, so prompts contain nothing project-specific.

use crate::config::Config;
use crate::outcome::MARKER;
use crate::schedule::Action;
use std::path::Path;

pub const PR_DIR: &str = ".polarwave/pr";

const PLANNER: &str = include_str!("../prompts/planner.md");
const IMPLEMENTER: &str = include_str!("../prompts/implementer.md");
const REVIEWER: &str = include_str!("../prompts/reviewer.md");
const FIXER: &str = include_str!("../prompts/fixer.md");

pub fn builtin(role: &str) -> &'static str {
    match role {
        "planner" => PLANNER,
        "implementer" => IMPLEMENTER,
        "reviewer" => REVIEWER,
        _ => FIXER,
    }
}

/// The prompt text for a role: the project's override if present, else the built-in one.
pub fn template(repo: &Path, config: &Config, role: &str) -> String {
    if !config.prompts_dir.trim().is_empty()
        && let Ok(text) = std::fs::read_to_string(repo.join(&config.prompts_dir).join(format!("{role}.md")))
    {
        return text;
    }
    builtin(role).to_string()
}

/// Fill the placeholders of a prompt for one run.
pub fn render(template: &str, config: &Config, action: &Action, task_file: Option<&Path>) -> String {
    let check = if config.check_command.trim().is_empty() {
        "Run the project's tests, linters and build as its docs describe, until they pass. Fix warnings; don't silence them.".to_string()
    } else {
        format!("Run `{}` until it passes. Fix warnings; don't silence them.", config.check_command)
    };
    let evidence = if config.evidence.trim().is_empty() {
        "If the project has a way to take screenshots (see its docs), include the tests that prove the task and every changed screen; if not, skip screenshots.".to_string()
    } else {
        config.evidence.trim().to_string()
    };
    let instructions = if config.instructions.trim().is_empty() {
        String::new()
    } else {
        format!("\nProject instructions:\n{}\n", config.instructions.trim())
    };
    let milestone = match action {
        Action::Plan(m) => m.clone(),
        _ => config.current_milestone.clone(),
    };
    let docs = if config.planning_docs.is_empty() {
        "the README and docs".to_string()
    } else {
        config.planning_docs.iter().map(|d| format!("`{d}`")).collect::<Vec<_>>().join(", ")
    };
    template
        .replace("{{task_file}}", &task_file.map(|p| p.display().to_string()).unwrap_or_default())
        .replace("{{task_id}}", &action.task_id().map(|i| i.to_string()).unwrap_or_default())
        .replace("{{milestone}}", &milestone)
        .replace("{{tasks_dir}}", &config.tasks_dir)
        .replace("{{base_branch}}", &config.base_branch)
        .replace("{{planning_docs}}", &docs)
        .replace("{{check}}", &check)
        .replace("{{evidence}}", &evidence)
        .replace("{{instructions}}", &instructions)
        .replace("{{pr_dir}}", PR_DIR)
        .replace("{{marker}}", MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_have_no_unknown_placeholders_after_rendering() {
        let config = Config::default();
        for (role, action) in [
            ("planner", Action::Plan("M1".into())),
            ("implementer", Action::Implement(7)),
            ("reviewer", Action::Review(7)),
            ("fixer", Action::Fix(7)),
        ] {
            let text = render(builtin(role), &config, &action, Some(Path::new("/r/tasks/0007-x.md")));
            assert!(!text.contains("{{"), "{role} has an unfilled placeholder:\n{text}");
            assert!(text.contains(MARKER), "{role} must explain the result line");
        }
    }

    #[test]
    fn config_values_are_filled_in() {
        let config = Config {
            check_command: "make test".into(),
            evidence: "Use `npm run shots`.".into(),
            instructions: "Never touch vendor/.".into(),
            tasks_dir: "work/tasks".into(),
            ..Config::default()
        };
        let text = render(
            builtin("implementer"),
            &config,
            &Action::Implement(7),
            Some(Path::new("/r/work/tasks/0007-x.md")),
        );
        assert!(text.contains("Run `make test` until it passes."));
        assert!(text.contains("Use `npm run shots`."));
        assert!(text.contains("Project instructions:\nNever touch vendor/."));
        assert!(text.contains("`/r/work/tasks/0007-x.md`"));
        assert!(text.contains("(#7)"));
        assert!(text.contains("edit `work/tasks/`"));
    }

    #[test]
    fn defaults_stay_generic() {
        let text = render(builtin("reviewer"), &Config::default(), &Action::Review(1), None);
        assert!(text.contains("Run the project's tests, linters and build as its docs describe"));
        for word in ["just", "cargo", "pnpm", "Mnemo", "langtype"] {
            assert!(!text.contains(word), "built-in prompt mentions `{word}`");
        }
    }

    #[test]
    fn project_overrides_win() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/reviewer.md"), "Custom review of {{task_file}}").unwrap();
        let config = Config { prompts_dir: "agents".into(), ..Config::default() };
        assert_eq!(template(dir.path(), &config, "reviewer"), "Custom review of {{task_file}}");
        assert_eq!(template(dir.path(), &config, "fixer"), builtin("fixer"));
    }
}
