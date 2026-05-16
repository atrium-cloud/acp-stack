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
- agents that require browser OAuth or interactive account login are unsupported in the 0.0.x line
- optional restart on crash based on config
- stderr capture into logs
- lifecycle events persisted to SQLite
- expected binary hash check if `expected_sha256` is configured

## Agent Installation

`acps agent install` resolves the configured `[agent].id` against an embedded curated catalog at `data/registry.toml` (compiled into the binary). The catalog is intentionally narrow while the headless deployment pipeline is being proven: the first embedded support target is OpenCode with OpenCode Go. The registry model still classifies entries as **native** (the install IS the ACP-speaking process) or **adapter** (the install is a wrapper that exec()s an upstream harness), so later agents can be added one at a time once their headless setup docs and smoke verification are credible. Unsupported or unknown agents are refused before running installer code.

Native entries produce one install step. Adapter-backed entries, when added to the catalog, produce two steps: harness first, adapter second. Each step writes an `installer_runs` row tagged with `step = "install" | "harness" | "adapter"`. If the harness step fails, the adapter step is not attempted.

Example (native OpenCode deployment with OpenCode Go API-key auth):

```toml
[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["--acp"]
cwd = "/workspace"
env = ["OPENCODE_API_KEY"]
restart = "on-crash"
```

The embedded registry tells the runtime to install OpenCode from `anomalyco/opencode` GitHub Releases and verify `opencode` resolves on PATH or in the managed `~/.local/bin` directory used by GitHub Release installs.

Install types in `data/registry.toml`:

- `npx`: `npm install -g <package>`
- `uvx`: `uv tool install <package>`
- `github_release`: download a release asset matching `asset_pattern` (with `{arch}` substituted from the host), optionally verify a `checksums.txt`-style sibling, extract if `archive = "tar.gz" | "zip"` (raw binary if `"none"`), drop into `~/.local/bin/<binary_name>`, chmod +x
- `shell`: free-form script with a `creates` postcheck (catalog-side use only; operator-facing use is the escape hatch below)

Operator-facing install:

- `acps init` may run the installer after explicit user confirmation.
- `acps agent install` resolves the registry entry, refuses unsupported entries before running installer code, runs each step for supported entries, persists row(s), and verifies that `[agent].command` is now on PATH or in the managed `~/.local/bin` directory used by GitHub Release installs.
- `[agent.expected_sha256]` runs against the final binary regardless of which install method ran.
- Operators can layer a private `~/.config/acp-stack/registry.toml` that supplements or replaces embedded entries by `id`.
- Operators can fully override the registry-driven path by declaring `[agent.install] type = "shell"` as an escape hatch for private forks, unreleased agents, or anything not in the curated catalog.

Browser OAuth sessions and account cookies are not passed through `acp-stack`; agents that require interactive OAuth remain outside the initial supported runtime path. Keep the embedded registry small until each additional agent has a documented non-interactive install, auth, launch, and smoke-test path.

The embedded registry replaces an earlier runtime fetch of `https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json`. Upstream is now a reference: the dev-only `sync-registry-check` tool fetches upstream `registry.json` and verifies that every embedded id is still present upstream. Upstream entries that are not embedded are reported for awareness but do not fail the check.

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

`acp-stack` reads and writes the filesystem; it does not own volume provisioning in the 0.0.x line.

## Workspace Source

`acps init` can seed the workspace from one source:

- `none` - start with an empty workspace and upload work data later
- `git` - clone a repository into the workspace
- `s3` - download or sync an S3 bucket/prefix into the workspace

Example:

```toml
[workspace.source]
type = "s3"
bucket = "my-research-data"
prefix = "experiments/2026-05/"
dest = "/workspace/data"
access_key_ref = "AWS_ACCESS_KEY_ID"
secret_key_ref = "AWS_SECRET_ACCESS_KEY"
region = "us-east-1"
```

## Command Gateway

The Command Gateway is the runtime path for `POST /v1/commands`. Each submitted shell string is evaluated against `[permissions].deny` and `[permissions].review` glob lists, then spawned via `[workspace].default_shell -c <command>` with the child process in a fresh process group. The supervisor:

- resolves `cwd` under `workspace.root` (relative paths join the root, absolute paths must canonicalize inside);
- clears the child environment and forwards only the variables explicitly listed in the request, each one cross-checked against `[commands].env_allowlist`. The `commands` row records the env *names* only — never values — so SQLite history does not become a secondary plaintext store for credentials that callers pass via env;
- reads stdout/stderr in bounded chunks (up to 4 KiB per read), persisting each chunk as a `command.stdout` / `command.stderr` event and fanning it out on the `commands.{id}` WebSocket topic — chunk boundaries are not guaranteed to align with newlines;
- caps total persisted bytes at `[commands].max_output_bytes`, after which it drains the pipes and sets the row's `truncated` flag;
- enforces the per-request `timeout` (or `[commands].default_timeout`) and the explicit `POST /v1/commands/{id}/cancel` path with a two-stage SIGTERM → SIGKILL transition separated by `[commands].cancel_grace`.

The 0.0.1 gateway does not yet hold submissions in an approval queue; `review` glob matches behave like `deny` outside of `mode = "auto"`.

## Dependencies And MCP

The dependency manifest is part of the reusable environment config.

0.0.2 dependency responsibilities:

- parse dependency declarations
- validate supported declaration syntax
- check whether declared commands/tools are present
- report missing dependencies in `status` and `deps check`
- launch declared MCP servers when their commands are available

0.0.4 adds `deps apply` for supported installers. The first supported target should be narrow and explicit, such as Debian/Ubuntu system packages through `apt` plus npm/pip packages when Node/Python are already available.

The 0.0.x line does not promise broad reconciliation across `apt`, `apk`, `dnf`, Homebrew, npm, pip, uv, mise, asdf, or direct runtime downloads.

Future `acps deps apply` can add installation behavior without changing the manifest shape.

## Self-Hosting

Expected flow:

```sh
ssh root@example.com
curl -fsSL https://acp-stack.dev/install.sh | sh
acps init
```
