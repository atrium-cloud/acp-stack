-- installer_runs.step records which install layer produced the row.
--
-- For native ACP agents and the legacy shell-recipe escape hatch there is
-- one row per install with step = 'install'. For adapter-backed agents the
-- runtime writes two rows per install: step = 'harness' for the underlying
-- agent binary, then step = 'adapter' for the ACP-speaking wrapper. The
-- default keeps any pre-rework rows readable as one-step installs.

ALTER TABLE installer_runs ADD COLUMN step TEXT NOT NULL DEFAULT 'install';
