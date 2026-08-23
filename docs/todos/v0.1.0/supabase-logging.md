# Supabase logging schema provisioning

`acps logging supabase sql --schema <name> --table-prefix <prefix>` emits DDL whose `setup_sql` creates the schema first and schema-qualifies its tables, views, function, and privilege statements (`src/runtime/logging/supabase_mirror.rs`).

- [ ] Confirm `acps logging supabase sql --schema <non-public>` produces DDL that applies cleanly against a database where that schema does not pre-exist. (Needs a live Postgres; fold into the release walkthrough.)
