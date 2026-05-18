use std::path::{Path, PathBuf};

use crate::config::CloudflareEdgeConfig;
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, create_dir_owner_only};

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
    let dir = config_dir.join("cloudflared");
    create_dir_owner_only(&dir)?;
    let artifacts = [
        (
            "cloudflared config",
            dir.join("config.yml"),
            render_config_yml(cloudflare, service_url),
        ),
        (
            "cloudflared systemd snippet",
            dir.join("acp-stack-cloudflared.service"),
            render_systemd_unit(&dir.join("config.yml")),
        ),
        (
            "cloudflared docker compose snippet",
            dir.join("docker-compose.yml"),
            render_docker_compose(cloudflare, service_url),
        ),
        (
            "cloudflared operator checklist",
            dir.join("README.md"),
            render_checklist(cloudflare, service_url),
        ),
    ];
    let mut written = Vec::with_capacity(artifacts.len());
    for (label, path, content) in artifacts {
        atomic_write_owner_only(&path, content.as_bytes())?;
        written.push(GeneratedCloudflareArtifact { label, path });
    }
    Ok(written)
}

fn render_config_yml(cloudflare: &CloudflareEdgeConfig, service_url: &str) -> String {
    let mut out = String::new();
    if let Some(tunnel_id) = cloudflare.tunnel_id.as_deref() {
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

fn render_systemd_unit(config_path: &Path) -> String {
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

fn render_docker_compose(cloudflare: &CloudflareEdgeConfig, service_url: &str) -> String {
    let tunnel_name = cloudflare.tunnel_name.as_deref().unwrap_or("acp-stack");
    format!(
        r#"services:
  cloudflared:
    image: cloudflare/cloudflared:latest
    command: tunnel --no-autoupdate run --token ${{CLOUDFLARE_TUNNEL_TOKEN}}
    restart: unless-stopped
    environment:
      CLOUDFLARE_TUNNEL_TOKEN: ${{CLOUDFLARE_TUNNEL_TOKEN}}
    # Tunnel `{tunnel_name}` should publish {hostname} to {service_url}.
"#,
        hostname = cloudflare.hostname,
    )
}

fn render_checklist(cloudflare: &CloudflareEdgeConfig, service_url: &str) -> String {
    let tunnel_name = cloudflare.tunnel_name.as_deref().unwrap_or("acp-stack");
    format!(
        r#"# Cloudflare Tunnel checklist

1. Install `cloudflared` on the host or run the generated Docker Compose service.
2. Create a Cloudflare Tunnel named `{tunnel_name}`.
3. Publish `{hostname}` to `{service_url}`.
4. Enable WebSockets for the zone if it was disabled.
5. Keep `acps` bound to loopback and verify `acps security check` has no Cloudflare findings.
6. Optionally enable the Cloudflare managed transform that adds visitor location headers.

This generated profile does not store Cloudflare API tokens or tunnel tokens in `acp-stack.toml`.
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

    fn cloudflare() -> CloudflareEdgeConfig {
        CloudflareEdgeConfig {
            enabled: true,
            mode: "generated".to_owned(),
            exposure: "tunnel".to_owned(),
            hostname: "agent.example.com".to_owned(),
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
}
