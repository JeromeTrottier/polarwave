# polarwave — guide for Claude

polarwave is a project-agnostic dispatcher for Claude Code agents (see README.md). Keep it
generic: nothing about any particular project belongs in the code or built-in prompts; projects
configure everything through `polarwave.toml`.

- `src/schedule.rs` is pure: scheduling rules live there, with tests.
- `src/outcome.rs` parses `claude -p --output-format stream-json --verbose` (the final `result`
  event and `rate_limit_event`s for the usage cap). Re-verify field names when the CLI changes.
- `src/prompts.rs` + `prompts/*.md`: built-in role prompts; every placeholder must be filled
  (tested), and they must not mention specific tools or projects (tested).
- `src/pr.rs`, `src/issues.rs`: the only code that pushes or calls GitHub. Only the owner's
  actions count; polarwave's own text carries `<!-- polarwave -->`.
- Checks: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`.
