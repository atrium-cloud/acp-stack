# Kilo Code

Kilo Code is a native ACP target, an OpenCode fork. `acp-stack` launches `kilo acp`.

## Known limitation

Upstream issue [Kilo-Org/kilocode#10768](https://github.com/Kilo-Org/kilocode/issues/10768) reported `session/prompt` hangs for headless stdio clients on 7.3.16/Windows and was stale-closed; the code path has since been rewritten (`acp-next`). A real ACP prompt completed end-to-end against `acp-stack` on 2026-08-21 (`session/prompt` returned `end_turn` with no hang, over OpenRouter), confirming the fix for headless stdio.

## Setup

```sh
acps init --agent kilo
acps secrets set KILO_API_KEY
acps agent set --model <model-id>
```

`init` runs first: it creates the config, admin key, and secret store that `acps secrets set` requires, so a fresh box cannot store the key beforehand.

Agent config shape:

```toml
[agent]
id = "kilo"
command = "kilo"
args = ["acp"]
cwd = "/workspace"
env = ["KILO_API_KEY"]
restart = "on-crash"
```

`KILO_API_KEY` stays in the encrypted secret store and reaches the process only through `[agent].env`; the Kilo CLI's gateway provider reads it directly from the environment, so `kilo auth login` is not required. Kilo's default active provider is its own gateway, which stalls the ACP session when no `KILO_API_KEY` (or stored Kilo auth) is present. Provider-native keys such as `OPENROUTER_API_KEY` are honored OpenCode-style, so to run a different provider set `KILO_PROVIDER` to the provider id — a `[agent].env` ref whose secret value is the id, e.g. `openrouter` — alongside the provider-native key, then select an `openrouter/<model>` model. Kilo requires `KILO_API_KEY` present in the process env even when the active provider is not its gateway, but accepts an empty value (verified on a clean fly.io Sprite over OpenRouter on 2026-08-21). So for a non-Kilo provider declare the provider-native key and `KILO_PROVIDER` at init (via `--agent-env-ref` or the interactive env add-loop) and keep `KILO_API_KEY` in `[agent].env`; init, `acps config import`, and `acps agent set --model` seed the declaration when missing and, when a recognized provider-native credential is declared, record an empty placeholder for it automatically — no separate `secrets set` for `KILO_API_KEY` is needed. There is no `acp-stack`-written native config module for Kilo; provider and model selection stay with the harness (`set_provider = false`). Note that `KILO_API_KEY` is the CLI's key and is distinct from `KILOCODE_API_KEY`, which only the standalone Kilo gateway SDK reads.

Internally `kilo acp` bridges stdio ACP to an embedded `127.0.0.1` HTTP server, so the host needs a working loopback interface; the bridge is invisible to ACP clients.

Model selection goes through ACP: `acps` applies the configured model via `session/set_config_option` on each new session, and mode values (`build`, `plan`, inherited from the OpenCode fork) are discovered over ACP and can be selected with `acps agent set --mode <mode>`.

Managed Agent Skills are installed into `~/.agents/skills`, which Kilo's OpenCode-derived skill discovery reads natively.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from Kilo's `initialize` reply at runtime; `data/agents.toml` does not pin a value.

If the live ACP connection to Kilo drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect by calling `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new Kilo advertises `sessionCapabilities.resume`. If it does not, a fresh `POST /v1/sessions` is the recovery path.
