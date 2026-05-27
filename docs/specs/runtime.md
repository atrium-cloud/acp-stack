# Runtime

The runtime starts from config, prepares local state and secrets, launches the configured ACP agent, and exposes the API, WebSocket, and local `acpctl` interfaces.

## Supervisor

The supervisor owns the configured agent process. It starts the agent with:

- the configured command, args, cwd, and restart policy
- a scrubbed environment with managed `PATH` and the runtime user's `HOME`; `[agent].env` cannot override these reserved keys
- secret values listed in `[agent].env`

Lifecycle transitions are recorded in durable state and published to live subscribers. Agent start, stop, and restart are admin operations.

## Agent Installation

Supported agents are declared in the embedded catalog. Entries may be native ACP agents or adapter-backed agents with separate harness and adapter install steps.

Installer behavior:

- refuse unsupported catalog entries
- install into runtime-managed paths
- verify declared executables after install
- record install outcomes for operator inspection
- never receive provider API keys

Pinned installs use catalog metadata when available. Floating installs use the catalog's preferred install path.

## Init

`acps init` creates or validates config and state, initializes encrypted secrets, generates API keys when absent, and can configure agents, Agent Skills, providers, workspace sources, MCP servers, edge profiles, and testflight.

Init is resumable. A resumed run skips completed work whose result still exists and retries incomplete or failed work. Existing config and API keys are preserved unless the operator explicitly resets the instance.

## Provider And Model Resolution

Provider ids are resolved through the provider metadata for the configured agent. Mapped models and modes are validated against ACP-advertised session config options where the agent exposes them.

Custom providers are accepted only for agents that support them. Custom model ids are operator-supplied and are not certified by `acp-stack`.

Agent-owned config files are written before canonical config changes are committed. If provisioning fails, the canonical config is not advanced.

## Workspace And Files

The workspace API is rooted at `[workspace].root`. All request paths are workspace-relative. The runtime rejects traversal, absolute paths, NUL bytes, and symlink escapes.

Workspace operations support:

- metadata
- directory listing
- file read/write
- upload/download
- single-file delete

Writes are atomic where supported by the host filesystem. Mutations are logged and published to the workspace event topic.

## Workspace Sources

Workspace sources populate a new or empty destination under the workspace:

- Git code sources under `usr/code`
- local, HTTPS, or S3 data sources under `usr/data`

Materialization refuses unsafe archives, parent-directory traversal, symlinks, hardlinks, special files, and oversized entries. Each completed source drops a `.acp-stack-source.json` sentinel at its destination root. A non-empty destination without a matching sentinel hard-fails with `workspace.destination_not_empty` so init never silently merges into existing content.

## Command Gateway

The Command Gateway runs shell commands through the configured default shell inside the workspace boundary. It applies permission policy before execution, streams output to live subscribers, persists bounded output, and supports cancellation and timeouts.

Only environment variables in `[commands].env_allowlist` are forwarded from the request. Secrets are not injected into command children unless another explicit runtime mechanism provides them.

## Dependencies And MCP

Dependency declarations report whether expected tools, packages, runtimes, and MCP servers are present. Install actions run only when explicitly declared for a command dependency.

MCP server declarations are resolved at ACP session creation, load, or resume. Secret refs for stdio env vars and HTTP headers are resolved from the encrypted secret store at attach time.

## Self-Hosting

The supported deployment shapes are Docker and systemd. Public exposure should go through Cloudflare Tunnel, Nginx, or Caddy while runtime hardening remains enabled behind the edge.
