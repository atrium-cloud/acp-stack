# Google Antigravity

Google Antigravity is a native ACP target. Google publishes the official ACP server in the upstream ACP registry as `antigravity-acp`, distributed as per-platform zip archives containing `agy_acp_server.par`. `acp-stack` installs it under `~/.local/share/acp-stack/antigravity` and launches it through a `~/.local/bin/antigravity` launcher with the literal `--uid=` flag the upstream registry declares for the Linux targets. The install recipe resolves the archive URL from the upstream registry index at install time, so re-running the install step is the update path.

Authentication is headless and has two parts, both required: `acps` writes `auth.type: "gemini-api-key"` into `~/.gemini/antigravity-acp/settings.json`, and the launch environment carries `GEMINI_API_KEY` (a Google AI Studio key) from the encrypted secret store. With both in place the ACP server skips its browser sign-in and calls the Gemini API directly. Note the file is not the interactive CLI's `~/.gemini/antigravity-cli/settings.json` — the ACP server keeps its own settings directory and auth shape. `GOOGLE_API_KEY` and `.env` files are not read.

## Setup

```sh
acps init --agent antigravity
acps secrets set GEMINI_API_KEY
```

Agent config shape:

```toml
[agent]
id = "antigravity"
command = "antigravity"
args = ["--uid="]
cwd = "/workspace"
env = ["GEMINI_API_KEY"]
restart = "on-crash"
```

## Providers, models, and modes

Google AI Studio is the only supported headless provider; there is no provider selection through `acps`. Antigravity's other auth paths (Google account OAuth via browser or keyring, ADC) are not usable in this runtime.

The ACP server advertises model and mode selection per session: Gemini 3.x model/thinking-level tiers (default `gemini-3.7-flash-high`) and the modes `default`, `auto_edit`, and `yolo`. Pick them at init (`--model`, `--mode`, validated against the advertised values) or later with `acps agent set`. Free-tier AI Studio keys can stall on the default model while lower tiers answer, so pinning a model such as `gemini-3.5-flash-low` is recommended for keyed-but-unbilled setups. The `default` mode raises an ACP permission request for every file edit, which in the daemon path waits on an operator decision; use `auto_edit` or `yolo` for unattended runs.

The install targets Linux x86_64 and aarch64 only, matching the platforms the upstream registry distributes for this runtime.

Antigravity receives configured MCP servers through ACP, gated by the live `initialize` capability self-report. Managed Agent Skills are installed into `~/.agents/skills`.
