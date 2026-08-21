# Kimi Code

Kimi Code is a native ACP target. `acp-stack` launches `kimi acp`.

## Headless authentication status

The `Authentication required` (`-32000`) rejection that previously blocked API-key-backed ACP sessions no longer applies on current Kimi Code releases. The default `kimi acp` path (the agent-core-v2 `@moonshot-ai/acp-server` engine) accepts a config-resolved provider API key at the `session/new` auth gate without an interactive OAuth login, and the documented `KIMI_MODEL_*` environment contract synthesizes exactly such a provider from `KIMI_MODEL_API_KEY` / `KIMI_MODEL_NAME` / `KIMI_MODEL_BASE_URL` — which is the launch environment `acp-stack` constructs (see below). `acp-stack` does not automate Kimi's interactive OAuth flow, and this path does not need it.

Provenance (verified against upstream `main` and `@moonshot-ai/kimi-code@0.38.0`, 2026-08-21): the agent-core-v2 engine became the default `kimi acp` surface in [PR #2627](https://github.com/MoonshotAI/kimi-code/pull/2627) (merged 2026-08-05, first shipped in the `0.34.0` release on 2026-08-06); its `session/new` gate is `ensureAuthed` in `packages/acp-server`. The legacy `acp-adapter` had received the equivalent config-provider auth fix earlier in [PR #934](https://github.com/MoonshotAI/kimi-code/pull/934) (merged 2026-07-20). The tracking issue [#1330](https://github.com/MoonshotAI/kimi-code/issues/1330) is still open and the separately proposed [PR #1570](https://github.com/MoonshotAI/kimi-code/pull/1570) was closed unmerged, so the fix arrived through a different path than the two refs we had been watching.

Kimi Code is not yet listed in the README supported-harnesses table: that flip is gated on an end-to-end prompt smoke against a released build with a real key.

## Setup

```sh
acps init --agent kimi
acps secrets set KIMI_API_KEY
acps agent set --model <kimi-model-id>
```

Agent config shape:

```toml
[agent]
id = "kimi"
command = "kimi"
args = ["acp"]
cwd = "/workspace"
env = ["KIMI_API_KEY"]
restart = "on-crash"
model = "k3"
```

`KIMI_API_KEY` stays in the encrypted secret store. At launch, `acp-stack` passes its value as `KIMI_MODEL_API_KEY`, selects `agent.model` through `KIMI_MODEL_NAME`, and fixes `KIMI_MODEL_BASE_URL` to the first-party Kimi Code endpoint. Do not add `KIMI_MODEL_*` refs to `[agent].env`.

`acps init` pins `kimi-for-coding` when `--model` is not passed because that id is available on every Kimi plan; a model already present in config is kept. K3 requires a Moderato plan or above; eligible users can select it with `acps init --agent kimi --model k3` or `acps agent set --model k3`. Model ids are accepted as supplied without ACP discovery because Kimi requires the model environment to initialize; Kimi Code validates the id when the process starts. If a hand-edited config omits `agent.model`, the runtime launches with `kimi-for-coding`. Mode values are discovered over ACP and can be selected with `acps agent set --mode <mode>`.

Kimi Code receives configured MCP servers through ACP. Managed Agent Skills are installed into `~/.agents/skills`.

The native ACP implementation advertises session list, load, and resume support at runtime. Capability-dependent operations remain gated by the live `initialize` response.
