# Amp Code Headless Support

Amp Code is an adapter-backed target for `acp-stack`. The upstream harness is the Amp CLI; the ACP-facing process is `amp-acp`.

## Sources

- Amp manual: https://ampcode.com/manual
- Amp SDK docs: https://ampcode.com/manual/sdk
- Amp npm package changes: https://ampcode.com/news/npm-package-changes
- `amp-acp` repository: https://github.com/finn-lyu/amp-acp

The Amp docs were checked on 2026-05-20. They document installation with `curl -fsSL https://ampcode.com/install.sh | bash`, API-key auth through `AMP_API_KEY`, and smart/rush/deep as available CLI modes. The current ACP bridge is a fork built on the official `@ampcode/sdk`.

## Install

The harness step installs the official Amp CLI. The adapter step downloads the prebuilt `amp-acp` release binary into the runtime-managed `$HOME/.local/bin` directory that `acps agent install` verifies. Install path metadata is maintained in `data/agents.toml`.

## ACP Launch

Recommended `acp-stack` agent config:

```toml
[agent]
id = "amp"
name = "Amp Code"
command = "amp-acp"
args = []
cwd = "/workspace"
env = ["AMP_API_KEY"]
restart = "on-crash"
```

Runtime secret refs are defined by the shared API-key mapping in `data/mapping.toml` and summarized in `docs/specs/agents/api_key.md`.

## Model And Mode Selection

Amp Code does not expose raw provider/model designations through the current `acp-stack` support contract; `acps agent set --provider <provider>` and `acps agent set --model <model>` are not valid for Amp.

Instead, same as using Amp Code via its interactive TUI, select a mode instead. `amp-acp` exposes `smart`, `rush`, and `deep` as `mode` session config values, and `acps agent set --mode <mode>` writes `[agent].mode` after validating against that list. When mode is unset, Amp defaults to Smart per upstream behavior.

## Unsupported Auth Paths

Unsupported for the `acp-stack` headless contract:

- Browser or TUI login through `amp login`.
- Local account cookies or interactive setup flows.
