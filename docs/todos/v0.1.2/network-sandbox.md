# Network sandbox follow-ups

- [ ] When CI lands (deferred until after the Sprite release walkthroughs), the privileged Linux job must run `cargo test --test sandbox_network_tests --test sandbox_isolation_tests -- --ignored`; these ignored-by-default tests fail hard when required capabilities are unavailable.
- [ ] The same job must provision a workload user and run `ACPS_TEST_WORKLOAD_USER=<user> cargo test --all-features --test workload_identity_tests -- --ignored` plus `cargo test --all-features --lib -- --ignored` for the workload filesystem, reachability and native config identity tests.
