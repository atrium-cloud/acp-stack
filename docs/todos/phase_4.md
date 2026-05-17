# Phase 4 Todo - Packaging And Deployment

## References

- [project-spec](../specs/project-spec.md)
- [api](../specs/api/api.md)
- [acpctl](../specs/acpctl/acpctl.md)
- [roadmap](../mgmt/roadmap.md)

## Docker

- [ ] Create production Dockerfile for the `acps` daemon.
- [ ] Run as unprivileged `acp` user by default.
- [ ] Mount `/workspace` as writable runtime data.
- [ ] Persist `/home/acp/.config/acp-stack`.
- [ ] Persist `/home/acp/.local/share/acp-stack`.
- [ ] Document required environment variables and volume mounts.
- [ ] Add smoke test for container startup and `GET /v1/status`.

## systemd

- [ ] Add installer flow for Linux hosts.
- [ ] Add bash installer/root phase that installs `acps`, creates the runtime user, installs supported OS-wide dependencies, then runs `acps init` as the runtime user.
- [ ] Create unprivileged `acp` user when missing.
- [ ] Create config, state, and workspace directories with owner-only permissions where required.
- [ ] Install the `acps` binary.
- [ ] Install a systemd unit for the daemon.
- [ ] Configure `ReadWritePaths` for workspace, config, and state paths.
- [ ] Add restart policy compatible with configured runtime behavior.
- [ ] Document start, stop, status, and journal inspection commands.

## Reverse Proxy Guides

- [ ] Write Nginx deployment guide.
- [ ] Write Caddy deployment guide.
- [ ] Document TLS termination at the reverse proxy.
- [ ] Document WebSocket upgrade configuration.
- [ ] Document public-edge compression policy.
- [ ] Document `trust_proxy_headers` and trusted proxy address configuration.
- [ ] Clarify that runtime HTTP hardening remains enabled behind the proxy.

## Runtime User Automation

- [ ] Validate ownership of workspace, config, state, age key, and encrypted secret store.
- [ ] Add remediation hints for incorrect ownership.
- [ ] Ensure daemon, agent, MCP servers, and mediated commands run as the runtime user.
- [ ] Keep root execution limited to explicit disposable/dev profile behavior.

## Config Import/Export Hardening

- [ ] Validate imported config paths are absolute where required.
- [ ] Reject imported config with secret values in fields that must be references.
- [ ] Add import dry-run output.
- [ ] Add export redaction checks.
- [ ] Add config compatibility version field.
- [ ] Add tests for malformed, oversized, and unsafe imports.

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
- [ ] Use `DEEPSEEK-V4-FLASH` as the default real-prompt smoke-test model when available for the selected OpenCode provider.

## Workspace Init Sources

- [ ] Replace the single-source init model with separate code and data lanes.
- [ ] Clone repositories into `/workspace/usr/code/<repo-name>/`.
- [ ] Place user data under `/workspace/usr/data/<data-dir-name>/`.
- [ ] Support local file or directory upload by CLI path before daemon startup.
- [ ] Support public Google Drive and Dropbox data as generic HTTPS archive download and extraction.
- [ ] Support S3 bucket/prefix ingestion into a derived data subfolder.
- [ ] Reject archive extraction with absolute paths, `..`, symlink escapes, unsafe formats, oversized downloads, oversized extracted output, or unsupported redirects.

## Init Testflight

- [ ] After config and secrets are present, run a full init testflight that starts the agent and sends a minimal real prompt.
- [ ] Warn that real-prompt testflight may consume provider credits and provide an explicit skip/confirmation path.
- [ ] Hard-fail unsupported init paths early: browser OAuth agents, private Drive/Dropbox links, non-archive cloud folders, unsafe archives, and missing required secrets.

## Test Hardening

- [ ] Cover the two-step install flow in `tests/agent_install_tests.rs` end-to-end against a mocked GitHub API.

## Security Self-Check

- [ ] Implement `acps security check`.
- [ ] Implement `GET /v1/security/check`.
- [ ] Check API keys exist and are not default or empty.
- [ ] Check config, state, age key, and encrypted secret store owner-only permissions.
- [ ] Check workspace is writable by the runtime user.
- [ ] Check WebSocket origin and CORS policies are configured for public deployments.
- [ ] Check recent auth failure rate is below configured thresholds.
- [ ] Check external logging sink health when enabled.

## acpctl Hardening

- [ ] Audit each `acpctl` command for permission boundaries.
- [ ] Ensure high-risk local commands cannot be self-approved unless policy explicitly allows it.
- [ ] Add durable audit records for all `acpctl` actions.
- [ ] Add tests proving `acpctl` cannot read secrets or rotate API keys.

## Acceptance

- [ ] The runtime can be deployed through Docker.
- [ ] The runtime can be deployed through systemd.
- [ ] Public deployments have documented reverse proxy configurations.
- [ ] `acps deps apply` supports only narrow, explicit installation behavior.
- [ ] Security self-checks are available through CLI and API.
- [ ] `acpctl` permission boundaries are tested and audited.
