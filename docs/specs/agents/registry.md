# Agent Registry

The embedded agent registry defines which agents `acp-stack` can install and launch from embedded data alone.

## Entry Shape

Registry entries describe:

- `id` and display name
- whether the agent is headless-compatible
- native command or adapter-backed command
- optional `harness.acp_args`, used when the harness enters ACP mode through something other than an `acp` subcommand (e.g. a `--acp` flag)
    - must be non-empty; defaults to `["acp"]`
- install steps and post-install executable checks
- provider/model/mode/effort support flags
- Agent Skills support flag
- Agent Skills install directory when skills are supported
- optional Agent Skills link directory, used when the harness discovers skills somewhere other than the install directory (e.g. Claude Code's `~/.claude/skills`)
    - must not equal the install directory; neither may nest within the other
- support documentation path

Only headless-compatible entries are offered as supported runtime targets.

The registry carries no MCP or other ACP capability declarations. Those come from the agent's live `initialize` advertisement, captured by the init capability probe and on every agent start.

## Install Paths

Install metadata may describe shell, npm, or GitHub Release sources.

- Native agents have one install step.
- Adapter-backed agents have a harness step and an adapter step, unless `[agents.harness.install] provided_by = "adapter"` declares that the adapter package supplies the harness.

An operator config's `[agent.adapter_override]` (see [config.md](../config.md)) rewrites the effective entry at resolution time. Kind becomes adapter and the adapter spec comes from the override. Everything else stays as the catalog declares it.

Tool requirements per source:

- Shell paths declare `required_tools` for external commands they invoke.
- Npm paths require `npm`.
- GitHub Release paths use the runtime downloader and require no host fetch tools.

The installer preflights declared paths and uses a fallback path when one is available.

### Timeouts

One budget covers a shell install path's whole run: fetch, upstream installer, and any follow-up work.

- The default is 600s.
- Shell paths may declare `timeout_secs` to override it, for recipes that cannot fit the default (e.g. an upstream installer that provisions a language toolchain).
- `timeout_secs` must be positive. Omitting it keeps the default.
- Npm and GitHub Release paths always use the default.

The installer verifies declared executables after each managed step. Provider secrets stay out of install steps.

### Updates

Updates follow the recorded install method:

- Npm and GitHub Release paths resolve a latest version.
- Apt uses the declared package.
- Shell-installed harnesses are probed for a native `update`/`upgrade` subcommand.

A shell-installed harness with no such subcommand and no other channel may declare `update.shell_rerun = true`. The update step then re-runs the shell install recipe. The recipe is responsible for exiting cheaply when nothing changed.

### Upstream Registry Sync

`sync_id` and `sync_exempt` are maintainer-only fields for the ACP registry comparison. Neither has runtime effect.

- Adapter `sync_id` is a comparison alias for adapters whose upstream registry id differs from the local launch command.
- Native entries may declare the same alias at the entry level (entry `sync_id`) when upstream names the agent differently than the installed binary.
- Resolution: adapter entries use adapter `sync_id`, else the adapter id. Native entries use entry `sync_id`, else the entry id.
- Entry-level `sync_exempt` marks agents the ACP project documents but the upstream registry index does not list yet.
- `sync-registry-check` skips the upstream-existence requirement for exempt entries while still reporting them.
- Remove `sync_exempt` once the upstream index carries the id.

## Operator Override

The embedded registry is the default source. Operators may provide a local override catalog for their instance. The project's support guarantee covers supported entries only.

## Skills Catalog

Skill sources are cataloged separately. See [skills.md](skills.md).
