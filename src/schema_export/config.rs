//! The Deserialize-contract umbrella for the `acps-config.toml` schema:
//! registering the `Config` root pulls every section into `#/$defs/config/*`.

use crate::config::Config;

#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
pub(super) enum AcpsConfigTypes {
    Config(Config),
}
