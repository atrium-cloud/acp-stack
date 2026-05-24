# Phase 4 Todo - Packaging And Deployment

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acpctl](../specs/acpctl/acpctl.md)
- [roadmap](../mgmt/roadmap.md)

## Docker

- [x] Create production Dockerfile for the `acps` daemon.
- [x] Run as unprivileged `acp` user by default.
- [x] Mount `/workspace` as writable runtime data.
- [x] Persist `/home/acp/.config/acp-stack`.
- [x] Persist `/home/acp/.local/share/acp-stack`.
- [x] Document required environment variables and volume mounts.
- [x] Add smoke test for container startup and `GET /v1/status`.

## systemd

- [x] Add installer flow for Linux hosts.
- [x] Add bash installer/root phase that installs `acps`, creates the runtime user, installs supported OS-wide dependencies, then runs `acps init` as the runtime user.
- [x] Create unprivileged `acp` user when missing.
- [x] Create config, state, and workspace directories with owner-only permissions where required.
- [x] Install the `acps` binary.
- [x] Install a systemd unit for the daemon.
- [x] Configure `ReadWritePaths` for workspace, config, and state paths.
- [x] Add restart policy compatible with configured runtime behavior.
- [x] Document start, stop, status, and journal inspection commands.

## Reverse Proxy Guides

- [x] Write Nginx deployment guide.
- [x] Write Caddy deployment guide.
- [x] Write Cloudflare Tunnel deployment guide as the preferred public-edge pattern.
- [x] Document Cloudflare proxied-DNS public-origin deployment as an advanced fallback.
- [x] Document TLS termination at the reverse proxy.
- [x] Document WebSocket upgrade configuration.
- [x] Document public-edge compression policy.
- [x] Document `trust_proxy_headers` and trusted proxy address configuration.
- [x] Clarify that runtime HTTP hardening remains enabled behind the proxy.

## Runtime User Automation

- [x] Validate ownership of workspace, config, state, age key, and encrypted secret store.
- [x] Add remediation hints for incorrect ownership.
- [x] Ensure daemon, agent, MCP servers, and mediated commands run as the runtime user.
- [x] Keep root execution limited to explicit disposable/dev profile behavior.

## Config Import/Export Hardening

- [x] Validate imported config paths are absolute where required.
- [x] Reject imported config with secret values in fields that must be references.
- [x] Add import dry-run output.
- [x] Add export redaction checks.
- [x] Add config compatibility version field.
- [x] Add tests for malformed, oversized, and unsafe imports.

## Dependency Apply

- [x] Define narrow supported `acps deps apply` actions.
- [x] Require explicit user confirmation before applying dependency changes.
- [x] Log dependency apply stdout, stderr, exit status, and timestamps.
- [x] Report dependency status before and after apply.
- [x] Avoid broad cross-distro package reconciliation.
- [x] Distinguish privileged OS-wide dependency installation from runtime-user agent harness and adapter installation.

## Installer UX

- [x] Define resumable init step state: completed steps are verified and reused, failed or incomplete steps resume from the first failing point.
- [x] Define fail-on-nonempty collision behavior for init-created code and data directories.
- [x] Add installer command status history.
- [x] Record installed ACP adapter/harness versions and expose them in agent status.
- [x] Add an upgrade/check command that reports stale managed agent harnesses or adapters without upgrading automatically.
- [x] Add a manual end-to-end ACP compatibility test that starts the configured agent and sends a real prompt.
- [x] Add retry flow for failed agent installer commands.
- [x] Preserve installer logs for audit.
- [x] Preserve init/download/extraction stdout and stderr in per-step log files while recording structured step status in SQLite. (Workspace git clone + rev-parse stdout/stderr land on disk; pure-Rust local copy, HTTPS download/extract/fallback copy, and S3 ingest write synthetic stdout/stderr audit captures under the same per-source log tree while preserving typed errors.)
- [x] Surface expected command and hash verification failures clearly.

## Provider And Model Selection

- [x] Start a provisional ACP session during interactive provider/model setup.
- [x] Read ACP-advertised `model` and `mode` session config options before accepting model or mode choices.
- [x] Validate explicit model and mode values against ACP-advertised session config options before writing config.
- [x] When provider-backed `--model` is omitted in non-interactive mode, print ACP-advertised model values without writing a model into config (the provider lane still persists per its own step).
- [x] Expose provider/model discovery through the unified API using provider metadata and ACP-advertised model options.
- [x] Validate provider ids against the provider/env mapping and enforce the configured agent's provider scope from the mapping.
- [x] Resolve default API-key refs, companion refs, and optional provider refs from the provider/env mapping.
- [x] Persist only non-secret provider id, ACP-advertised model id, supported mode value, and secret refs without storing secret values in plaintext config.
- [x] Regenerate supported agent-owned config before writing canonical config when provider/model settings change.
- [x] Provide a manual supervised-agent restart path for provider/model changes that require process-level config reload, and route Goose model changes through ACP session config on new sessions instead of requiring restart.
- [x] Document whether each supported agent can change model/mode after session creation; reject unsupported live changes explicitly.

## Config And Agent Setup Hardening

- [x] Document agent-owned config lifecycle for supported agents: when config files are written, when they are read, and whether relaunch or new session is required.
- [x] Treat plugin/skill/hook setup as unsupported unless a supported agent has a verified non-interactive setup path.

## Workspace Init Sources

- [x] Replace the single-source init model with separate code and data lanes.
- [x] Clone repositories into `/workspace/usr/code/<repo-name>/`.
- [x] Place user data under `/workspace/usr/data/<data-dir-name>/`.
- [x] Support local file or directory upload by CLI path before daemon startup.
- [x] Support public Google Drive and Dropbox data as generic HTTPS archive download and extraction.
- [x] Support S3 bucket/prefix ingestion into a derived data subfolder.
- [x] Reject archive extraction with absolute paths, `..`, symlink escapes, unsafe formats, oversized downloads, oversized extracted output, or unsupported redirects.

## Init Testflight

- [x] After config and secrets are present, run a full init testflight that starts the agent and sends a minimal real prompt.
- [x] Integrate the existing `acps agent test` runner into `acps init --testflight` after explicit confirmation.
- [x] Testflight must verify session creation, prompt completion, streamed updates, and terminal prompt state, not just process startup.
- [x] For each supported agent, smoke test at least one filesystem-visible tool action when the agent supports tools.
- [x] Fail testflight if an agent appears active but emits no progress or terminal state within the configured timeout.
- [x] Warn that real-prompt testflight may consume provider credits and provide an explicit skip/confirmation path.
- [x] Hard-fail unsupported init paths early: browser OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, and missing required secrets.

## Test Hardening

- [x] Cover the two-step install flow in `tests/agent_install_tests.rs` end-to-end against a mocked GitHub API.

## Security Self-Check

- [x] Implement `acps security check`.
- [x] Implement `GET /v1/security/check`.
- [x] Check API keys exist and are not default or empty.
- [x] Check config, state, age key, and encrypted secret store owner-only permissions.
- [x] Check workspace is writable by the runtime user.
- [x] Check WebSocket origin and CORS policies are configured for public deployments.
- [x] Check recent auth failure rate is below configured thresholds.
- [x] Check external logging sink health when enabled.

## acpctl Hardening

- [x] Audit each `acpctl` command for permission boundaries.
- [x] Ensure high-risk local commands cannot be self-approved unless policy explicitly allows it.
- [x] Add durable audit records for all `acpctl` actions.
- [x] Add tests proving `acpctl` cannot read secrets or rotate API keys.

## Acceptance

- [x] The runtime can be deployed through Docker.
- [x] The runtime can be deployed through systemd.
- [x] Public deployments have documented reverse proxy and Cloudflare Tunnel configurations.
- [x] `acps deps apply` supports only narrow, explicit installation behavior.
- [x] Security self-checks are available through CLI and API.
- [x] `acpctl` permission boundaries are tested and audited.
