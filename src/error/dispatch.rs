//! Cross-domain dispatch for [`StackError`]: error code, public message,
//! remediation hint, and HTTP status. Each accessor fans out to the
//! per-domain helper modules so the variant-to-value tables stay next to
//! the domain that owns them.

use super::*;

impl StackError {
    /// Dotted-namespace code suitable for the HTTP error envelope at
    /// `docs/specs/api/api.md:29-48`. Delegates to per-domain helpers so the
    /// variant-to-code table lives next to the matching domain.
    pub fn error_code(&self) -> &str {
        if let Self::NativeAgentConfigOperationFailed { code } = self {
            return code;
        }
        config::error_code(self)
            .or_else(|| state::error_code(self))
            .or_else(|| security::error_code(self))
            .or_else(|| secrets::error_code(self))
            .or_else(|| supabase::error_code(self))
            .or_else(|| edge::error_code(self))
            .or_else(|| extensions::error_code(self))
            .or_else(|| workspace_source::error_code(self))
            .or_else(|| download::error_code(self))
            .or_else(|| archive::error_code(self))
            .or_else(|| serve::error_code(self))
            .or_else(|| agent_install::error_code(self))
            .or_else(|| agent_runtime::error_code(self))
            .or_else(|| session::error_code(self))
            .or_else(|| workspace::error_code(self))
            .or_else(|| command::error_code(self))
            .or_else(|| permission::error_code(self))
            .or_else(|| auth_http::error_code(self))
            .expect("StackError variant should be claimed by exactly one error domain")
    }

    /// Human-readable message safe to expose through the public HTTP API.
    /// `Display` remains intentionally detailed for CLI diagnostics and local
    /// logs; this method avoids leaking local filesystem paths, OS errors, or
    /// secret-store metadata to remote clients.
    pub fn public_message(&self) -> String {
        config::public_message(self)
            .or_else(|| state::public_message(self))
            .or_else(|| security::public_message(self))
            .or_else(|| secrets::public_message(self))
            .or_else(|| supabase::public_message(self))
            .or_else(|| edge::public_message(self))
            .or_else(|| extensions::public_message(self))
            .or_else(|| workspace_source::public_message(self))
            .or_else(|| download::public_message(self))
            .or_else(|| archive::public_message(self))
            .or_else(|| serve::public_message(self))
            .or_else(|| agent_install::public_message(self))
            .or_else(|| agent_runtime::public_message(self))
            .or_else(|| session::public_message(self))
            .or_else(|| workspace::public_message(self))
            .or_else(|| command::public_message(self))
            .or_else(|| permission::public_message(self))
            .or_else(|| auth_http::public_message(self))
            .expect("StackError variant should be claimed by exactly one error domain")
    }

    pub fn remediation_hint(&self) -> Option<String> {
        if let StackError::DepsApplyFailed { retry_command, .. } = self {
            return Some(format!(
                "inspect `acps installer history --agent deps_apply`, fix the failing install action, then re-run `{retry_command}`"
            ));
        }
        Some(match self {
            StackError::ConfigRead { .. } => {
                "verify the config path and file permissions, then retry the command"
            }
            StackError::SecretNotFound { .. }
            | StackError::MissingSupabaseApiKey { .. }
            | StackError::MissingSupabaseDbUrl { .. } => {
                "store the missing secret with `acps secrets set <name>`"
            }
            StackError::ConfigExists { .. } => "use `--force` only when replacing the config is intentional",
            StackError::ResetNotConfirmed => "re-run with `--yes` to confirm reset",
            StackError::AgentInstallerFailed { .. }
            | StackError::AgentInstallerCreatesMissing { .. }
            | StackError::AgentInstallerBinaryUnrunnable { .. }
            | StackError::AgentInstallerPrerequisitesMissing { .. }
            | StackError::AgentInstallerWorkingDirectoryMissing { .. }
            | StackError::AgentInstallerTimeout => {
                "inspect `acps installer history`, then retry with `acps agent install`"
            }
            StackError::WorkspaceMaterializeFailed { .. } | StackError::WorkspaceCommandFailed { .. } => {
                "inspect the failed command output and retry after fixing the source or command"
            }
            StackError::CloudflareManagedProvision { .. } | StackError::CloudflareApiStatus { .. } => {
                "verify the Cloudflare API token, account id, tunnel permissions, and hostname, then retry `acps init --resume`"
            }
            StackError::AgentTestFailed { .. } | StackError::AgentInitializeFailed { .. } => {
                "verify agent install, provider secrets, and model selection, then retry the testflight"
            }
            StackError::StackUpdateBinarySwap { rollback_errors, .. } => {
                if rollback_errors.is_empty() {
                    "verify the install directory is writable, then re-run `acps update`"
                } else {
                    "the install directory may hold a partial binary set; reinstall with the install script"
                }
            }
            // Unreachable: the early return above handles this variant so
            // the hint can name the surface-specific retry command.
            StackError::DepsApplyFailed { .. } => {
                "inspect `acps installer history --agent deps_apply` and fix the failing install action"
            }
            StackError::InvalidParam { .. }
            | StackError::InvalidSocketAddress { .. }
            | StackError::InvalidCloudflareMode { .. }
            | StackError::InvalidCloudflareExposure { .. }
            | StackError::InvalidCloudflaredDeployment { .. }
            | StackError::InvalidCloudflareHostname { .. }
            | StackError::InvalidCloudflareTunnelName { .. }
            | StackError::InvalidCloudflareTunnelId { .. } => {
                "run the command with `--help` and correct the invalid input"
            }
            StackError::MissingField { .. } => {
                "edit the config or imported TOML to include the required fields"
            }
            StackError::ConfigToml(_) => "fix the TOML syntax or field types, then retry",
            _ => return None,
        }
        .to_owned())
    }

    /// HTTP status code for this error when rendered through the API envelope.
    /// Coarse mapping: client-provided invalid input is 4xx; failures the
    /// server hits internally (filesystem, sqlite, age decrypt) are 5xx.
    pub fn http_status(&self) -> StatusCode {
        config::http_status(self)
            .or_else(|| state::http_status(self))
            .or_else(|| security::http_status(self))
            .or_else(|| secrets::http_status(self))
            .or_else(|| supabase::http_status(self))
            .or_else(|| edge::http_status(self))
            .or_else(|| extensions::http_status(self))
            .or_else(|| workspace_source::http_status(self))
            .or_else(|| download::http_status(self))
            .or_else(|| archive::http_status(self))
            .or_else(|| serve::http_status(self))
            .or_else(|| agent_install::http_status(self))
            .or_else(|| agent_runtime::http_status(self))
            .or_else(|| session::http_status(self))
            .or_else(|| workspace::http_status(self))
            .or_else(|| command::http_status(self))
            .or_else(|| permission::http_status(self))
            .or_else(|| auth_http::http_status(self))
            .expect("StackError variant should be claimed by exactly one error domain")
    }
}
