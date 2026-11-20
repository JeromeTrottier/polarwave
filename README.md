# polarwave

Runs [Claude Code](https://claude.com/claude-code) agents on your project's task queue, on your
own Mac, while it's on: a **planner** breaks a milestone into tasks, an **implementer** builds
each one (tests first) in its own git worktree, a **reviewer** checks it with fresh eyes, a
**fixer** addresses findings. Reviewed work arrives as a **GitHub pull request** with a
description and validation screenshots; you steer everything from **GitHub issues**.

polarwave knows nothing about your project: language, build tool and conventions come from
`polarwave.toml` and your project's own docs (e.g. `CLAUDE.md`).

## How it works

```
 milestone (your planning docs)
   └─ planner ──▶ task files (status: proposed) ──▶ one GitHub issue each
 you add the `approved` label on an issue
   └─ implementer, in ../<project>-worktrees/<id> on branch task/<id>-<slug>
        └─ reviewer (fresh context) ──▶ pass: polarwave runs the checks, pushes the branch,
                                        opens a PR (description + screenshots, "Closes #N")
                                    └─ fail: fixer (max 3 attempts)
 you merge the PR ──▶ task done, issue closed
 you comment / request changes ──▶ fixer
```

| On GitHub you… | polarwave… |
|---|---|
| add the **`approved`** label to an issue | starts the task once its dependencies are done |
| **edit** the issue before work starts | updates the task's goal, criteria and notes |
| **comment** | passes it to the agents as a note; answers unblock a blocked task |
| **close** the issue | cancels the task |
| **merge** the PR | marks the task done and pulls the base branch |
| **comment on / request changes on** the PR | sends the task back to the fixer |

Issue labels follow the task: `polarwave: proposed`, `queued`, `in progress`, `your review`,
`blocked`, `done`, `cancelled`.

## Requirements

- macOS (launchd), git ≥ 2.42, Rust (to build)
- [Claude Code](https://claude.com/claude-code) CLI, signed in (`claude` on your PATH)
- For PRs and issues: [GitHub CLI](https://cli.github.com) signed in (`gh auth login`) with `repo` scope

## Install

```bash
cargo install --git https://github.com/JeromeTrottier/polarwave
```

## Set up a project

```bash
cd ~/code/my-project
polarwave init        # writes polarwave.toml, tasks/_template.md, adds .polarwave/ to .gitignore
```

Edit `polarwave.toml` (every option is documented in it). The ones that matter most:

| Setting | What |
|---|---|
| `planning_docs` | Where your milestones are described (e.g. `["docs/PLAN.md"]`) |
| `current_milestone` | The milestone the planner works on (`M1`) |
| `check_command` | Must pass before hand-off and before a PR (`make test`, `just check`, `npm test`…) |
| `evidence` | How agents take validation screenshots (e.g. "Run `npm run shots -- <page>`") |
| `instructions` | Anything every agent should know, added to every prompt |
| `pull_requests`, `issues` | Turn on the GitHub flow (needs the base branch pushed) |

Then start it (now and at every login):

```bash
polarwave install
```

## Watching

- **GitHub**: issue labels, PRs and notifications.
- `polarwave watch [<task>]`: live feed of running agents (what they say, tool calls, errors, usage).
- `polarwave status`: tasks, running agents, usage against the cap.

## Usage limits

Every run reports your account's Claude usage per window (5-hour, 7-day). polarwave starts no new
runs while any window is at or above `max_usage_percent` (default **70%**) and stops a running
agent the moment it crosses it, resuming after the reset. Your interactive use counts too, so the
rest stays free for you. `max_runs_per_day`, `max_tokens_per_day` and `quiet_hours` add limits.

## Commands

| Command | Does |
|---|---|
| `polarwave init` | Set up the current project |
| `polarwave install` / `uninstall` | Start at login with launchd (one job per project) / remove it |
| `polarwave status` | Tasks, running agents, usage |
| `polarwave watch [<id>] [--from-start]` | Live feed of running agents |
| `polarwave approve <id>…` | Approve without GitHub |
| `polarwave accept <id>` | Merge a task's PR (or, without PRs, merge locally after checks) |
| `polarwave reject <id> "<note>"` | Send finished work back with a note |
| `polarwave publish <id>` | Open or update a task's PR now |
| `polarwave pause` / `resume` / `stop` | Control the loop |
| `polarwave once` / `run` | One cycle / the loop itself (what launchd runs) |

## Agents and prompts

Each role has a built-in prompt (`prompts/` in this repo) filled in from `polarwave.toml`. To
change one for your project, set `prompts_dir` and add `<role>.md` (`planner`, `implementer`,
`reviewer`, `fixer`); placeholders like `{{task_file}}`, `{{check}}`, `{{evidence}}`,
`{{instructions}}`, `{{base_branch}}`, `{{tasks_dir}}`, `{{pr_dir}}`, `{{marker}}` are filled in.

Agents run headless (`claude -p … --output-format stream-json --permission-mode acceptEdits`)
with the environment variables `POLARWAVE=1`, `POLARWAVE_ROLE`, `POLARWAVE_TARGET`,
`POLARWAVE_BASE`, so your project's Claude Code hooks can tell them apart from interactive use.
They end with a `POLARWAVE_RESULT: {...}` line, and leave PR material in `.polarwave/pr/`.

## Safety

- **Agents never push, merge or call GitHub.** polarwave does, in one place, only for task
  branches; it never pushes your base branch. Deny `git push`, `gh` etc. for agents in your
  project's `.claude/settings.json`.
- **Only the repository owner's GitHub actions count** (labels, closing, comments, reviews).
  Anyone else's text is ignored: on a public repo it could carry injected instructions.
- Each task works in its own worktree; runs have turn limits, timeouts, a fix-attempt limit, a
  concurrency cap and the usage cap. `polarwave stop` is the kill switch.

## Files

| Path (in your project) | What |
|---|---|
| `polarwave.toml` | Configuration (tracked) |
| `<tasks_dir>/NNNN-slug.md` | Task files: YAML header + goal, criteria, notes, log (tracked) |
| `.polarwave/` | State, usage, run logs, live streams, PR bodies (ignored) |
| `../<project>-worktrees/` | One git worktree per task, plus `_assets` for screenshots |

## Licence

Apache-2.0
