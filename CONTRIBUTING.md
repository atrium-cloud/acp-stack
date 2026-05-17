# Contributing to `acp-stack`

`acp-stack` is a self-hostable Linux runtime for ACP-compatible agents. 

Contributions are welcome but make sure you follow the guidelines below carefully. Not doing so may result in PR auto-closed or even account blocking.

No Contributor License Agreement is required.

## Rule No. 1

You must maintain full understanding of the changes you submit. Be ready to answer any questions regarding design choices. 

Using LLMs for code generation is acceptable. However, submitting work that is purely LLM-generated and cannot be explained, validated, or maintained is not. PRs that read as unvetted output without human expertise, effort, and judgement may be closed without review.

We also advise against including LLM-generated text in your issue text or responses. This may result in PR auto-close or account blocking for repeated offenders.

## Always Link to Issues

All PRs must reference an existing issue. Open the issue before implementation so maintainers can confirm the problem, scope, and likely acceptance criteria.

Small fixes can use a short issue. Larger work should wait for maintainer agreement before implementation, especially agent support, CLI features, core runtime features, security-sensitive changes, dependency changes, and user-facing behavior changes.

## Accepted Contributions

Use the linked issue to make the contribution type clear:

- Agent support: you may propose candidates in issues/PRs. Maintainers may evaluate and add support for the suggested agent in due time. A candidate must include official docs, non-interactive API-key auth, headless config, ACP launch, and any install/config or prompt-test evidence available.
- Bug fix: explain the observed behavior, expected behavior, root cause, fix, and verification. Add a regression test when practical.
- CLI feature: describe the command shape, output behavior, errors, docs updates, and tests. CLI behavior should use the same core service path as the HTTP/API runtime where practical.
- Core feature: discuss first. Changes to config, state, secrets, permissions, API behavior, ACP bridge behavior, or the supervisor need docs, tests, and migration/security consideration where relevant.
- Documentation: clarify existing behavior. If docs describe changed behavior, update code/tests in the same PR.

We kindly decline standalone PRs relating to codebase organization, architecture, and maintenance, as these are maintainers' duty. 

Please open an issue if you believe clarity needs to be improved in specific areas, or if any focused, localized changes should be made.

## Pull Request Expectations

Keep PRs small and focused. Explain what changed, why it changed, and how you verified it.

We recommend PR descriptions to be short and human-written. There is no need for long paragraphs of exposition or verbose explanation. Keep your message and request succinct.

Use conventional PR titles:

- `fix:` for bug fixes
- `feat:` for new functionality
- `docs:` for documentation-only changes
- `test:` for tests

Dependency changes are only acceptable as part of a bug fix or feature.

## Checks

Run focused tests while developing.

Before asking for review, run the checks that match your change:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

For registry changes, also run:

```bash
cargo run --bin sync-registry-check
```

If PR involves dependency changes, run `cargo audit` if possible.

Docs changes must be carefully checked for typos, grammatical errors, and verbosity. Avoid looping descriptions.

## Documentation

Update docs with the code change when behavior or project structure changes. New runtime modules or module-boundary changes belong in `docs/mgmt/architecture.md`; new dependencies belong in `docs/mgmt/tech-stack.md`; user-facing behavior belongs under `docs/specs/`; todo-backed work should update the matching checklist under `docs/todos/`.

## Security

Do not include secrets, local tokens, account cookies, browser sessions, or `.env.*` contents in issues, PRs, tests, screenshots, or logs.

`acp-stack` is designed around explicit secret references and headless API-key flows. Do not add support paths that depend on browser OAuth sessions or interactive login unless maintainers have explicitly accepted that design first.
