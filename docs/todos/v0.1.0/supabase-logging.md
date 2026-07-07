# Supabase logging schema provisioning

`acps logging supabase sql --schema <name> --table-prefix <prefix>` emits DDL for an arbitrary validated schema and table prefix. `setup_sql` currently schema-qualifies its tables, views, function, and privilege statements but assumes the target schema already exists. A non-public schema can therefore fail on a fresh database before any mirrored logging table is created.

- [x] Emit `CREATE SCHEMA IF NOT EXISTS {schema};` at the start of `setup_sql` (`src/runtime/logging/supabase_mirror.rs`) before any qualified table, view, function, grant, or revoke statement.
- [x] Add a focused SQL-generation test asserting schema creation precedes the first qualified object or privilege statement.
- [ ] Confirm `acps logging supabase sql --schema <non-public>` produces DDL that applies cleanly against a database where that schema does not pre-exist. (Needs a live Postgres; fold into the release walkthrough.)
