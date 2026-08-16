//! API, security, and edge (Cloudflare) schema types.

use super::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub bind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub max_request_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityConfig {
    pub http: SecurityHttpConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityHttpConfig {
    pub max_request_bytes: u64,
    pub rate_limit_per_minute: u64,
    pub burst: u64,
    pub auth_failures_per_minute: u64,
    pub auth_block_duration: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub trust_proxy_headers: bool,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareEdgeConfig>,
}

impl EdgeConfig {
    pub fn is_empty(&self) -> bool {
        self.cloudflare.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareEdgeConfig {
    pub enabled: bool,
    pub mode: String,
    pub exposure: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_id: Option<String>,
    #[serde(default = "default_cloudflared_deployment")]
    pub cloudflared_deployment: String,
}

fn default_cloudflared_deployment() -> String {
    "host".to_owned()
}
