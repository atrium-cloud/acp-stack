-- installer_runs.agent_id scopes install rows to the configured agent, and
-- installer_runs.version records the resolved version the installer wrote.
--
-- Populated by `github_release` installs (resolved release tag) and `npm`
-- installs (resolved with `npm view <package> version --json`). Shell-recipe
-- installs leave this NULL; `acps agent check` then reports the binary as
-- `version=unknown, manual check required`.

ALTER TABLE installer_runs ADD COLUMN version TEXT;
ALTER TABLE installer_runs ADD COLUMN agent_id TEXT;
