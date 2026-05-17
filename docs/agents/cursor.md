# Cursor CLI Headless Support

Cursor CLI is a native ACP target for `acp-stack`. The ACP-facing process is `cursor-agent acp`.

## Sources

- Cursor CLI install docs: https://docs.cursor.com/en/cli/installation
- Cursor CLI auth docs: https://docs.cursor.com/en/cli/reference/authentication
- Cursor ACP docs: https://docs.cursor.com/en/cli/acp

The Cursor docs were checked on 2026-05-17. They document installation with `curl https://cursor.com/install -fsS | bash`, API-key auth through `CURSOR_API_KEY`, and ACP stdio mode through `cursor-agent acp`.

## Install

Cursor CLI is treated as a native ACP peer. No separate adapter is required for this entry.

The official bootstrap installs `cursor-agent` into the runtime-managed `$HOME/.local/bin` directory that `acps agent install` verifies. Install path metadata is maintained in `data/agents.toml`.

## ACP Launch

Recommended `acp-stack` agent config:

```toml
[agent]
id = "cursor"
name = "Cursor CLI"
command = "cursor-agent"
args = ["acp"]
cwd = "/workspace"
env = ["CURSOR_API_KEY"]
restart = "on-crash"
```

`acp-stack` launches agents with a scrubbed environment plus managed `PATH`, the runtime user's `HOME`, and the secrets listed in `[agent].env`. Cursor's official wrapper requires `HOME` for its config/cache paths.

Runtime secret refs are defined by the shared API-key mapping in `data/mapping.toml` and summarized in `docs/specs/agents/api_key.md`.

## Model And Mode Selection

Cursor CLI auth is independent from model discovery. `acps agent set --model <model-id>` validates model choices against Cursor's ACP-advertised model list, stores the exact advertised model value, and uses `CURSOR_API_KEY` as the runtime secret ref. Cursor is not listed under provider mappings because its ACP model values do not expose provider ids.

Cursor advertises ACP mode values through `session/new`. `acps agent set --mode <mode>` validates against that list; the current real ACP probe returned `agent`, `ask`, and `plan`.

## Unsupported Auth Paths

Unsupported for the `acp-stack` headless contract:

- Browser login through `cursor-agent login`.
- Local account cookies or browser sessions created interactively.
