//! User-declared Agent Skills sources.

use super::*;

/// User-declared Agent Skills sources (managed via `acps skills source add`),
/// layered alongside the embedded curated catalog. Unlike the catalog these
/// are operator-supplied and untrusted by default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillsConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<UserSkillSource>,
}

impl SkillsConfig {
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserSkillSource {
    /// Unique alias used by `acps skills add <alias> ...` and `source get`.
    pub alias: String,
    /// GitHub source as `owner/repo`.
    pub github: String,
    #[serde(default = "default_user_skill_branch")]
    pub branch: String,
    /// Operator assertion that the source has been vetted. Defaults false;
    /// installs from untrusted sources are allowed but surfaced as such.
    #[serde(default)]
    pub trusted: bool,
}

/// Default branch for user-declared and ad-hoc skill sources. Shared by the
/// serde default, the `skills sources add` route, and ad-hoc `github:` refs so
/// all three always resolve the same ref.
pub const DEFAULT_SKILL_SOURCE_BRANCH: &str = "main";

fn default_user_skill_branch() -> String {
    DEFAULT_SKILL_SOURCE_BRANCH.to_owned()
}
