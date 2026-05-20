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
- [ ] Ensure daemon, agent, MCP servers, and mediated commands run as the runtime user.
- [ ] Keep root execution limited to explicit disposable/dev profile behavior.

## Config Import/Export Hardening

- [x] Validate imported config paths are absolute where required.
- [x] Reject imported config with secret values in fields that must be references.
- [x] Add import dry-run output.
- [x] Add export redaction checks.
- [x] Add config compatibility version field.
- [x] Add tests for malformed, oversized, and unsafe imports.

## Dependency Apply

- [ ] Define narrow supported `acps deps apply` actions.
- [ ] Require explicit user confirmation before applying dependency changes.
- [ ] Log dependency apply stdout, stderr, exit status, and timestamps.
- [ ] Report dependency status before and after apply.
- [ ] Avoid broad cross-distro package reconciliation.
- [ ] Distinguish privileged OS-wide dependency installation from runtime-user agent harness and adapter installation.

## Installer UX

- [ ] Define resumable init step state: completed steps are verified and reused, failed or incomplete steps resume from the first failing point.
- [ ] Define fail-on-nonempty collision behavior for init-created code and data directories.
- [ ] Add installer command status history.
- [ ] Record installed ACP adapter/harness versions and expose them in agent status.
- [ ] Add an upgrade/check command that reports stale managed agent harnesses or adapters without upgrading automatically.
- [ ] Verify install scripts and embedded registry entries cannot silently install an older protocol-incompatible adapter.
- [ ] Add retry flow for failed agent installer commands.
- [ ] Preserve installer logs for audit.
- [ ] Preserve init/download/extraction stdout and stderr in per-step log files while recording structured step status in SQLite.
- [ ] Surface expected command and hash verification failures clearly.

## Provider And Model Selection

- [ ] Fetch `https://models.dev/api.json` over HTTPS during init and explicit provider refresh.
- [ ] Expose provider/model discovery through the unified API so clients can render one selection flow for OpenCode and Pi.
- [ ] Add a provider resolution layer that maps configured secret refs to allowed `models.dev` provider ids.
- [ ] For `OPENCODE_API_KEY`, accept `models.dev` provider ids `opencode` and `opencode-go`.
- [ ] Persist the selected provider id, model id, and secret refs without storing secret values in plaintext config.
- [ ] Update generated OpenCode or Pi provider config when the selected provider/model changes.
- [ ] Relaunch the active OpenCode or Pi process after provider/model config changes.
- [ ] Document whether each supported agent can change model/mode after session creation; reject unsupported live changes explicitly.
- [ ] Use `DEEPSEEK-V4-FLASH` as the default real-prompt smoke-test model when available for the selected OpenCode provider.

## Config And Agent Setup Hardening

- [ ] Document agent-owned config lifecycle for supported agents: when config files are written, when they are read, and whether relaunch or new session is required.
- [ ] Treat plugin/skill/hook setup as unsupported unless a supported agent has a verified non-interactive setup path.

## Workspace Init Sources

- [x] Replace the single-source init model with separate code and data lanes.
- [x] Clone repositories into `/workspace/usr/code/<repo-name>/`.
- [x] Place user data under `/workspace/usr/data/<data-dir-name>/`.
- [x] Support local file or directory upload by CLI path before daemon startup.
- [x] Support public Google Drive and Dropbox data as generic HTTPS archive download and extraction.
- [x] Support S3 bucket/prefix ingestion into a derived data subfolder.
- [x] Reject archive extraction with absolute paths, `..`, symlink escapes, unsafe formats, oversized downloads, oversized extracted output, or unsupported redirects.

## Init Testflight

- [ ] After config and secrets are present, run a full init testflight that starts the agent and sends a minimal real prompt.
- [ ] Testflight must verify session creation, prompt completion, streamed updates, and terminal prompt state, not just process startup.
- [ ] For each supported agent, smoke test at least one filesystem-visible tool action when the agent supports tools.
- [ ] Fail testflight if an agent appears active but emits no progress or terminal state within the configured timeout.
- [ ] Warn that real-prompt testflight may consume provider credits and provide an explicit skip/confirmation path.
- [ ] Hard-fail unsupported init paths early: browser OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, and missing required secrets.

## Test Hardening

- [ ] Cover the two-step install flow in `tests/agent_install_tests.rs` end-to-end against a mocked GitHub API.

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

- [ ] Audit each `acpctl` command for permission boundaries.
- [ ] Ensure high-risk local commands cannot be self-approved unless policy explicitly allows it.
- [ ] Add durable audit records for all `acpctl` actions.
- [ ] Add tests proving `acpctl` cannot read secrets or rotate API keys.

## Acceptance

- [x] The runtime can be deployed through Docker.
- [x] The runtime can be deployed through systemd.
- [x] Public deployments have documented reverse proxy and Cloudflare Tunnel configurations.
- [ ] `acps deps apply` supports only narrow, explicit installation behavior.
- [ ] Security self-checks are available through CLI and API.
- [ ] `acpctl` permission boundaries are tested and audited.
