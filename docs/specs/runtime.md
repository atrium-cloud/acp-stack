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

The normal `acps` installation flow should install only agents or adapters published through the ACP registry. The registry supplies the agent id, package metadata, and distribution options such as platform-specific binaries or package-manager launchers. If an upstream agent needs an adapter, the registry entry for the adapter is the install unit and `[agent.adapter]` records the relationship to the upstream agent.

Example:

```toml
[agent]
id = "opencode"
name = "OpenCode"
command = "opencode"
args = ["--acp"]
cwd = "/workspace"
env = ["OPENCODE_API_KEY"]
restart = "on-crash"

[agent.install]
type = "registry"
id = "opencode"
creates = "opencode"
```

Adapter-backed agents use the adapter as the installed and launched ACP process. As of 2026-05-15, the externally identified adapter-backed agents are Claude Agent, Codex CLI, and Pi. For API-key Codex deployments, `acps` should resolve to a `codex-acp` registry entry, install the adapter distribution, and record `[agent.adapter]` metadata that identifies `codex-cli` as the upstream agent. Browser OAuth sessions and account cookies are not passed through `acp-stack`; agents that require interactive OAuth remain outside the initial supported runtime path.

Install behavior:

- `acps init` may run the installer after explicit user confirmation.
- `acps agent install` resolves the configured agent or adapter from the ACP registry before installing.
- `creates` is checked before and after installation to detect whether the command is already available.
- installer stdout/stderr and exit status are written to SQLite logs.
- in 0.0.1, installer execution is admin-tier and logged; permission-pipeline mediation lands with the 0.0.2 permission system.
- the resulting command still launches through the normal `[agent]` command, args, cwd, env, and hash verification path.
- registry `npx` package distributions install through `npm install -g`; registry `uvx` package distributions install through `uv tool install`; binary registry distributions are not installed until archive extraction support lands.
- binary URL installers download to an explicit destination, verify `sha256` when provided, mark the file executable, and then check `creates`.

Direct shell recipes are a low-level/manual escape hatch. They are not the default way operators discover or install agents because arbitrary install scripts bypass the ACP registry's curated list and metadata.

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
