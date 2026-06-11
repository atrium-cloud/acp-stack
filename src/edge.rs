use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::CloudflareEdgeConfig;
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only};

const CLOUDFLARE_API_BASE_URL: &str = "https://api.cloudflare.com/client/v4";
const CLOUDFLARE_HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const CLOUDFLARE_DEFAULT_TUNNEL_NAME: &str = "acp-stack";
const CLOUDFLARE_TUNNEL_DNS_SUFFIX: &str = "cfargotunnel.com";
const CLOUDFLARE_TUNNEL_TOKEN_ENV_FILE: &str = "tunnel-token.env";
const CLOUDFLARE_TUNNEL_TOKEN_ENV_NAME: &str = "CLOUDFLARE_TUNNEL_TOKEN";

#[derive(Debug, Clone)]
pub struct GeneratedCloudflareArtifact {
    pub label: &'static str,
    pub path: PathBuf,
}

pub fn write_cloudflare_artifacts(
    config_dir: &Path,
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
) -> Result<Vec<GeneratedCloudflareArtifact>> {
    write_cloudflare_artifacts_inner(config_dir, cloudflare, service_url, None)
}

fn write_managed_cloudflare_artifacts(
    config_dir: &Path,
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    tunnel_token: &str,
) -> Result<Vec<GeneratedCloudflareArtifact>> {
    write_cloudflare_artifacts_inner(config_dir, cloudflare, service_url, Some(tunnel_token))
}

fn write_cloudflare_artifacts_inner(
    config_dir: &Path,
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    tunnel_token: Option<&str>,
) -> Result<Vec<GeneratedCloudflareArtifact>> {
    let dir = config_dir.join("cloudflared");
    create_dir_owner_only(&dir)?;
    let token_env_path = tunnel_token.map(|_| dir.join(CLOUDFLARE_TUNNEL_TOKEN_ENV_FILE));
    let artifacts = [
        (
            "cloudflared config",
            dir.join("config.yml"),
            render_config_yml(cloudflare, service_url),
        ),
        (
            "cloudflared systemd snippet",
            dir.join("acp-stack-cloudflared.service"),
            render_systemd_unit(&dir.join("config.yml"), token_env_path.as_deref()),
        ),
        (
            "cloudflared docker compose snippet",
            dir.join("docker-compose.yml"),
            render_docker_compose(cloudflare, service_url, token_env_path.is_some()),
        ),
        (
            "cloudflared operator checklist",
            dir.join("README.md"),
            render_checklist(cloudflare, service_url, token_env_path.is_some()),
        ),
    ];
    let mut written = Vec::with_capacity(artifacts.len());
    for (label, path, content) in artifacts {
        atomic_write_owner_only(&path, content.as_bytes())?;
        written.push(GeneratedCloudflareArtifact { label, path });
    }
    if let (Some(path), Some(token)) = (token_env_path, tunnel_token) {
        let content = render_tunnel_token_env(token)?;
        atomic_write_owner_only(&path, content.as_bytes())?;
        written.push(GeneratedCloudflareArtifact {
            label: "cloudflared tunnel token env",
            path,
        });
    }
    Ok(written)
}

pub fn ensure_managed_cloudflare_tunnel(
    cloudflare: &mut CloudflareEdgeConfig,
    api_token: &str,
    account_id: &str,
) -> Result<bool> {
    ensure_managed_cloudflare_tunnel_with_base(
        cloudflare,
        api_token,
        account_id,
        CLOUDFLARE_API_BASE_URL,
    )
}

fn ensure_managed_cloudflare_tunnel_with_base(
    cloudflare: &mut CloudflareEdgeConfig,
    api_token: &str,
    account_id: &str,
    api_base_url: &str,
) -> Result<bool> {
    let client = reqwest::blocking::Client::builder()
        .timeout(CLOUDFLARE_HTTP_TIMEOUT)
        .build()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "client setup",
            reason: source.to_string(),
        })?;
    let tunnel_name = cloudflare
        .tunnel_name
        .as_deref()
        .unwrap_or(CLOUDFLARE_DEFAULT_TUNNEL_NAME);
    if cloudflare.tunnel_id.is_some() {
        return Ok(false);
    }
    let created =
        create_cloudflare_tunnel(&client, api_base_url, api_token, account_id, tunnel_name)?;
    cloudflare.tunnel_id = Some(created);
    Ok(true)
}

pub fn finish_managed_cloudflare_provisioning(
    config_dir: &Path,
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    api_token: &str,
    account_id: &str,
) -> Result<Vec<GeneratedCloudflareArtifact>> {
    finish_managed_cloudflare_provisioning_with_base(
        config_dir,
        cloudflare,
        service_url,
        api_token,
        account_id,
        CLOUDFLARE_API_BASE_URL,
    )
}

fn finish_managed_cloudflare_provisioning_with_base(
    config_dir: &Path,
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    api_token: &str,
    account_id: &str,
    api_base_url: &str,
) -> Result<Vec<GeneratedCloudflareArtifact>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(CLOUDFLARE_HTTP_TIMEOUT)
        .build()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "client setup",
            reason: source.to_string(),
        })?;
    let tunnel_id = cloudflare
        .tunnel_id
        .as_deref()
        .ok_or(StackError::MissingField {
            field: "edge.cloudflare.tunnel_id",
        })?;
    let tunnel_token =
        get_cloudflare_tunnel_token(&client, api_base_url, api_token, account_id, tunnel_id)?;
    put_cloudflare_tunnel_config(
        &client,
        api_base_url,
        api_token,
        account_id,
        tunnel_id,
        &cloudflare.hostname,
        service_url,
    )?;
    put_cloudflare_dns_record(
        &client,
        api_base_url,
        api_token,
        &cloudflare.hostname,
        tunnel_id,
    )?;
    write_managed_cloudflare_artifacts(config_dir, cloudflare, service_url, &tunnel_token)
}

fn get_cloudflare_tunnel_token(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    account_id: &str,
    tunnel_id: &str,
) -> Result<String> {
    let url = cloudflare_url(
        api_base_url,
        &format!(
            "/accounts/{}/cfd_tunnel/{}/token",
            path_component(account_id)?,
            path_component(tunnel_id)?
        ),
    );
    let response = client
        .get(url)
        .bearer_auth(api_token)
        .send()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "get tunnel token",
            reason: source.to_string(),
        })?;
    let value = cloudflare_json(response, "get tunnel token")?;
    value
        .get("result")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| StackError::CloudflareManagedProvision {
            operation: "get tunnel token",
            reason: "response did not include result token".to_owned(),
        })
}

fn put_cloudflare_dns_record(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    hostname: &str,
    tunnel_id: &str,
) -> Result<()> {
    let zone_id = resolve_cloudflare_zone_id(client, api_base_url, api_token, hostname)?;
    let existing_record_id =
        find_cloudflare_dns_record_id(client, api_base_url, api_token, &zone_id, hostname)?;
    let tunnel_target = format!("{tunnel_id}.{CLOUDFLARE_TUNNEL_DNS_SUFFIX}");
    let payload = serde_json::json!({
        "type": "CNAME",
        "name": hostname,
        "content": tunnel_target,
        "proxied": true,
        "comment": "acp-stack managed Cloudflare Tunnel"
    });
    let path = match existing_record_id.as_deref() {
        Some(record_id) => format!(
            "/zones/{}/dns_records/{}",
            path_component(&zone_id)?,
            path_component(record_id)?
        ),
        None => format!("/zones/{}/dns_records", path_component(&zone_id)?),
    };
    let url = cloudflare_url(api_base_url, &path);
    let request = match existing_record_id {
        Some(_) => client.put(url),
        None => client.post(url),
    };
    let response = request
        .bearer_auth(api_token)
        .json(&payload)
        .send()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "configure DNS record",
            reason: source.to_string(),
        })?;
    cloudflare_json(response, "configure DNS record").map(|_| ())
}

fn resolve_cloudflare_zone_id(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    hostname: &str,
) -> Result<String> {
    for candidate in zone_name_candidates(hostname) {
        let url = cloudflare_url(api_base_url, &format!("/zones?name={candidate}"));
        let response = client
            .get(url)
            .bearer_auth(api_token)
            .send()
            .map_err(|source| StackError::CloudflareManagedProvision {
                operation: "resolve DNS zone",
                reason: source.to_string(),
            })?;
        let value = cloudflare_json(response, "resolve DNS zone")?;
        if let Some(zone_id) = value
            .get("result")
            .and_then(serde_json::Value::as_array)
            .and_then(|zones| zones.first())
            .and_then(|zone| zone.get("id"))
            .and_then(serde_json::Value::as_str)
        {
            return Ok(zone_id.to_owned());
        }
    }
    Err(StackError::CloudflareManagedProvision {
        operation: "resolve DNS zone",
        reason: format!("no Cloudflare zone found for hostname `{hostname}`"),
    })
}

fn zone_name_candidates(hostname: &str) -> Vec<String> {
    let labels: Vec<&str> = hostname.split('.').collect();
    if labels.len() < 2 {
        return vec![hostname.to_owned()];
    }
    (0..labels.len() - 1)
        .map(|index| labels[index..].join("."))
        .collect()
}

fn find_cloudflare_dns_record_id(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    zone_id: &str,
    hostname: &str,
) -> Result<Option<String>> {
    let url = cloudflare_url(
        api_base_url,
        &format!(
            "/zones/{}/dns_records?type=CNAME&name={hostname}",
            path_component(zone_id)?
        ),
    );
    let response = client
        .get(url)
        .bearer_auth(api_token)
        .send()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "find DNS record",
            reason: source.to_string(),
        })?;
    let value = cloudflare_json(response, "find DNS record")?;
    Ok(value
        .get("result")
        .and_then(serde_json::Value::as_array)
        .and_then(|records| records.first())
        .and_then(|record| record.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

fn create_cloudflare_tunnel(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    account_id: &str,
    tunnel_name: &str,
) -> Result<String> {
    let url = cloudflare_url(
        api_base_url,
        &format!("/accounts/{}/cfd_tunnel", path_component(account_id)?),
    );
    let response = client
        .post(url)
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "name": tunnel_name,
            "config_src": "cloudflare"
        }))
        .send()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "create tunnel",
            reason: source.to_string(),
        })?;
    let value = cloudflare_json(response, "create tunnel")?;
    value
        .get("result")
        .and_then(|result| result.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| StackError::CloudflareManagedProvision {
            operation: "create tunnel",
            reason: "response did not include result.id".to_owned(),
        })
}

fn put_cloudflare_tunnel_config(
    client: &reqwest::blocking::Client,
    api_base_url: &str,
    api_token: &str,
    account_id: &str,
    tunnel_id: &str,
    hostname: &str,
    service_url: &str,
) -> Result<()> {
    let url = cloudflare_url(
        api_base_url,
        &format!(
            "/accounts/{}/cfd_tunnel/{}/configurations",
            path_component(account_id)?,
            path_component(tunnel_id)?
        ),
    );
    let response = client
        .put(url)
        .bearer_auth(api_token)
        .json(&serde_json::json!({
            "config": {
                "ingress": [
                    { "hostname": hostname, "service": service_url },
                    { "service": "http_status:404" }
                ]
            }
        }))
        .send()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation: "configure tunnel",
            reason: source.to_string(),
        })?;
    cloudflare_json(response, "configure tunnel").map(|_| ())
}

fn cloudflare_json(
    response: reqwest::blocking::Response,
    operation: &'static str,
) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .map_err(|source| StackError::CloudflareManagedProvision {
            operation,
            reason: source.to_string(),
        })?;
    if !status.is_success() {
        return Err(StackError::CloudflareApiStatus {
            operation,
            status: status.as_u16(),
            body,
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|source| StackError::CloudflareManagedProvision {
            operation,
            reason: format!("response was not JSON: {source}"),
        })?;
    if value
        .get("success")
        .and_then(serde_json::Value::as_bool)
        .is_some_and(|success| !success)
    {
        return Err(StackError::CloudflareManagedProvision {
            operation,
            reason: format!("API response reported success=false: {body}"),
        });
    }
    Ok(value)
}

fn cloudflare_url(api_base_url: &str, path: &str) -> String {
    format!("{}{}", api_base_url.trim_end_matches('/'), path)
}

fn path_component(value: &str) -> Result<&str> {
    if value.trim().is_empty()
        || value.contains('/')
        || value.contains('?')
        || value.contains('#')
        || value.chars().any(char::is_whitespace)
    {
        return Err(StackError::CloudflareManagedProvision {
            operation: "build request URL",
            reason: "Cloudflare API path component contains unsupported characters".to_owned(),
        });
    }
    Ok(value)
}

fn render_tunnel_token_env(tunnel_token: &str) -> Result<String> {
    if tunnel_token.trim().is_empty()
        || tunnel_token.chars().any(char::is_whitespace)
        || tunnel_token.chars().any(char::is_control)
    {
        return Err(StackError::CloudflareManagedProvision {
            operation: "write tunnel token env",
            reason: "Cloudflare tunnel token contains unsupported characters".to_owned(),
        });
    }
    Ok(format!(
        "{CLOUDFLARE_TUNNEL_TOKEN_ENV_NAME}={tunnel_token}\n"
    ))
}

fn render_config_yml(cloudflare: &CloudflareEdgeConfig, service_url: &str) -> String {
    let mut out = String::new();
    if cloudflare.mode == "managed" {
        out.push_str("# Remote-managed Cloudflare Tunnel.\n");
        out.push_str("# Run cloudflared with the generated tunnel-token.env file.\n");
        if let Some(tunnel_id) = cloudflare.tunnel_id.as_deref() {
            out.push_str(&format!("# tunnel: {tunnel_id}\n"));
        }
        out.push('\n');
    } else if let Some(tunnel_id) = cloudflare.tunnel_id.as_deref() {
        out.push_str(&format!("tunnel: {tunnel_id}\n"));
        out.push_str("credentials-file: /etc/cloudflared/");
        out.push_str(tunnel_id);
        out.push_str(".json\n\n");
    } else if let Some(tunnel_name) = cloudflare.tunnel_name.as_deref() {
        out.push_str(&format!("# tunnel: {tunnel_name}\n"));
        out.push_str("# Set the tunnel UUID and credentials-file after creating the tunnel.\n\n");
    } else {
        out.push_str("# Set tunnel and credentials-file after creating the tunnel.\n\n");
    }
    out.push_str("ingress:\n");
    out.push_str(&format!("  - hostname: {}\n", cloudflare.hostname));
    out.push_str(&format!("    service: {service_url}\n"));
    out.push_str("  - service: http_status:404\n");
    out
}

fn render_systemd_unit(config_path: &Path, token_env_path: Option<&Path>) -> String {
    if let Some(token_env_path) = token_env_path {
        return format!(
            r#"[Unit]
Description=Cloudflare Tunnel for acp-stack
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
EnvironmentFile={}
ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate run --token ${{{}}}
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
"#,
            token_env_path.display(),
            CLOUDFLARE_TUNNEL_TOKEN_ENV_NAME
        );
    }
    format!(
        r#"[Unit]
Description=Cloudflare Tunnel for acp-stack
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/cloudflared tunnel --config {} run
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=multi-user.target
"#,
        config_path.display()
    )
}

fn render_docker_compose(
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    managed_token_env: bool,
) -> String {
    let tunnel_name = cloudflare.tunnel_name.as_deref().unwrap_or("acp-stack");
    let env_file = if managed_token_env {
        "    env_file:\n      - ./tunnel-token.env\n"
    } else {
        ""
    };
    let environment = if managed_token_env {
        ""
    } else {
        "    environment:\n      CLOUDFLARE_TUNNEL_TOKEN: ${CLOUDFLARE_TUNNEL_TOKEN}\n"
    };
    format!(
        r#"services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    entrypoint: ["/bin/sh", "-c"]
    command: exec cloudflared tunnel --no-autoupdate run --token "$$CLOUDFLARE_TUNNEL_TOKEN"
    restart: unless-stopped
{env_file}
{environment}
    # Tunnel `{tunnel_name}` should publish {hostname} to {service_url}.
"#,
        hostname = cloudflare.hostname,
    )
}

fn render_checklist(
    cloudflare: &CloudflareEdgeConfig,
    service_url: &str,
    managed_token_env: bool,
) -> String {
    let tunnel_name = cloudflare.tunnel_name.as_deref().unwrap_or("acp-stack");
    if managed_token_env {
        let tunnel_target = cloudflare
            .tunnel_id
            .as_deref()
            .map(|tunnel_id| format!("{tunnel_id}.{CLOUDFLARE_TUNNEL_DNS_SUFFIX}"))
            .unwrap_or_else(|| format!("<tunnel-id>.{CLOUDFLARE_TUNNEL_DNS_SUFFIX}"));
        return format!(
            r#"# Cloudflare Tunnel checklist

1. Install `cloudflared` on the host or run the generated Docker Compose service.
2. Use `tunnel-token.env` with the generated systemd or Docker Compose snippet.
3. Verify `{hostname}` is a proxied CNAME to `{tunnel_target}`.
4. Keep `acps` bound to loopback and verify `acps security check` has no Cloudflare findings.
5. Optionally enable the Cloudflare managed transform that adds visitor location headers.

Managed provisioning stores the tunnel token only in the owner-only `tunnel-token.env` artifact, not in `acps-config.toml`.
"#,
            hostname = cloudflare.hostname,
        );
    }
    format!(
        r#"# Cloudflare Tunnel checklist

1. Install `cloudflared` on the host or run the generated Docker Compose service.
2. Create a Cloudflare Tunnel named `{tunnel_name}`.
3. Publish `{hostname}` to `{service_url}`.
4. Enable WebSockets for the zone if it was disabled.
5. Keep `acps` bound to loopback and verify `acps security check` has no Cloudflare findings.
6. Optionally enable the Cloudflare managed transform that adds visitor location headers.

This generated profile does not store Cloudflare API tokens or tunnel tokens in `acps-config.toml`.
"#,
        hostname = cloudflare.hostname,
    )
}

pub fn service_url_from_bind(bind: &str) -> Result<String> {
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|_| StackError::InvalidSocketAddress { field: "api.bind" })?;
    let host = if addr.ip().is_ipv6() {
        format!("[{}]", addr.ip())
    } else {
        addr.ip().to_string()
    };
    Ok(format!("http://{host}:{}", addr.port()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;

    fn cloudflare() -> CloudflareEdgeConfig {
        CloudflareEdgeConfig {
            enabled: true,
            mode: "generated".to_owned(),
            exposure: "tunnel".to_owned(),
            hostname: "agent.example.com".to_owned(),
            api_token_ref: None,
            account_id_ref: None,
            tunnel_name: Some("acp-stack".to_owned()),
            tunnel_id: None,
            cloudflared_deployment: "host".to_owned(),
        }
    }

    #[test]
    fn generated_artifacts_include_ingress_and_operator_docs() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let artifacts =
            write_cloudflare_artifacts(tempdir.path(), &cloudflare(), "http://127.0.0.1:7700")
                .expect("write artifacts");
        assert_eq!(artifacts.len(), 4);
        let config =
            std::fs::read_to_string(tempdir.path().join("cloudflared/config.yml")).expect("config");
        assert!(config.contains("hostname: agent.example.com"));
        assert!(config.contains("service: http://127.0.0.1:7700"));
        let checklist = std::fs::read_to_string(tempdir.path().join("cloudflared/README.md"))
            .expect("checklist");
        assert!(checklist.contains("does not store Cloudflare API tokens"));
    }

    #[test]
    fn managed_artifacts_include_provisioned_tunnel_id() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let cloudflare = CloudflareEdgeConfig {
            mode: "managed".to_owned(),
            api_token_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
            account_id_ref: Some("CLOUDFLARE_ACCOUNT_ID".to_owned()),
            tunnel_id: Some("11111111-2222-3333-4444-555555555555".to_owned()),
            ..cloudflare()
        };

        let artifacts = write_managed_cloudflare_artifacts(
            tempdir.path(),
            &cloudflare,
            "http://127.0.0.1:7700",
            "managed-token",
        )
        .expect("write artifacts");

        assert_eq!(artifacts.len(), 5);
        let config =
            std::fs::read_to_string(tempdir.path().join("cloudflared/config.yml")).expect("config");
        assert!(config.contains("tunnel: 11111111-2222-3333-4444-555555555555"));
        let token_env =
            std::fs::read_to_string(tempdir.path().join("cloudflared/tunnel-token.env"))
                .expect("token env");
        assert_eq!(token_env, "CLOUDFLARE_TUNNEL_TOKEN=managed-token\n");
        let systemd = std::fs::read_to_string(
            tempdir
                .path()
                .join("cloudflared/acp-stack-cloudflared.service"),
        )
        .expect("systemd");
        assert!(systemd.contains("EnvironmentFile="));
        assert!(systemd.contains("run --token ${CLOUDFLARE_TUNNEL_TOKEN}"));
        let compose =
            std::fs::read_to_string(tempdir.path().join("cloudflared/docker-compose.yml"))
                .expect("compose");
        assert!(compose.contains("env_file:"));
        assert!(compose.contains("$$CLOUDFLARE_TUNNEL_TOKEN"));
        assert!(!compose.contains("CLOUDFLARE_TUNNEL_TOKEN: ${CLOUDFLARE_TUNNEL_TOKEN}"));
    }

    #[test]
    fn cloudflare_request_path_components_reject_unsafe_values() {
        assert!(path_component("account123").is_ok());
        assert!(path_component("bad/account").is_err());
        assert!(path_component("bad account").is_err());
    }

    #[test]
    fn managed_provisioning_calls_cloudflare_api_and_writes_runnable_artifacts() {
        let tunnel_id = "11111111-2222-3333-4444-555555555555";
        let (base_url, requests) = start_cloudflare_mock(vec![
            r#"{"success":true,"result":{"id":"11111111-2222-3333-4444-555555555555"}}"#,
            r#"{"success":true,"result":"managed-token"}"#,
            r#"{"success":true,"result":{}}"#,
            r#"{"success":true,"result":[]}"#,
            r#"{"success":true,"result":[{"id":"zone123","name":"example.com"}]}"#,
            r#"{"success":true,"result":[]}"#,
            r#"{"success":true,"result":{"id":"dns123"}}"#,
        ]);
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut cloudflare = CloudflareEdgeConfig {
            mode: "managed".to_owned(),
            api_token_ref: Some("CLOUDFLARE_API_TOKEN".to_owned()),
            account_id_ref: Some("CLOUDFLARE_ACCOUNT_ID".to_owned()),
            ..cloudflare()
        };

        let created = ensure_managed_cloudflare_tunnel_with_base(
            &mut cloudflare,
            "api-token",
            "account123",
            &base_url,
        )
        .expect("ensure tunnel");
        assert!(created);
        assert_eq!(cloudflare.tunnel_id.as_deref(), Some(tunnel_id));

        let artifacts = finish_managed_cloudflare_provisioning_with_base(
            tempdir.path(),
            &cloudflare,
            "http://127.0.0.1:7700",
            "api-token",
            "account123",
            &base_url,
        )
        .expect("finish provisioning");
        assert_eq!(artifacts.len(), 5);

        let token_env =
            std::fs::read_to_string(tempdir.path().join("cloudflared/tunnel-token.env"))
                .expect("token env");
        assert_eq!(token_env, "CLOUDFLARE_TUNNEL_TOKEN=managed-token\n");
        let checklist = std::fs::read_to_string(tempdir.path().join("cloudflared/README.md"))
            .expect("checklist");
        assert!(checklist.contains(&format!("{tunnel_id}.cfargotunnel.com")));

        let requests = requests.recv().expect("requests");
        assert_eq!(requests.len(), 7);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/accounts/account123/cfd_tunnel");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer api-token")
        );
        assert_eq!(requests[0].json["name"], "acp-stack");
        assert_eq!(requests[0].json["config_src"], "cloudflare");
        assert_eq!(
            requests[1].path,
            format!("/accounts/account123/cfd_tunnel/{tunnel_id}/token")
        );
        assert_eq!(requests[2].method, "PUT");
        assert_eq!(
            requests[2].path,
            format!("/accounts/account123/cfd_tunnel/{tunnel_id}/configurations")
        );
        assert_eq!(
            requests[2].json["config"]["ingress"][0]["hostname"],
            "agent.example.com"
        );
        assert_eq!(
            requests[2].json["config"]["ingress"][0]["service"],
            "http://127.0.0.1:7700"
        );
        assert_eq!(requests[3].path, "/zones?name=agent.example.com");
        assert_eq!(requests[4].path, "/zones?name=example.com");
        assert_eq!(
            requests[5].path,
            "/zones/zone123/dns_records?type=CNAME&name=agent.example.com"
        );
        assert_eq!(requests[6].method, "POST");
        assert_eq!(requests[6].path, "/zones/zone123/dns_records");
        assert_eq!(requests[6].json["type"], "CNAME");
        assert_eq!(requests[6].json["name"], "agent.example.com");
        assert_eq!(
            requests[6].json["content"],
            format!("{tunnel_id}.cfargotunnel.com")
        );
        assert_eq!(requests[6].json["proxied"], true);
    }

    #[test]
    fn service_url_from_bind_handles_ipv4_and_ipv6() {
        assert_eq!(
            service_url_from_bind("127.0.0.1:7700").unwrap(),
            "http://127.0.0.1:7700"
        );
        assert_eq!(
            service_url_from_bind("[::1]:7700").unwrap(),
            "http://[::1]:7700"
        );
    }

    #[derive(Debug)]
    struct RecordedRequest {
        method: String,
        path: String,
        authorization: Option<String>,
        json: Value,
    }

    fn start_cloudflare_mock(
        responses: Vec<&'static str>,
    ) -> (String, mpsc::Receiver<Vec<RecordedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let base_url = format!("http://{}", listener.local_addr().expect("addr"));
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut requests = Vec::new();
            for response in responses {
                let (stream, _) = listener.accept().expect("accept");
                requests.push(handle_mock_request(stream, response));
            }
            tx.send(requests).expect("send requests");
        });
        (base_url, rx)
    }

    fn handle_mock_request(mut stream: TcpStream, response: &str) -> RecordedRequest {
        let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
        let mut request_line = String::new();
        reader.read_line(&mut request_line).expect("request line");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().expect("method").to_owned();
        let path = parts.next().expect("path").to_owned();
        let mut content_length = 0usize;
        let mut authorization = None;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).expect("header");
            if header == "\r\n" {
                break;
            }
            let Some((name, value)) = header.trim_end().split_once(':') else {
                continue;
            };
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().expect("content length");
            }
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).expect("body");
        let json = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).expect("json body")
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response.len(),
            response
        );
        stream.write_all(response.as_bytes()).expect("response");
        RecordedRequest {
            method,
            path,
            authorization,
            json,
        }
    }
}
