-- installer_runs.log_dir records the on-disk directory holding the full
-- stdout/stderr capture for this step. The 64 KiB cap on installer_runs
-- stays — the SQLite row is a fast preview — but a complete copy lives on
-- disk so the operator can audit a failing install without re-running it.
--
-- Populated by the installer flow when the CLI / HTTP entry points pass a
-- log base path; legacy rows keep this NULL.

ALTER TABLE installer_runs ADD COLUMN log_dir TEXT;
