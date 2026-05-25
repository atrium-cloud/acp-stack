//! Cloudflare-edge configuration error helpers (`edge.cloudflare.*` namespace).

use http::StatusCode;

use super::StackError;

pub(super) fn error_code(err: &StackError) -> Option<&'static str> {
    use StackError::*;
    Some(match err {
        CloudflareManagedNotImplemented => "edge.cloudflare.managed_not_implemented",
        InvalidCloudflareMode { .. } => "edge.cloudflare.invalid_mode",
        InvalidCloudflareExposure { .. } => "edge.cloudflare.invalid_exposure",
        InvalidCloudflaredDeployment { .. } => "edge.cloudflare.invalid_deployment",
        InvalidCloudflareHostname { .. } => "edge.cloudflare.invalid_hostname",
        InvalidCloudflareTunnelName { .. } => "edge.cloudflare.invalid_tunnel_name",
        InvalidCloudflareTunnelId { .. } => "edge.cloudflare.invalid_tunnel_id",
        _ => return None,
    })
}

pub(super) fn public_message(err: &StackError) -> Option<String> {
    use StackError::*;
    Some(match err {
        CloudflareManagedNotImplemented => {
            "Cloudflare managed provisioning is not implemented yet; use generated mode".to_owned()
        }
        InvalidCloudflareMode { .. } => "invalid Cloudflare edge mode".to_owned(),
        InvalidCloudflareExposure { .. } => "invalid Cloudflare exposure mode".to_owned(),
        InvalidCloudflaredDeployment { .. } => "invalid cloudflared deployment mode".to_owned(),
        InvalidCloudflareHostname { .. } => "invalid Cloudflare hostname".to_owned(),
        InvalidCloudflareTunnelName { .. } => "invalid Cloudflare tunnel name".to_owned(),
        InvalidCloudflareTunnelId { .. } => "invalid Cloudflare tunnel id".to_owned(),
        _ => return None,
    })
}

pub(super) fn http_status(err: &StackError) -> Option<StatusCode> {
    use StackError::*;
    Some(match err {
        CloudflareManagedNotImplemented
        | InvalidCloudflareMode { .. }
        | InvalidCloudflareExposure { .. }
        | InvalidCloudflaredDeployment { .. }
        | InvalidCloudflareHostname { .. }
        | InvalidCloudflareTunnelName { .. }
        | InvalidCloudflareTunnelId { .. } => StatusCode::BAD_REQUEST,
        _ => return None,
    })
}
