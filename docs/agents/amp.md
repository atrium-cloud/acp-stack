# Amp Code Headless Support

Amp Code is an adapter-backed target for `acp-stack`. The upstream harness is the Amp CLI; the ACP-facing process is `amp-acp`.

## Sources

- Amp manual: https://ampcode.com/manual
- Amp SDK docs: https://ampcode.com/manual/sdk
- Amp npm package changes: https://ampcode.com/news/npm-package-changes
- `amp-acp` repository: https://github.com/tao12345666333/amp-acp

The Amp docs were checked on 2026-05-17. They document installation with `curl -fsSL https://ampcode.com/install.sh | bash`, API-key auth through `AMP_API_KEY`, and smart mode as an available CLI mode. The current ACP bridge is the third-party `amp-acp` adapter.

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

Amp Code does not expose raw provider/model designations through the current `acp-stack` support contract. The real ACP probe against `amp-acp v0.7.0` did not advertise a `mode` session config option, so `acps agent set --mode <mode>` is not enabled for Amp until the ACP adapter exposes modes.

From testing with an actual prompt, it appears to default to Smart mode. This could be an `amp-acp` limitation.

Amp's upstream CLI documents modes such as smart mode, but those mode selectors are not currently visible through ACP.

## Unsupported Auth Paths

Unsupported for the `acp-stack` headless contract:

- Browser or TUI login through `amp login`.
- Local account cookies or interactive setup flows.
