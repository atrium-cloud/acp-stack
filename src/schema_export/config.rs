//! The Deserialize-contract umbrella for the `acps-config.toml` schema. `Config`
//! transitively references every section struct, so registering the root here
//! pulls the whole config model into `#/$defs/config/*`.

use crate::config::Config;

#[derive(schemars::JsonSchema)]
#[allow(dead_code)]
pub(super) enum AcpsConfigTypes {
    Config(Config),
}
