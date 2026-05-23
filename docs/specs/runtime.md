# Runtime

The runtime owns child process supervision, workspace access, dependency checks, and runtime integration around one configured ACP agent.

## Supervisor

The runtime supervisor owns child processes:

- active ACP agent
- MCP stdio servers
- mediated shell commands

Runtime process behavior:

- one configured active agent per runtime
- daemon, agent, MCP servers, and mediated commands run as an unprivileged Linux user by default
- default runtime user is `acp`
- runtime user owns workspace, config, state, and secret files
- explicit start and stop
- compatibility limited to headless agents that accept direct API keys through environment variables or config files
- agents that require browser OAuth or interactive account login are unsupported in the initial release
- optional restart on crash based on config
- stderr capture into logs
- lifecycle events persisted to SQLite
- expected binary hash check if `expected_sha256` is configured

## Agent Installation

`acps agent install` resolves the configured `[agent].id` against an embedded curated catalog at `data/agents.toml` (compiled into the binary). The catalog is intentionally narrow while the headless deployment pipeline is being proven: OpenCode, Cursor CLI, and Goose are native targets; Amp through `amp-acp`, Pi through `pi-acp`, and Codex through `codex-acp` are adapter-backed targets. The registry model classifies entries as **native** (the harness itself speaks ACP) or **adapter** (an ACP adapter launches or coordinates an upstream harness), so later agents can be added one at a time once their headless setup docs and smoke verification are credible. Unsupported or unknown agents are refused before running installer code.

Every registry entry declares `[agents.harness.install.{shell,npm,github}]` for the upstream agent harness. Native entries produce one install step because the harness itself speaks ACP. Adapter-backed entries also declare `[agents.adapter.install.{shell,npm,github}]` for the ACP-facing adapter; harness and adapter install steps run concurrently and each writes an `installer_runs` row tagged with `step = "harness" | "adapter"`. A failure in either step fails the install after both in-flight steps finish. The final `[agent].command` verification runs only after all selected steps succeed.

Example native OpenCode deployment:

```toml
[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["acp"]
cwd = "/workspace"
env = ["<provider-api-key-ref>"]
restart = "on-crash"
```

The embedded registry tells the runtime to install the latest OpenCode through the official shell bootstrap into the managed `~/.local/bin` directory and verify `opencode` resolves there or on PATH.

Install paths in `data/agents.toml`:

- `shell`: free-form upstream bootstrap with a `creates` postcheck
- `npm`: `npm install -g --prefix "$HOME/.local" <package>`, verified by `creates`
- `github`: download a release asset matching `asset_pattern`, substitute `{arch}` from the path-local arch map, optionally verify a checksums asset, extract into `~/.local/bin`, and chmod the resulting binary

Latest/floating installs prefer `shell`, then `npm`, then `github`. Pinned harness installs prefer `github`, then `npm@version`; shell-only entries cannot honor a pin.

Operator-facing install:

- `acps init` can prompt for a supported agent, update `[agent]`, and run the selected agent installer after explicit user confirmation or `--install-agent`.
- `acps agent install` resolves the registry entry, refuses unsupported entries before running installer code, runs each step for supported entries, persists row(s), and verifies that `[agent].command` is now on PATH or in the managed `~/.local/bin` directory used by registry-managed installs.
- `[agent.expected_sha256]` runs against the final binary regardless of which install method ran.
- Operators can layer a private `~/.config/acp-stack/agents.toml` that supplements or replaces embedded entries by `id`.
- Operators can fully override the registry-driven path by declaring `[agent.install] type = "shell"` as an escape hatch for private forks, unreleased agents, or anything not in the curated catalog.

Browser OAuth sessions and account cookies are not passed through `acp-stack`; agents that require interactive OAuth remain outside the initial supported runtime path. Keep the embedded registry small until each additional agent has a documented non-interactive install, auth, launch, and smoke-test path.

Agent processes start from a scrubbed environment. `acp-stack` sets only the managed process `PATH`, the runtime user's `HOME`, and secrets explicitly resolved from `[agent].env`; `[agent].env` cannot override reserved runtime context such as `PATH` or `HOME`.

The embedded registry replaces an earlier runtime fetch of `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`. Upstream is now a reference: the dev-only `sync-registry-check` tool fetches upstream `registry.json` and verifies that every embedded sync id is still present upstream. A sync id is `adapter.id` for adapter-backed entries and top-level `id` for native entries. Upstream entries that are not embedded are reported for awareness but do not fail the check.

### Init run state machine

`acps init` records one row in `init_runs` per invocation and an
`init_steps` row for each phase that is executed or resumed. Tracked phase
kinds (in order) are `secrets_init`, `agent_install`, `provider_configure`,
`workspace_materialize`, `agent_headless_config`, `edge_artifacts`,
`init_complete`, and `testflight`. Optional phases are absent when they are
not requested and have no prior unsettled row. Each recorded step starts as
`pending`, transitions to `running` when its body executes, and settles to
`succeeded`, `skipped`, or `failed`. `init_runs.status` aggregates to
`succeeded` when every recorded step succeeded or was skipped; any
unsettled or failed recorded step keeps the run `failed` and bubbles the
typed error.

On rerun the orchestrator looks up each step by `(run_id, ordinal)` and
consults a per-step verifier before deciding whether to re-execute:

- `secrets_init` — verifier checks both `[auth]` secret refs are present
  in the encrypted secret store.
- `agent_install` — verifier checks the configured `creates` binary
  resolves on PATH (or under `~/.local/bin`).
- `workspace_materialize` — verifier checks every declared code/data
  source's destination has the `.acp-stack-source.json` sentinel.
- `init_complete` — verifier checks an `init.completed` event for this
  run id is already recorded in the unified log.
- Other phases (`provider_configure`, `agent_headless_config`,
  `edge_artifacts`, `testflight`) are cheap and idempotent; they
  re-execute on every resume without consulting a verifier.

The installer retry that previously lived behind a TTY prompt
(`Try the next install path now? [y/N]`) is removed. `install_resolved`
already walks `shell → npm → github` in sequence within one call, and
re-running `acps init --resume` re-executes the failed `agent_install`
step from scratch with the current registry. Non-interactive deployments
recover from a transient download failure the same way an operator at
a terminal would: by re-running the command.

`acps init --resume` continues the latest non-terminal run; with
`--run-id <id>` it targets a specific historical row. `acps init
--fresh` begins a brand-new run row even when an incomplete one exists.
Without either flag, init always begins a fresh row; auto-resume across
unrelated invocations is intentionally opt-in.

## Provider And Model Resolution

`acps init` selects the supported agent and may select the initial provider, but it does not infer a model from API-key refs or test-only defaults. Provider-backed generated config is written only after `[agent.provider]` is explicit:

- Goose: writes or merges `~/.config/goose/config.yaml` with `GOOSE_PROVIDER`, `GOOSE_MODE = auto`, summarizing context, and session naming disabled. Goose consumes provider-native API-key env vars directly, so the selected `api_key_ref` must match the provider mapping.
- OpenCode: writes or merges `~/.config/opencode/opencode.json` with a provider `apiKey` reference to the configured API-key ref; when a model is configured, it also writes the selected provider-qualified model.
- Pi: writes or merges `~/.pi/agent/settings.json` and sets `enabledModels` only when a model is configured.
- Codex: supports only `openai` and `openrouter`. OpenAI writes `~/.codex/config.toml` with the selected model and `model_provider = "openai"` while leaving auth Codex-native; switching from a generated non-OpenAI provider first backs up the previous config and removes that provider table. OpenRouter writes the Responses provider table and references `OPENROUTER_API_KEY`.

Provider id validation uses the reusable API-key/provider mapping in the runtime.

Provider/model edit paths validate model and mode values against the ACP `session/new` response before config is written. `acps init --provider` currently records mapped provider refs without selecting a model unless the operator uses the explicit custom-provider path. Cursor is model-only and stores the exact advertised value in `[agent].model`; OpenCode, Cursor, Codex, and `amp-acp v0.1.1` currently advertise ACP modes, while Pi and Goose do not.

Provider management includes a provider/model resolution layer for provider refresh:

- resolve provider ids through the embedded provider mapping
- start a provisional ACP session and read its `configOptions` before accepting a model or mode choice in `acps agent set`
- expose ACP-advertised model/mode choices through the unified API so clients can render selection without scraping agent-specific CLIs (planned)
- map available secret refs and required companion refs to allowed provider ids before accepting a provider choice
- preserve the resolved provider id, model id, and selected secret refs as non-secret config plus secret references
- update generated agent config before writing the main config; later relaunch the active Goose, OpenCode, or Pi process so the new provider/model takes effect. For Goose, model changes take effect through the next session's ACP model config update instead of `GOOSE_MODEL`.

ACP session config options are discovery data, not an execution dependency for every prompt. If discovery fails during an already-configured deployment, the runtime should keep the existing provider/model config and report the refresh failure. Init should fail fast only when no valid provider/model choice is available.

## Workspace And Files

The workspace is the shared filesystem surface between users, clients, tools, and the agent.

Default:

```text
/workspace
```

Rules:

- all relative paths resolve under `workspace.root`
- `workspace.root` is owned by the runtime user, default `acp`
- uploads land under `workspace.uploads`
- path traversal is rejected
- symlink escapes are rejected by default
- file reads have bounded size limits
- binary downloads are explicit
- writes are atomic where possible
- root execution is not the default; it is only an explicit disposable/dev profile

Persistence is provided by the deployment environment:

- ephemeral container filesystem
- mounted Docker volume
- VM disk
- network storage
- hosted workspace volume

`acp-stack` reads and writes the filesystem; it does not own volume provisioning in the initial release.

## Workspace Source

`acps init` materializes the workspace from two parallel lanes:

- `[[workspace.code_sources]]` clones Git repositories into
  `<workspace.root>/usr/code/<repo-name>/`.
- `[[workspace.data_sources]]` ingests local paths, public HTTPS archives,
  and S3 buckets into `<workspace.root>/usr/data/<data-dir-name>/`.

Example:

```toml
[[workspace.code_sources]]
type = "git"
repo = "https://github.com/example/project.git"
branch = "main"
credential_ref = "GITHUB_TOKEN"  # optional; resolved through the secret store

[[workspace.data_sources]]
type = "https"
url = "https://example.com/dataset.tar.gz"
expected_sha256 = "0123…"

[[workspace.data_sources]]
type = "s3"
bucket = "my-research-data"
prefix = "experiments/2026-05/"
region = "us-east-1"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"
```

S3 ingestion is implemented with a minimal SigV4 client (no AWS SDK
dependency); production endpoints follow the path-style
`https://s3.<region>.amazonaws.com/<bucket>` pattern. Set
`ACP_STACK_S3_ENDPOINT_OVERRIDE` to redirect to a local mock or a
VPC-internal endpoint without baking that into TOML. Only static
`access_key_ref` + `secret_key_ref` credentials are supported today;
session-token / role-assumption flows are out of scope.

`acps init --code-from <repo-url>` and `acps init --data-from <path-or-url>`
pre-seed entries into the starter config. Each is repeatable. Local paths
must be absolute, and `--data-from` rejects `http://` URLs at CLI parse
time.

Materializer behaviour:

- HTTPS downloads run through a streaming reader with a default 500 MiB
  cap, max 3 redirects, and an https-only scheme allowlist applied to both
  the requested URL and every redirect target. Archive bodies are
  extracted into the destination via the safe-extract path; non-archive
  bodies land at `<dest>/<basename>`.
- Archive extraction (tar/tar.gz/zip, format detected by magic bytes)
  rejects entries containing absolute paths, `..` segments, symlinks,
  hardlinks, FIFOs, character/block devices, or output exceeding the
  configured per-entry or cumulative size caps.
- Local sources are copied directory-recursive; symlinks in the source
  tree are refused rather than dereferenced.
- Every successful materialization drops a `.acp-stack-source.json`
  sentinel at the destination root recording the source identity.
  Re-running `acps init` with a matching sentinel skips the source without
  re-fetching; a non-empty destination without a matching sentinel
  hard-fails so init never silently merges into existing content.

Resumable init beyond the per-source sentinel skip (e.g. partial
interruption of the materializer itself) is tracked under Phase 4
Installer UX and is not part of the workspace-source feature today.

## Command Gateway

The Command Gateway is the runtime path for `POST /v1/commands`. Each submitted shell string is evaluated against `[permissions].deny` and `[permissions].review` glob lists, then spawned via `[workspace].default_shell -c <command>` with the child process in a fresh process group. The supervisor:

- resolves `cwd` under `workspace.root` (relative paths join the root, absolute paths must canonicalize inside);
- clears the child environment and forwards only the variables explicitly listed in the request, each one cross-checked against `[commands].env_allowlist`. The `commands` row records the env *names* only — never values — so SQLite history does not become a secondary plaintext store for credentials that callers pass via env;
- reads stdout/stderr in bounded chunks (up to 4 KiB per read), persisting each chunk as a `command.stdout` / `command.stderr` event and fanning it out on the `commands.{id}` WebSocket topic — chunk boundaries are not guaranteed to align with newlines;
- caps total persisted bytes at `[commands].max_output_bytes`, after which it drains the pipes and sets the row's `truncated` flag;
- enforces the per-request `timeout` (or `[commands].default_timeout`) and the explicit `POST /v1/commands/{id}/cancel` path with a two-stage SIGTERM → SIGKILL transition separated by `[commands].cancel_grace`.

The Phase 1 gateway does not yet hold submissions in an approval queue; `review` glob matches behave like `deny` outside of `mode = "auto"`.

## Dependencies And MCP

The dependency manifest is part of the reusable environment config.

Phase 2 dependency responsibilities:

- parse dependency declarations
- validate supported declaration syntax
- check whether declared commands/tools are present
- report missing dependencies in `status` and `deps check`
- launch declared MCP servers when their commands are available

Phase 4 adds `deps apply` for supported installers. The first supported target should be narrow and explicit, such as Debian/Ubuntu system packages through `apt` plus npm/pip packages when Node/Python are already available.

The initial release does not promise broad reconciliation across `apt`, `apk`, `dnf`, Homebrew, npm, pip, uv, mise, asdf, or direct runtime downloads.

Future `acps deps apply` can add installation behavior without changing the manifest shape.

## Self-Hosting

Expected flow:

```sh
ssh root@example.com
curl -fsSL https://acp-stack.dev/install.sh | sh
acps init
```

Phase 4 splits installation into two privilege domains. The installer/root phase installs `acps`, creates the runtime user, prepares owner-only directories, and installs supported OS-wide dependencies. `acps init` then runs as the runtime user and manages instance state, workspace ingestion, secrets, agent/MCP config, agent harness or adapter installation, and testflight. Deployment installers pass the selected workspace root, uploads path, and runtime user into `acps init` when creating the starter config so the generated `[workspace]` block matches the process manager's user and writable paths.

The full init testflight starts the configured agent and sends a minimal real prompt after secrets and config are present. Because this can consume provider credits, the UX must require explicit confirmation or provide a documented skip path for offline and no-spend setups.
