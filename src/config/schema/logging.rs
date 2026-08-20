//! Local and Supabase logging schema types.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: String,
    pub local_retention_days: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supabase: Option<SupabaseLoggingConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SupabaseLoggingConfig {
    pub enabled: bool,
    #[serde(default = "default_supabase_backend")]
    pub backend: SupabaseLoggingBackend,
    pub url: String,
    #[serde(default = "default_supabase_table_prefix")]
    pub table_prefix: String,
    #[serde(
        default = "default_supabase_db_url_ref",
        skip_serializing_if = "Option::is_none"
    )]
    pub db_url_ref: Option<String>,
    pub api_key_ref: String,
    pub schema: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SupabaseLoggingBackend {
    Postgrest,
    Postgres,
}

fn default_supabase_backend() -> SupabaseLoggingBackend {
    SupabaseLoggingBackend::Postgrest
}

fn default_supabase_table_prefix() -> String {
    String::new()
}

fn default_supabase_db_url_ref() -> Option<String> {
    None
}
