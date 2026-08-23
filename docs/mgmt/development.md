# Development Notes

This document is for maintainers and (future) contributors.

## Documentation Rules

- Keep `README.md`, `docs/deploy/*`, and `docs/agents/*` operator-facing.
- Keep `docs/specs/*` focused on contracts: commands, routes, fields, auth tiers, limits, errors, and invariants.
- Do not put CI workflow names, test harness details, source-file inventories, or migration history in user/operator docs.
- Put maintainer-only verification and implementation notes here or in another `docs/mgmt/*` document.

## Verification Commands

Use Rust `1.95.0`, matching `rust-toolchain.toml`. Default Cargo commands build the production-shaped target set. Development commands and fixtures require explicit features:

```sh
cargo test
cargo test --features dev-tools,test-fixtures
cargo clippy --all-targets
cargo clippy --all-targets --features dev-tools,test-fixtures
```

For code changes, use the repository's `cargo` checks and run the pre-commit hook before commit. For doc-only changes, run the link/leak checks below.

```sh
rg -n "tests/|\\.github|Phase [0-9]|migration|src/" README.md docs/specs docs/deploy docs/agents
rg -n "\\[[^]]+\\]\\(([^)#]+)\\)" README.md docs
```

The first check flags maintainer/internal language that has leaked into operator docs or stable specs. Review any hit there. Hits inside `docs/mgmt/` are expected.

## Test Scripts

Repository test scripts are maintainer tools, not deployment instructions:

- `scripts/docker-test.sh` validates the Docker image startup path.
- `scripts/install-systemd-test.sh` validates the systemd installer path in a containerized systemd environment. Its default image is built from `packaging/systemd/installer-test.Dockerfile` so `/sbin/init` exists before the container boots.

## Dev Builds

The manual `dev-build` workflow uploads release-shaped Linux tarballs as Actions artifacts. It runs `scripts/build-release.sh --no-default-features`, so those binaries omit `acps update`. Replace dev deployments manually from the artifact. `install.sh` remains tied to public GitHub Releases.

## Public Releases

Release tagging, changelog, and publishing rules live in the `.rules` file under "Release workflows".

## Placebo Agent

`placebo-agent` is a deterministic ACP fixture for integration tests. It is compiled only with `--features test-fixtures`. Tests invoke it through `CARGO_BIN_EXE_placebo-agent` with the `acp` subcommand, so the bundled fixture binary is their only agent-side dependency.

The fixture serves tests exclusively and sits outside the supported agent set. Binary release packaging must continue to bundle only the runtime binary, `acps`.

## Dev Commands

`acps dev ...` and hidden bypass flags are compiled only with `--features dev-tools`. Use this path for local maintainer loops such as `acps dev init --skip-workspace-init` or `acps dev serve --allow-root`. Default builds exclude those commands.

## Local Socket Coupling

The internal local socket is intentionally allowlisted. Any change to daemon-backed keyless local `acps` routes must keep these in sync:

- the local socket router
- deny-list coverage for high-risk routes
