# Kilo Code

Kilo Code is a native ACP target, an OpenCode fork. `acp-stack` launches `kilo acp`.

## Known limitation

Upstream issue [Kilo-Org/kilocode#10768](https://github.com/Kilo-Org/kilocode/issues/10768) reported `session/prompt` hangs for headless stdio clients on 7.3.16/Windows and was stale-closed; the code path has since been rewritten (`acp-next`), so it is plausibly fixed but not yet confirmed end-to-end against `acp-stack`.

## Setup

```sh
acps secrets set KILO_API_KEY
acps init --agent kilo
acps agent set --model <model-id>
```

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

`KILO_API_KEY` stays in the encrypted secret store and reaches the process only through `[agent].env`; the Kilo CLI's gateway provider reads it directly from the environment, so `kilo auth login` is not required. Provider-native keys such as `OPENROUTER_API_KEY` are also honored, OpenCode-style, so those refs can be used in `[agent].env` instead. There is no `acp-stack`-written native config module for Kilo; provider and model selection stay with the harness (`set_provider = false`). Note that `KILO_API_KEY` is the CLI's key and is distinct from `KILOCODE_API_KEY`, which only the standalone Kilo gateway SDK reads.

Internally `kilo acp` bridges stdio ACP to an embedded `127.0.0.1` HTTP server, so the host needs a working loopback interface; the bridge is invisible to ACP clients.

Model selection goes through ACP: `acps` applies the configured model via `session/set_config_option` on each new session, and mode values (`build`, `plan`, inherited from the OpenCode fork) are discovered over ACP and can be selected with `acps agent set --mode <mode>`.

Managed Agent Skills are installed into `~/.agents/skills`, which Kilo's OpenCode-derived skill discovery reads natively.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from Kilo's `initialize` reply at runtime; `data/agents.toml` does not pin a value.

If the live ACP connection to Kilo drops, `restart = "on-crash"` relaunches the supervised agent automatically. Any prompt that was mid-stream is flipped to `stalled` once the stale-prompt sweeper observes no further updates beyond `[prompts].stale_threshold`. Clients reconnect by calling `GET /v1/sessions/{id}/snapshot`, wait for the agent process to be running, then call `POST /v1/sessions/{id}/resume` when the new Kilo advertises `sessionCapabilities.resume`. If it does not, a fresh `POST /v1/sessions` is the recovery path.
