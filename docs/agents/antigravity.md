# Google Antigravity

Google Antigravity is a native ACP target. Google publishes the official ACP server in the upstream ACP registry as `antigravity-acp`, distributed as per-platform zip archives containing `agy_acp_server.par`. `acp-stack` installs it under `~/.local/share/acp-stack/antigravity` and launches it through a `~/.local/bin/antigravity` launcher with the literal `--uid=` flag the upstream registry declares for the Linux targets. The install recipe resolves the archive URL from the upstream registry index at install time, so re-running the install step is the update path.

Authentication is headless and has two parts, both required: `acps` writes `modelProvider: "gemini"` into `~/.gemini/antigravity-cli/settings.json`, and the launch environment carries `GEMINI_API_KEY` (a Google AI Studio key) from the encrypted secret store. With both in place the harness bypasses its browser sign-in and calls the Gemini API directly. `GOOGLE_API_KEY` and `.env` files are not read by the harness.

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

## Providers

Google AI Studio is the only supported headless provider; there is no provider selection, no model selection, and no mode selection through `acps`. Model choice follows the harness's own default for API-key sessions. Antigravity's other auth paths (Google account OAuth via browser or keyring, ADC) are not usable in this runtime.

The install targets Linux x86_64 and aarch64 only, matching the platforms the upstream registry distributes for this runtime.

Antigravity receives configured MCP servers through ACP, gated by the live `initialize` capability self-report. Managed Agent Skills are installed into `~/.agents/skills`.
