# Agent Registry

`data/agents.toml` is the embedded catalog for supported ACP agents. It is intentionally separate from dependency metadata; a future dependency catalog can live in `data/deps.toml`.

## Terminology

- Agent: the product the operator chooses in `[agent].id`, such as `opencode`, `cursor`, `amp`, `pi`, or `goose`.
- Harness: the upstream agent CLI that performs the model work. Native agents use the harness as the ACP process.
- Adapter: an ACP-facing wrapper for a harness that does not speak ACP directly. Adapter-backed entries keep the real agent at top level and put the wrapper identity in `[agents.adapter]`.

## Entry Shape

Top-level metadata:

- `id`: stable `acp-stack` agent id.
- `name`: display name.
- `kind`: `native` or `adapter`.
- `headless_compatible`: true only after documented non-interactive smoke verification.
- `set_provider`: true when `acps agent set` can safely update generated agent config.
- `set_model`: true when `acps agent set` can validate and store an ACP-advertised model value.
- `set_mode`: true when `acps agent set` can validate and store an ACP-advertised mode value.
- `stdio_framing`: ACP stdio framing verified during onboarding. Supported values currently include `json-lines`, the ACP spec transport used by the Rust SDK.
- `website`: official product website.
- `github`: GitHub path shorthand for the official harness/source repository when one exists, in `owner/repo` or `owner/repo/path` form.
- `support_doc`: local support note under `docs/agents/`.

Native entries require `[agents.harness]` and no `[agents.adapter]`. Adapter entries require both `[agents.harness]` and `[agents.adapter]`.

## Install Paths

Harness installs live under `agents.harness.install.{shell,npm,github}`. Adapter installs live under `agents.adapter.install.{shell,npm,github}`.

Latest/default selection prefers `shell`, then `npm`, then `github`. Npm install entries are also the managed form for packages documented primarily as `npx <package>`, because installation still creates a stable executable in the managed bin directory. Pinned harness selection prefers `github`, then `npm@version`; shell-only entries fail because shell bootstraps are assumed to install latest.

`github` install paths derive the release repository from the first two segments of the containing object’s `github` value. Values should normally be `owner/repo`; a path suffix is allowed only when metadata needs to point at a repository subdirectory. Architecture substitutions are local to the path:

```toml
[agents.harness.install.github]
asset_pattern = "opencode-linux-{arch}.tar.gz"
archive = "tar.gz"
binary_name = "opencode"

[agents.harness.install.github.arch]
x86_64 = "x64"
aarch64 = "arm64"
```

Do not use a global arch map. OpenCode uses `x64`/`arm64`; `amp-acp` uses `x86_64`/`aarch64`.

Npm installs use the runtime-managed local prefix so executables land in `$HOME/.local/bin`, matching `creates` and launch PATH resolution.

## Sync Identity

The dev-only upstream sync check uses top-level `id` for native entries. For adapter-backed entries it uses `adapter.id`, because the upstream ACP registry tracks the ACP adapter identity while the embedded catalog’s top-level id remains the real agent.
