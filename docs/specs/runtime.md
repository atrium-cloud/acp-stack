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

0.0.1 supports declared installer commands for agents with stable non-interactive installation flows. This is intentionally simpler than registry resolution.

Example:

```toml
[agent.install]
shell = "curl -fsSL https://opencode.ai/install | bash"
creates = "opencode"
```

Install behavior:

- `acps init` may run the installer after explicit user confirmation.
- `acps agent install` runs the configured installer.
- `creates` is checked before and after installation to detect whether the command is already available.
- installer stdout/stderr and exit status are written to SQLite logs.
- installer execution is mediated by the permission pipeline.
- the resulting command still launches through the normal `[agent]` command, args, cwd, env, and hash verification path.
- binary URL installers download to an explicit destination, verify `sha256` when provided, mark the file executable, and then check `creates`.

The 0.0.x line does not resolve agents from an ACP registry. Future registry installers should resolve into the same `[agent]` and `[agent.install]` shape.

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
