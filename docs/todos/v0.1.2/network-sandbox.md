# Network sandbox follow-ups

- [ ] When CI lands (deferred until after the Sprite release walkthroughs), the privileged Linux job must run `cargo test --test sandbox_network_tests --test sandbox_isolation_tests -- --ignored`; these ignored-by-default tests fail hard when required capabilities are unavailable.
