# Agent Registry

The embedded agent registry defines which agents `acp-stack` can install and launch without requiring an external catalog lookup.

## Entry Shape

Registry entries describe:

- `id` and display name
- whether the agent is headless-compatible
- native command or adapter-backed command
- optional `harness.acp_args` when the harness enters ACP mode through something other than an `acp` subcommand (e.g. a harness launched with a `--acp` flag); it must be non-empty and defaults to `["acp"]`
- install steps and post-install executable checks
- provider/model/mode/effort support flags
- Agent Skills support flag
- Agent Skills install directory when skills are supported
- optional Agent Skills link directory when the harness discovers skills somewhere other than the install directory (e.g. Claude Code's `~/.claude/skills`); it must not equal the install directory, and neither may nest within the other
- support documentation path

Only entries marked headless-compatible are offered as supported runtime targets.

The registry carries no MCP or other ACP capability declarations. Those come from the agent's live `initialize` advertisement, captured by the init capability probe and on every agent start.

## Install Paths

Install metadata may describe shell, npm, or GitHub Release sources. Native agents have one install step. Adapter-backed agents have a harness step and an adapter step unless `[agents.harness.install] provided_by = "adapter"` declares that the adapter package supplies the harness.

Shell install paths declare `required_tools` for external commands they invoke. Npm install paths require `npm`. GitHub Release install paths use the runtime downloader and do not require host fetch tools. The installer preflights declared paths and uses a fallback path when one is available.

One budget covers a shell install path's whole run — fetch, upstream installer, and any follow-up work. Shell install paths may declare `timeout_secs` to override the 600s default for recipes that cannot fit it, such as one whose upstream installer provisions a language toolchain; the field must be positive, and omitting it keeps the default. Npm and GitHub Release paths always use the default.

The installer verifies declared executables after each managed step. Provider secrets are never passed to install steps.

Updates follow the recorded install method: npm and GitHub Release paths resolve a latest version, apt uses the declared package, and shell-installed harnesses are probed for a native `update`/`upgrade` subcommand. A shell-installed harness with no such subcommand and no other channel may declare `update.shell_rerun = true` instead; the update step then re-runs the shell install recipe, and the recipe is responsible for exiting cheaply when nothing changed.

Adapter `sync_id` is a maintainer-only ACP registry comparison alias for adapters whose upstream registry id differs from the local launch command. Native entries may declare the same alias at the entry level (entry `sync_id`) when upstream names the agent differently than the installed binary; adapter entries resolve from the adapter block alone (`sync_id`, else adapter id), native entries from entry `sync_id`, else the entry id. Entry-level `sync_exempt` is a maintainer-only flag for agents the ACP project documents but the upstream registry index does not list yet; `sync-registry-check` skips the upstream-existence requirement for exempt entries while reporting them, and the flag has no runtime effect. Remove it once the upstream index carries the id.

## Operator Override

The embedded registry is the default source. Operators may provide a local override catalog for their instance, but unsupported entries remain outside the project's support guarantee.

## Skills Catalog

Agent Skills sources are cataloged separately in `data/skills.toml`. During `acps init`, selected skills are copied into the selected agent's `agent_skills_install_dir`. When the agent declares an `agent_skills_link_dir`, each installed skill is additionally symlinked there; the link refresh is best-effort and also prunes dangling links left by removed skills. Linking is a one-way mirror: the install directory is the source of truth and the link directory only receives symlinks — symlinks pointing into the install directory are managed wherever they sit (the refresh recurses into group directories and removes a directory left empty by pruning), while everything else in the link directory is user-owned and left untouched. Nested skills are linked under group directories, so a managed link may be added inside a pre-existing directory of the same name.
