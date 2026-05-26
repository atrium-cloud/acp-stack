# Contributing to `acp-stack`

`acp-stack` is a self-hostable Linux runtime for ACP-compatible agents.

External PR contributions are paused for the foreseeable future. Maintainers do not currently have enough review capacity to handle PRs responsibly.

If you have a feature/fix you strongly believe should be added, reach out to the author directly and share your findings. Once reviewed, you may be added to the collaborators list.

The repository remains licensed under Apache 2.0. Anyone who wants to build on the project can fork it.

## Issues

Issues remain welcome when they are focused and actionable. Please describe the observed behavior, expected behavior, relevant environment details, and any reproduction steps.

Agent support suggestions are also welcome as issues. Include official docs, non-interactive API-key auth details, headless config, ACP launch notes, and any install/config or prompt-test evidence available.

## Security

Do not include secrets, local tokens, account cookies, browser sessions, or `.env.*` contents in issues, tests, screenshots, or logs.

`acp-stack` is designed around explicit secret references and headless API-key flows. Do not propose support paths that depend on browser OAuth sessions or interactive login unless maintainers have explicitly accepted that design first.
