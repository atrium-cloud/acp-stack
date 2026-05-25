//! Edge (Cloudflare tunnel) validation.

use crate::config::schema::EdgeConfig;
use crate::error::{Result, StackError};

pub(crate) fn validate_edge(edge: &EdgeConfig) -> Result<()> {
    let Some(cloudflare) = &edge.cloudflare else {
        return Ok(());
    };
    if !cloudflare.enabled {
        return Ok(());
    }
    if cloudflare.mode == "managed" {
        return Err(StackError::CloudflareManagedNotImplemented);
    }
    if cloudflare.mode != "generated" {
        return Err(StackError::InvalidCloudflareMode {
            mode: cloudflare.mode.clone(),
        });
    }
    if cloudflare.exposure != "tunnel" {
        return Err(StackError::InvalidCloudflareExposure {
            exposure: cloudflare.exposure.clone(),
        });
    }
    if !matches!(
        cloudflare.cloudflared_deployment.as_str(),
        "host" | "docker" | "external"
    ) {
        return Err(StackError::InvalidCloudflaredDeployment {
            deployment: cloudflare.cloudflared_deployment.clone(),
        });
    }
    validate_cloudflare_hostname(&cloudflare.hostname)?;
    validate_cloudflare_tunnel_name(cloudflare.tunnel_name.as_deref())?;
    validate_cloudflare_tunnel_id(cloudflare.tunnel_id.as_deref())?;
    Ok(())
}

fn validate_cloudflare_hostname(hostname: &str) -> Result<()> {
    let hostname = hostname.trim();
    if hostname.is_empty()
        || hostname.len() > 253
        || hostname.contains('/')
        || hostname.contains(':')
        || hostname.chars().any(char::is_whitespace)
        || !hostname.contains('.')
    {
        return Err(StackError::InvalidCloudflareHostname {
            hostname: hostname.to_owned(),
        });
    }
    for label in hostname.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        {
            return Err(StackError::InvalidCloudflareHostname {
                hostname: hostname.to_owned(),
            });
        }
    }
    Ok(())
}

fn validate_cloudflare_tunnel_name(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(StackError::MissingField {
            field: "edge.cloudflare.tunnel_name",
        });
    }
    if value.len() > 64
        || value.chars().any(|ch| {
            !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) || ch.is_ascii_control()
        })
    {
        return Err(StackError::InvalidCloudflareTunnelName {
            tunnel_name: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_cloudflare_tunnel_id(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() {
        return Err(StackError::MissingField {
            field: "edge.cloudflare.tunnel_id",
        });
    }
    let bytes = value.as_bytes();
    let uuid_shape = bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        });
    if !uuid_shape {
        return Err(StackError::InvalidCloudflareTunnelId {
            tunnel_id: value.to_owned(),
        });
    }
    Ok(())
}
