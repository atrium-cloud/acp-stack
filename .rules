# .rules

## About This Project

`acp-stack` is a standalone, self-hostable Linux runtime for ACP-compatible agents.

- Runtime: Rust-only, distributed as a single deployable binary.
- CLI: `acps` is the human/operator-facing CLI.
- Local agent interface: `acpctl` is the constrained local interface for agents running inside the instance.
- Protocol boundary: `acp-stack` acts as an ACP client and uses ACP as the agent protocol boundary.
- State: SQLite is the local source of truth for runtime history.
- Secrets: age-compatible encrypted storage, with secret references in config.
- Config: portable TOML at `~/.config/acp-stack/acp-stack.toml`.

References:

- Project spec: docs/specs/project-spec.md
- API contract: docs/specs/api/api.md
- ACP bridge: docs/specs/acp/acp-bridge.md
- CLI: docs/specs/cli.md
- Runtime: docs/specs/runtime.md
- Security: docs/specs/security.md
- Linear MCP example: docs/specs/mcp.md
- State and logging: docs/specs/state-logging.md
- Architecture: docs/mgmt/architecture.md
- Tech stack: docs/mgmt/tech-stack.md
- Roadmap: docs/mgmt/roadmap.md
- Todos: docs/todos/

## General Behavioral Guidance

- Interactions with humans
    - Avoid making assumptions, especially engineering decisions, on behalf of humans. Pause and ask unless the liberty to pursue the most sensible solution is explicitly given.
    - If given an implementation task that has been thoroughly discussed, complete all parts of the task. If a blocker affects implementation, pause and report immediately.
    - During spec review, rank real issues by priority and avoid over-indexing on minor wording preferences.
    - When reporting findings, rank by priority (`P0`, `P1`, `P2`, etc.).
- Git
    - Do not run `git commit`, `git reset`, or `git restore`. These operations must only be performed by humans.
    - When asked to compose a commit message, review the staged changes and base the draft on them.
    - Do not revert user changes unless explicitly asked.
- Keeping things current
    - Perform web searches when current external facts matter.
    - When using web search, keep the current date in mind and run `date` if uncertain.
    - When setting up dependencies, use the latest compatible versions unless the project pins a version.
- Maintaining documentation
    - After adding a new runtime module or changing module boundaries, update docs/mgmt/architecture.md.
    - After adding a new dependency, update docs/mgmt/tech-stack.md.
    - After changing user-facing behavior, update the relevant file under docs/specs/.
    - If a task maps to docs/todos/ checkboxes, update them as work is completed.
    - Keep specs in `docs/specs/`.
- Before writing code
    - Explore first. Search for existing modules, docs, patterns, and naming.
    - Trace the full code path of the area being modified before changing it.
    - When fixing a bug, identify the root cause before proposing a fix.
    - After making a change, trace adjacent behavior for regressions.
- Code quality
    - Prioritize correctness and clarity. Speed and efficiency are secondary unless specified.
    - Avoid patch-on-patch fixes. If a change requires repeated boilerplate, special casing, or an architectural workaround, stop and discuss the design.
    - Avoid creative additions unless explicitly requested.
- Programmatic code checks
    - Run these after a logical set of changes are complete, or before commit:
        - Rust checks: when a Rust workspace exists, use the repository's own Cargo commands for build, test, formatting, and linting.
        - Codex audit:
            - If you are not Codex, run `bash scripts/codex-audit.sh` to check for issues and address findings accordingly.
            - If you are Codex, 
                - Dispatch a subagent for the same review purpose instead of running `scripts/codex-audit.sh` yourself.
                - Codex subagent use: always launch subagents with GPT-5.5 medium.
            - If `scripts/codex-audit.sh` is run, make sure changes are staged first, then run it to check for issues.
            - The script can run for a while especially if the diff is large. Wait until its completion.
            - Once an agent reads the output from Codex, validate every issue found, then plan appropriate fixes.
            - Run this script outside sandbox.
            - Once this script is running, let it run until finished. Do not kill an in-progress audit that has not produced output because it has already incurred cost. Killing it prematurely is wasteful.
            - The rule of thumb is run this script 3 times, address all concrete problems, until only circumstantial P2s remain. The loop can end earlier if P0/P1s are addressed sufficiently already.
        - Pre-commit hook at `.git/hooks/pre-commit`, if present. It may stage changes, auto-fix format issues, and run relevant checks.
            - Run this script outside sandbox if it requires internet.
- Tests
    - Appropriate tests must be added for every new logical unit of code created, in the same initial feature commit.
    - When a feature is updated, relevant tests must be updated accordingly after functional code changes.
    - When a bug fix is implemented or refactoring is performed, tests must be run promptly to check for regressions.
    - Using real LLMs for testing: default to use `deepseek-v4-flash` (model released in Apr 2026). Default to use OpenCode Go as provider where supported, otherwise OpenRouter. For other cases, agent must consult human for specific guidance. As an LLM your intenal knowledge of what models exist is woefully outdated.
- Security
    - Please refrain from probing into developer secrets like `.env.*` files.
    - If you have accidentally read a secret, report to the human immediately and ask them to rotate it.
- Code Review
    - When asked to perform a code review, review the changes on their own merits and from the perspective of the feature or product.
    - If provided with a plan or spec, use it as context, but do not ignore bugs simply because they are outside the plan.
- Testing agent with ACP
    - Run the harness to be tested in interactive shell outside sandbox to make use of existing env vars.
    - Refrain from claiming an agent is supported before the real prompt smoke passes end-to-end.
    - Always add newly supported agents to the end of [agents.toml](./data/agents.toml) and [README.md](./README.md).

## Error Handling

This project is early-stage. Catching bugs early matters more than graceful degradation.

- Fail fast. No silent swallowing.
- No catch-all error handlers that hide root causes.
- Rust: propagate errors with `?`. Never use `let _ =` on fallible operations.
- Never write empty `match` arms that discard errors.
- Error messages should be specific enough to identify the failed subsystem and action.

## Guidance On Rust

- Comments should explain why, not summarize what the code does.
- Prefer implementing functionality in existing files unless it is a new logical component.
- Avoid creating many tiny files for tightly coupled code.
- Never create files with `mod.rs` paths; prefer `src/some_module.rs` over `src/some_module/mod.rs`.
- When creating new crates, specify the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of relying on the default `lib.rs`.
- Never use `unwrap()` in production code. Propagate errors with `?` or handle them explicitly.
- Never silently discard errors with `let _ =` on fallible operations.
- Use full words for variable names; avoid abbreviations like `cfg` when `config` is clearer.
- TODO comments are forbidden unless explicitly approved by a human developer.
- Keep `main.rs` thin. Put runtime behavior behind focused modules and services.
- Prefer typed domain errors, ideally using `thiserror`, once the Rust workspace exists.
- Config, state, secrets, permissions, API, ACP bridge, workspace, command gateway, and runtime supervisor should remain separate concerns.

## Agent-Specific Notes

### Claude Code

- Do not use `sed` or `cat`. Always use Edit/Write tools properly.
- Always run the appropriate repo checks when you believe changes are ready. Other checks are not a substitute for the pre-commit hook.
