# Kilo Code

Kilo Code is a native ACP target, an OpenCode fork. `acp-stack` launches `kilo acp`.

## Known limitation

Upstream issue [Kilo-Org/kilocode#10768](https://github.com/Kilo-Org/kilocode/issues/10768) reported `session/prompt` hangs for headless stdio clients on 7.3.16/Windows and was stale-closed. The code path has since been rewritten (`acp-next`). A real ACP prompt completed end-to-end against `acp-stack` on 2026-08-21: `session/prompt` returned `end_turn` with no hang, over OpenRouter. The fix holds for headless stdio.

## Setup

Run:

```sh
acps init --agent kilo
acps secrets set KILO_API_KEY
acps agent set --model <model-id>
```

`init` runs first: it creates the config, admin key, and secret store that `acps secrets set` requires. On a fresh box, the key lands only after that store exists.

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

## Provider and API key

### The gateway key

- `KILO_API_KEY` stays in the encrypted secret store. It reaches the process only through `[agent].env`.
- The Kilo CLI's gateway provider reads it directly from the environment, so `kilo auth login` is not required.
- Kilo's default active provider is its own gateway. Without `KILO_API_KEY` (or stored Kilo auth), the gateway stalls the ACP session.
- `KILO_API_KEY` is the CLI's key. It is distinct from `KILOCODE_API_KEY`, which only the standalone Kilo gateway SDK reads.

### Non-gateway providers

To run a provider other than the Kilo gateway:

- Provider-native keys such as `OPENROUTER_API_KEY` are honored OpenCode-style.
- Declare the provider-native key and `KILO_PROVIDER` at init, via `--agent-env-ref` or the interactive env add-loop. `KILO_PROVIDER` is a `[agent].env` ref whose secret value is the provider id, e.g. `openrouter`.
- Keep `KILO_API_KEY` in `[agent].env`. Kilo requires it in the process env even when the gateway is not the active provider, but accepts an empty value (verified on a clean fly.io Sprite over OpenRouter on 2026-08-21).
- `init`, `acps config import`, and `acps agent set --model` seed the declaration when missing. When a recognized provider-native credential is declared, they record an empty placeholder for `KILO_API_KEY` automatically. No separate `acps secrets set KILO_API_KEY` is needed.
- Select an `<provider>/<model>` model, e.g. `openrouter/<model>`.

`acp-stack` has no native config module for Kilo. Provider and model selection stay with the harness (`set_provider = false`).

## Loopback bridge

Internally `kilo acp` bridges stdio ACP to an embedded `127.0.0.1` HTTP server. The host needs a working loopback interface. The bridge is invisible to ACP clients.

## Model and mode selection

Model selection goes through ACP. `acps` applies the configured model via `session/set_config_option` on each new session. Mode values (`build`, `plan`, inherited from the OpenCode fork) are discovered over ACP. Select one with `acps agent set --mode <mode>`.

Managed Agent Skills are installed into `~/.agents/skills`, which Kilo's OpenCode-derived skill discovery reads natively.

## Session Resume

`session/load`, `session/resume`, and `session/list` are discovered from Kilo's `initialize` reply at runtime. For reconnection, stale-prompt, and recovery behavior, see the Sessions section of [docs/specs/acp/acp-bridge.md](../specs/acp/acp-bridge.md).
