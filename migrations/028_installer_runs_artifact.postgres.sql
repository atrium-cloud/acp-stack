-- installer_runs.path and sha256 name the binary a finished step left in place,
-- so a later install can tell a binary acp-stack installed or kept from one it
-- did not. Rows from before this migration carry neither and read as foreign.
ALTER TABLE installer_runs ADD COLUMN path text;
ALTER TABLE installer_runs ADD COLUMN sha256 text;
