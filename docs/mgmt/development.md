# Development Notes

This document is for maintainers and (future) contributors.

## Documentation Rules

- Keep `README.md`, `docs/deploy/*`, and `docs/agents/*` operator-facing.
- Keep `docs/specs/*` focused on contracts: commands, routes, fields, auth tiers, limits, errors, and invariants.
- Do not put CI workflow names, test harness details, source-file inventories, or migration history in user/operator docs.
- Put maintainer-only verification and implementation notes here or in another `docs/mgmt/*` document.

## Verification Commands

For code changes, use the repository's `cargo` checks (build, test, fmt, clippy) and run the pre-commit hook before commit. For doc-only changes, run the link/leak checks below.

```sh
rg -n "tests/|\\.github|Phase [0-9]|migration|src/" README.md docs/specs docs/deploy docs/agents
rg -n "\\[[^]]+\\]\\(([^)#]+)\\)" README.md docs
```

The first check flags maintainer/internal language that has leaked into operator docs or stable specs — review any hit there. Hits inside `docs/mgmt/` are expected.

## Test Scripts

Repository test scripts are maintainer tools, not deployment instructions:

- `scripts/docker-test.sh` validates the Docker image startup path.
- `scripts/install-systemd-test.sh` validates the systemd installer path in a containerized systemd environment. Its default image is built from `packaging/systemd/installer-test.Dockerfile` so `/sbin/init` exists before the container boots.

## Local Interface Coupling

`acpctl` is intentionally allowlisted. Any change to the local command surface must keep these in sync:

- the documented allowlist in `docs/specs/acpctl/acpctl.md`
- the local socket router
- the MCP tool facade
- deny-list coverage for high-risk routes
