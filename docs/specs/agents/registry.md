# Agent Registry

The embedded agent registry defines which agents `acp-stack` can install and launch without requiring an external catalog lookup.

## Entry Shape

Registry entries describe:

- `id` and display name
- whether the agent is headless-compatible
- native command or adapter-backed command
- install steps and post-install executable checks
- provider/model/mode support flags
- MCP and Agent Skills support flags
- support documentation path

Only entries marked headless-compatible are offered as supported runtime targets.

## Install Paths

Install metadata may describe shell, npm, or GitHub Release sources. Native agents have one install step. Adapter-backed agents have a harness step and an adapter step.

The installer verifies declared executables after each managed step. Provider secrets are never passed to install steps.

## Operator Override

The embedded registry is the default source. Operators may provide a local override catalog for their instance, but unsupported entries remain outside the project's support guarantee.

## Skills Catalog

Agent Skills sources are cataloged separately in `data/skills.toml`. The skills
catalog records trusted official source directories only; it does not affect
agent installation or runtime launch behavior.
