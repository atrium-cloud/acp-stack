use http::StatusCode;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    #[error("HOME is not set; cannot resolve default config path")]
    HomeNotSet,

    #[error("failed to read config at {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write config export at {path}: {source}")]
    ConfigWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create directory {path}: {source}")]
    DirectoryCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to create file {path}: {source}")]
    FileCreate {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to provision agent config at {path}: {reason}")]
    AgentConfigProvision { path: PathBuf, reason: String },

    #[error("failed to set owner-only permissions on {path}: {source}")]
    PermissionSet {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to initialize config at {path}: {source}")]
    ConfigInitialize {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("config already exists at {path}; pass --force to replace it")]
    ConfigExists { path: PathBuf },

    #[error("import data was not valid base64: {source}")]
    ImportBase64Decode {
        #[source]
        source: base64::DecodeError,
    },

    #[error("imported config was not valid UTF-8: {source}")]
    ImportUtf8 {
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("failed to remove {path}: {source}")]
    FileRemove {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("acps reset would delete the listed files; re-run with --yes to confirm")]
    ResetNotConfirmed,

    #[error("path {path} has no parent directory")]
    MissingParentDir { path: PathBuf },

    #[error("config TOML is invalid: {0}")]
    ConfigToml(#[from] toml::de::Error),

    #[error("failed to serialize canonical config TOML: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    #[error("state database error: {0}")]
    State(#[from] rusqlite::Error),

    #[error("state schema version {found} is newer than supported version {supported}")]
    IncompatibleStateSchema { found: i64, supported: i64 },

    #[error("existing state table `{table}` is not managed by a recorded migration")]
    UnmanagedStateTable { table: &'static str },

    #[error("migration manifest is invalid: {0}")]
    MigrationManifestParse(toml::de::Error),

    #[error(
        "migration manifest ids must be strictly increasing positive integers; saw {id} after {previous}"
    )]
    InvalidManifestOrder { id: i64, previous: i64 },

    #[error("migration manifest does not match the compiled registry: {reason}")]
    ManifestRegistryMismatch { reason: String },

    #[error(
        "state database is missing the required `{table}` table after migrations; the file may be corrupted"
    )]
    MissingMigratedTable { table: &'static str },

    #[error("event payload must be valid JSON text")]
    InvalidEventPayload,

    #[error("query parameter `{field}` is invalid: {reason}")]
    InvalidParam { field: &'static str, reason: String },

    #[error("auth failure payload must be valid JSON text")]
    InvalidAuthFailurePayload,

    #[error("failed to read age key at {path}: {source}")]
    AgeKeyRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write age key at {path}: {source}")]
    AgeKeyWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("age key at {path} is malformed: {reason}")]
    AgeKeyParse { path: PathBuf, reason: &'static str },

    #[error("failed to read secret store at {path}: {source}")]
    SecretStoreRead {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to write secret store at {path}: {source}")]
    SecretStoreWrite {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to encrypt secret store: {0}")]
    SecretStoreEncrypt(#[from] age::EncryptError),

    #[error("failed to decrypt secret store: {0}")]
    SecretStoreDecrypt(#[from] age::DecryptError),

    #[error("decrypted secret store could not be parsed as TOML: {0}")]
    SecretStorePlaintextParse(toml::de::Error),

    #[error("decrypted secret store plaintext was not valid UTF-8: {source}")]
    SecretStorePlaintextNotUtf8 {
        #[source]
        source: std::str::Utf8Error,
    },

    #[error("failed to serialize secret store plaintext: {0}")]
    SecretStorePlaintextSerialize(toml::ser::Error),

    #[error("secret `{name}` was not found in the secret store")]
    SecretNotFound { name: String },

    #[error(
        "secret store is non-empty but does not contain the session key reference `{name}`; reset and re-init"
    )]
    MissingSessionKey { name: String },

    #[error(
        "secret store is non-empty but does not contain the admin key reference `{name}`; reset and re-init"
    )]
    MissingAdminKey { name: String },

    #[error(
        "secret store is non-empty but does not contain the Supabase service-role key reference `{name}`"
    )]
    MissingSupabaseServiceRoleKey { name: String },

    #[error(
        "[logging.supabase].url must start with `https://` when external logging is enabled; got `{url}`"
    )]
    InvalidSupabaseUrl { url: String },

    #[error(
        "[logging.supabase].schema must be a safe Postgres identifier matching `^[a-z_][a-z0-9_]{{0,62}}$`; got `{schema}`"
    )]
    InvalidSupabaseSchema { schema: String },

    #[error("edge.cloudflare.mode = \"managed\" is not implemented yet; use mode = \"generated\"")]
    CloudflareManagedNotImplemented,

    #[error("edge.cloudflare.mode must be `generated`; got `{mode}`")]
    InvalidCloudflareMode { mode: String },

    #[error("edge.cloudflare.exposure must be `tunnel`; got `{exposure}`")]
    InvalidCloudflareExposure { exposure: String },

    #[error(
        "edge.cloudflare.cloudflared_deployment must be one of host, docker, external; got `{deployment}`"
    )]
    InvalidCloudflaredDeployment { deployment: String },

    #[error(
        "edge.cloudflare.hostname must be a bare hostname such as agent.example.com; got `{hostname}`"
    )]
    InvalidCloudflareHostname { hostname: String },

    #[error(
        "edge.cloudflare.tunnel_name must contain only ASCII letters, numbers, '.', '_', or '-', up to 64 bytes; got `{tunnel_name}`"
    )]
    InvalidCloudflareTunnelName { tunnel_name: String },

    #[error("edge.cloudflare.tunnel_id must be a Cloudflare tunnel UUID; got `{tunnel_id}`")]
    InvalidCloudflareTunnelId { tunnel_id: String },

    #[error("Supabase sink rejected upload: {status} {body}")]
    SupabaseSinkHttp { status: u16, body: String },

    #[error("Supabase sink received a row for unknown source table `{table}`; refusing to upload")]
    SupabaseSinkUnknownTable { table: String },

    #[error("failed to read stdin: {source}")]
    StdinRead { source: std::io::Error },

    #[error("missing required section `{section}`")]
    MissingSection { section: &'static str },

    #[error("{field} is required")]
    MissingField { field: &'static str },

    #[error("{field} is not valid when workspace.source.type is {source_type}")]
    InvalidWorkspaceSourceField {
        field: &'static str,
        source_type: &'static str,
    },

    #[error("{field} is not valid when {type_field} is {type_value}")]
    InvalidConfigFieldForType {
        field: &'static str,
        type_field: &'static str,
        type_value: &'static str,
    },

    #[error("{field} must be a socket address")]
    InvalidSocketAddress { field: &'static str },

    #[error("{field} must be greater than zero")]
    NonZeroRequired { field: &'static str },

    #[error("{field} must be absolute")]
    PathMustBeAbsolute { field: &'static str },

    #[error("{field} must not contain `..` segments")]
    PathContainsParentDir { field: &'static str },

    #[error("workspace.source.type must be one of none, git, s3")]
    InvalidWorkspaceSourceType,

    #[error("agent.restart must be one of never, on-crash")]
    InvalidAgentRestart,

    #[error("agent.expected_sha256 must be exactly 64 lowercase hex characters")]
    InvalidExpectedSha256,

    #[error("agent.install.type must be `shell` (the only operator-facing install type)")]
    InvalidAgentInstallType,

    #[error("{field} must start with http:// or https://")]
    UrlMustBeHttp { field: &'static str },

    #[error("{field} must start with https://")]
    UrlMustBeHttps { field: &'static str },

    #[error(
        "auth.session_key_ref and auth.admin_key_ref must be different names; aliasing them would collapse the session/admin boundary"
    )]
    AuthRefsNotDistinct,

    #[error(
        "secret `{name}` is the configured {kind} key reference; use `acps auth regenerate-session-key` or `acps reset --yes` instead of `acps secrets`"
    )]
    SecretReservedForAuth { name: String, kind: &'static str },

    #[error(
        "config import would change `[auth].{field}` from `{current}` to `{incoming}`; run `acps reset --yes` and re-init to rotate auth references"
    )]
    ImportChangesAuthRef {
        field: &'static str,
        current: String,
        incoming: String,
    },

    #[error("failed to bind {bind}: {source}")]
    ServeBind {
        bind: String,
        source: std::io::Error,
    },

    #[error("HTTP server error: {source}")]
    ServeIo {
        #[source]
        source: std::io::Error,
    },

    #[error(
        "refusing to run as root; pass --allow-root or set ACP_STACK_ALLOW_ROOT=1 only for disposable/dev profiles"
    )]
    ServeRefusedAsRoot,

    #[error(
        "running as root requires a non-empty admin API key; re-run `acps init` to provision one before retrying"
    )]
    ServeRootRequiresAdminKey,

    #[error(
        "agent is not configured; declare `[agent].id` matching a registry entry, or provide a `[agent.install] type = \"shell\"` recipe"
    )]
    AgentNotConfigured,

    #[error("agent installer exited with status {exit:?}: {stderr_tail}")]
    AgentInstallerFailed {
        exit: Option<i32>,
        stderr_tail: String,
    },

    #[error("agent installer ran but `creates = {name}` did not resolve afterwards")]
    AgentInstallerCreatesMissing { name: String },

    #[error("agent installer hit the 10-minute timeout")]
    AgentInstallerTimeout,

    #[error("ACP registry does not contain agent `{id}`")]
    AgentRegistryMissing { id: String },

    #[error("{name} is not currently supported. Please try a different agent.")]
    AgentUnsupported { name: String },

    #[error("agent registry could not be loaded: {reason}")]
    RegistryLoad { reason: String },

    #[error("failed to query GitHub Releases for {repo}: {source}")]
    GithubReleaseFetch {
        repo: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("no release asset for {repo} matched pattern `{pattern}`")]
    GithubReleaseAssetNotFound { repo: String, pattern: String },

    #[error(
        "{matches} release assets for {repo} matched pattern `{pattern}`; expected exactly one"
    )]
    GithubReleaseAssetAmbiguous {
        repo: String,
        pattern: String,
        matches: usize,
    },

    #[error("failed to extract release archive from {repo}: {reason}")]
    GithubReleaseArchiveExtract { repo: String, reason: String },

    #[error(
        "release asset `{asset}` from {repo} failed sha256 verification: expected {expected}, got {actual}"
    )]
    GithubReleaseChecksumMismatch {
        repo: String,
        asset: String,
        expected: String,
        actual: String,
    },

    #[error("unsupported host architecture `{arch}` for GitHub Release install")]
    UnsupportedHostArch { arch: &'static str },

    #[error("agent binary sha256 mismatch: expected {expected}, got {actual}")]
    AgentSha256Mismatch { expected: String, actual: String },

    #[error("failed to spawn agent subprocess: {source}")]
    AgentSpawnFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("agent is already running")]
    AgentAlreadyRunning,

    #[error("agent is not running")]
    AgentNotRunning,

    #[error("agent failed to initialize: {reason}")]
    AgentInitializeFailed { reason: String },

    #[error("agent has not been initialized yet")]
    AgentNotInitialized,

    #[error("agent does not support `{name}`")]
    AgentUnsupportedCapability { name: &'static str },

    #[error("agent API request to {path} failed: {source}")]
    AgentApiRequest {
        path: &'static str,
        #[source]
        source: reqwest::Error,
    },

    #[error("agent API request to {path} failed with status {status}: {body}")]
    AgentApiStatus {
        path: &'static str,
        status: StatusCode,
        body: String,
    },

    #[error("agent request to {method} failed: {message}")]
    AgentRequestFailed {
        method: &'static str,
        message: String,
    },

    #[error("session `{id}` was not found")]
    SessionNotFound { id: String },

    #[error("session `{id}` is closed")]
    SessionClosed { id: String },

    #[error("prompt `{id}` was not found")]
    PromptNotFound { id: String },

    #[error("session `{session_id}` does not own prompt `{prompt_id}`")]
    PromptSessionMismatch {
        session_id: String,
        prompt_id: String,
    },

    #[error("prompt body must include at least one content block")]
    PromptBodyEmpty,

    #[error("prompt body is not valid ACP content: {0}")]
    PromptBodyInvalid(String),

    #[error("workspace path `{requested}` is invalid: {reason}")]
    WorkspacePathInvalid { reason: String, requested: String },

    #[error("workspace path `{requested}` resolves outside the workspace root")]
    WorkspaceSymlinkEscape { requested: String },

    #[error("workspace path `{requested}` was not found")]
    WorkspaceNotFound { requested: String },

    #[error("workspace file exceeds the {limit}-byte size limit")]
    WorkspaceTooLarge { limit: u64 },

    #[error("workspace upload is invalid: {reason}")]
    WorkspaceUploadInvalid { reason: &'static str },

    #[error("workspace I/O on `{requested}` failed: {source}")]
    WorkspaceIo {
        requested: String,
        #[source]
        source: std::io::Error,
    },

    #[error("workspace file encoding is invalid: {reason}")]
    WorkspaceEncodingInvalid { reason: &'static str },

    #[error("workspace.uploads must be inside workspace.root")]
    WorkspaceUploadsNotUnderRoot,

    #[error("permissions.mode must be one of auto, supervised, locked")]
    InvalidPermissionsMode,

    #[error("{field} must be a duration like \"10m\", \"5s\", or \"100ms\"")]
    InvalidDurationField { field: &'static str },

    #[error("env variable name `{name}` is not a valid POSIX identifier")]
    InvalidEnvName { name: String },

    #[error("command `{id}` was not found")]
    CommandNotFound { id: String },

    #[error("command rejected by policy: {reason}")]
    CommandDenied { reason: &'static str },

    #[error("command cwd `{requested}` resolves outside the workspace root")]
    CommandCwdOutsideWorkspace { requested: String },

    #[error("command env variable `{name}` is not on commands.env_allowlist")]
    CommandEnvNotAllowed { name: String },

    #[error("failed to spawn command subprocess: {source}")]
    CommandSpawnFailed {
        #[source]
        source: std::io::Error,
    },

    #[error("command timed out before the subprocess produced an exit status")]
    CommandTimeout,

    #[error(
        "secret ref `{ref_name}` (referenced from {kind}) collides with the configured auth key ref; rename the secret"
    )]
    SecretRefReservedForAuth {
        ref_name: String,
        kind: &'static str,
    },

    #[error("secret ref name `{name}` is invalid; use ASCII letters, digits, and underscores")]
    InvalidSecretRefName { name: String },

    #[error("secret ref name `{name}` is declared more than once across the config")]
    DuplicateSecretRef { name: String },

    #[error("permissions.timeout_action must be one of deny, approve")]
    InvalidTimeoutAction,

    #[error("security.http.trusted_proxies entry `{value}` is not a valid IP address")]
    InvalidTrustedProxy { value: String },

    #[error("mcp.servers entry `{name}` is invalid: {reason}")]
    InvalidMcpServer { name: String, reason: &'static str },

    #[error("mcp.servers contains duplicate name `{name}`")]
    DuplicateMcpServer { name: String },

    #[error("dependencies.{category} entry has empty name")]
    DependencyMissingName { category: &'static str },

    #[error("dependencies.{category} contains duplicate name `{name}`")]
    DuplicateDependency {
        category: &'static str,
        name: String,
    },

    #[error("permission `{id}` was not found")]
    PermissionNotFound { id: String },

    #[error(
        "permission `{id}` cannot transition from `{from}` to `{to}`; the request is already terminal"
    )]
    InvalidPermissionTransition {
        id: String,
        from: &'static str,
        to: &'static str,
    },

    #[error("durable JSON corruption in `{field}`: {reason}")]
    StateInvalidJson { field: &'static str, reason: String },

    #[error("rate limit exceeded; retry later")]
    RateLimited,

    #[error("IP `{ip}` is temporarily blocked due to repeated auth failures")]
    IpBlocked { ip: String },

    #[error("Origin `{origin}` is not in the configured allowlist")]
    OriginNotAllowed { origin: String },
}

impl StackError {
    /// Dotted-namespace code suitable for the HTTP error envelope at
    /// `docs/specs/api/api.md:20-42`. The set is intentionally coarse: it
    /// identifies the failed subsystem and broad failure mode, not every
    /// variant.
    pub fn error_code(&self) -> &'static str {
        use StackError::*;
        match self {
            HomeNotSet => "config.home_missing",
            ConfigRead { .. } => "config.read_failed",
            ConfigWrite { .. } => "config.write_failed",
            AgentConfigProvision { .. } => "agent.config_provision_failed",
            ConfigInitialize { .. } => "config.initialize_failed",
            ConfigExists { .. } => "config.exists",
            ConfigToml(_) | ConfigSerialize(_) => "config.invalid",
            ImportBase64Decode { .. } => "import.base64_invalid",
            ImportUtf8 { .. } => "import.utf8_invalid",
            DirectoryCreate { .. } => "io.directory_create_failed",
            FileCreate { .. } => "io.file_create_failed",
            FileRemove { .. } => "io.file_remove_failed",
            PermissionSet { .. } => "io.permission_set_failed",
            MissingParentDir { .. } => "io.missing_parent_dir",
            ResetNotConfirmed => "reset.not_confirmed",
            State(_) => "state.error",
            IncompatibleStateSchema { .. } => "state.incompatible_schema",
            UnmanagedStateTable { .. } => "state.unmanaged_table",
            MigrationManifestParse(_) => "state.migration_manifest_invalid",
            InvalidManifestOrder { .. } => "state.invalid_manifest_order",
            ManifestRegistryMismatch { .. } => "state.manifest_registry_mismatch",
            MissingMigratedTable { .. } => "state.missing_migrated_table",
            InvalidEventPayload => "state.invalid_event_payload",
            InvalidAuthFailurePayload => "state.invalid_auth_failure_payload",
            AgeKeyRead { .. } | SecretStoreRead { .. } => "secrets.read_failed",
            AgeKeyWrite { .. } | SecretStoreWrite { .. } => "secrets.write_failed",
            AgeKeyParse { .. } => "secrets.age_key_invalid",
            SecretStoreEncrypt(_) => "secrets.encrypt_failed",
            SecretStoreDecrypt(_) => "secrets.decrypt_failed",
            SecretStorePlaintextParse(_)
            | SecretStorePlaintextSerialize(_)
            | SecretStorePlaintextNotUtf8 { .. } => "secrets.plaintext_invalid",
            SecretNotFound { .. } => "secrets.not_found",
            MissingSessionKey { .. } => "auth.missing_session_key",
            MissingAdminKey { .. } => "auth.missing_admin_key",
            MissingSupabaseServiceRoleKey { .. } => "logging.supabase.missing_service_role_key",
            InvalidSupabaseUrl { .. } => "logging.supabase.invalid_url",
            InvalidSupabaseSchema { .. } => "logging.supabase.invalid_schema",
            CloudflareManagedNotImplemented => "edge.cloudflare.managed_not_implemented",
            InvalidCloudflareMode { .. } => "edge.cloudflare.invalid_mode",
            InvalidCloudflareExposure { .. } => "edge.cloudflare.invalid_exposure",
            InvalidCloudflaredDeployment { .. } => "edge.cloudflare.invalid_deployment",
            InvalidCloudflareHostname { .. } => "edge.cloudflare.invalid_hostname",
            InvalidCloudflareTunnelName { .. } => "edge.cloudflare.invalid_tunnel_name",
            InvalidCloudflareTunnelId { .. } => "edge.cloudflare.invalid_tunnel_id",
            SupabaseSinkHttp { .. } => "logging.supabase.http_error",
            SupabaseSinkUnknownTable { .. } => "logging.supabase.unknown_table",
            StdinRead { .. } => "io.stdin_read_failed",
            MissingSection { .. }
            | MissingField { .. }
            | InvalidWorkspaceSourceField { .. }
            | InvalidConfigFieldForType { .. }
            | InvalidSocketAddress { .. }
            | NonZeroRequired { .. }
            | PathMustBeAbsolute { .. }
            | PathContainsParentDir { .. }
            | InvalidWorkspaceSourceType
            | InvalidAgentRestart
            | InvalidExpectedSha256
            | InvalidAgentInstallType
            | UrlMustBeHttp { .. }
            | UrlMustBeHttps { .. }
            | AuthRefsNotDistinct
            | InvalidPermissionsMode
            | InvalidDurationField { .. }
            | InvalidEnvName { .. } => "config.invalid",
            SecretReservedForAuth { .. } => "secrets.reserved_for_auth",
            ImportChangesAuthRef { .. } => "config.import_changes_auth_ref",
            ServeBind { .. } => "serve.bind_failed",
            ServeIo { .. } => "serve.io_error",
            ServeRefusedAsRoot => "serve.refused_as_root",
            ServeRootRequiresAdminKey => "serve.root_requires_admin_key",
            AgentNotConfigured => "agent.not_configured",
            AgentInstallerFailed { .. } => "agent.installer_failed",
            AgentInstallerCreatesMissing { .. } => "agent.installer_creates_missing",
            AgentInstallerTimeout => "agent.installer_timeout",
            AgentRegistryMissing { .. } => "agent.registry_missing",
            AgentUnsupported { .. } => "agent.unsupported",
            RegistryLoad { .. } => "agent.registry_load_failed",
            GithubReleaseFetch { .. } => "agent.github_release_fetch_failed",
            GithubReleaseAssetNotFound { .. } => "agent.github_release_asset_not_found",
            GithubReleaseAssetAmbiguous { .. } => "agent.github_release_asset_ambiguous",
            GithubReleaseArchiveExtract { .. } => "agent.github_release_archive_extract_failed",
            GithubReleaseChecksumMismatch { .. } => "agent.github_release_checksum_mismatch",
            UnsupportedHostArch { .. } => "agent.unsupported_host_arch",
            AgentSha256Mismatch { .. } => "agent.sha256_mismatch",
            AgentSpawnFailed { .. } => "agent.spawn_failed",
            AgentAlreadyRunning => "agent.already_running",
            AgentNotRunning => "agent.not_running",
            AgentInitializeFailed { .. } => "agent.initialize_failed",
            AgentNotInitialized => "agent.not_initialized",
            AgentUnsupportedCapability { .. } => "agent.unsupported_capability",
            AgentApiRequest { .. } => "agent.api_request_failed",
            AgentApiStatus { .. } => "agent.api_status_failed",
            AgentRequestFailed { .. } => "agent.request_failed",
            SessionNotFound { .. } => "session.not_found",
            SessionClosed { .. } => "session.closed",
            PromptNotFound { .. } => "prompt.not_found",
            PromptSessionMismatch { .. } => "prompt.session_mismatch",
            PromptBodyEmpty => "prompt.body_empty",
            PromptBodyInvalid(_) => "prompt.body_invalid",
            WorkspacePathInvalid { .. } => "workspace.path_invalid",
            WorkspaceSymlinkEscape { .. } => "workspace.symlink_escape",
            WorkspaceNotFound { .. } => "workspace.not_found",
            WorkspaceTooLarge { .. } => "workspace.too_large",
            WorkspaceUploadInvalid { .. } => "workspace.upload_invalid",
            WorkspaceIo { .. } => "workspace.io_failed",
            WorkspaceEncodingInvalid { .. } => "workspace.encoding_invalid",
            WorkspaceUploadsNotUnderRoot => "config.invalid",
            CommandNotFound { .. } => "command.not_found",
            CommandDenied { .. } => "command.denied",
            CommandCwdOutsideWorkspace { .. } => "command.cwd_outside_workspace",
            CommandEnvNotAllowed { .. } => "command.env_not_allowed",
            CommandSpawnFailed { .. } => "command.spawn_failed",
            CommandTimeout => "command.timeout",
            SecretRefReservedForAuth { .. } => "secrets.reserved_for_auth",
            InvalidSecretRefName { .. }
            | DuplicateSecretRef { .. }
            | InvalidTimeoutAction
            | InvalidTrustedProxy { .. }
            | InvalidMcpServer { .. }
            | DuplicateMcpServer { .. }
            | DependencyMissingName { .. }
            | DuplicateDependency { .. } => "config.invalid",
            PermissionNotFound { .. } => "permission.not_found",
            InvalidPermissionTransition { .. } => "permission.invalid_transition",
            StateInvalidJson { .. } => "state.invalid_json",
            RateLimited => "auth.rate_limited",
            IpBlocked { .. } => "auth.ip_blocked",
            OriginNotAllowed { .. } => "auth.origin_not_allowed",
            InvalidParam { .. } => "request.invalid_param",
        }
    }

    /// Human-readable message safe to expose through the public HTTP API.
    /// `Display` remains intentionally detailed for CLI diagnostics and local
    /// logs; this method avoids leaking local filesystem paths, OS errors, or
    /// secret-store metadata to remote clients.
    pub fn public_message(&self) -> String {
        use StackError::*;
        match self {
            HomeNotSet => "HOME is not set".to_owned(),
            ConfigRead { .. } => "failed to read config".to_owned(),
            ConfigWrite { .. } => "failed to write config".to_owned(),
            ConfigInitialize { .. } => "failed to initialize config".to_owned(),
            ConfigExists { .. } => "config already exists".to_owned(),
            ConfigToml(_) => "config TOML is invalid".to_owned(),
            ConfigSerialize(_) => "failed to serialize config".to_owned(),
            ImportBase64Decode { .. } => "import data was not valid base64".to_owned(),
            ImportUtf8 { .. } => "imported config was not valid UTF-8".to_owned(),
            DirectoryCreate { .. } => "failed to create directory".to_owned(),
            FileCreate { .. } => "failed to create file".to_owned(),
            AgentConfigProvision { .. } => "failed to provision agent config".to_owned(),
            FileRemove { .. } => "failed to remove file".to_owned(),
            PermissionSet { .. } => "failed to set owner-only permissions".to_owned(),
            MissingParentDir { .. } => "path has no parent directory".to_owned(),
            ResetNotConfirmed => "reset requires --yes".to_owned(),
            State(_) => "state database error".to_owned(),
            IncompatibleStateSchema { .. } => {
                "state schema is newer than this binary supports".to_owned()
            }
            UnmanagedStateTable { table } => {
                format!("existing state table `{table}` is not managed by a recorded migration")
            }
            MigrationManifestParse(_) => "migration manifest is invalid".to_owned(),
            InvalidManifestOrder { .. } => {
                "migration manifest ids must be strictly increasing positive integers".to_owned()
            }
            ManifestRegistryMismatch { .. } => {
                "migration manifest does not match the compiled registry".to_owned()
            }
            MissingMigratedTable { table } => {
                format!("state database is missing required table `{table}`")
            }
            InvalidEventPayload => "event payload must be valid JSON text".to_owned(),
            InvalidAuthFailurePayload => "auth failure payload must be valid JSON text".to_owned(),
            AgeKeyRead { .. } | SecretStoreRead { .. } => {
                "failed to read secret material".to_owned()
            }
            AgeKeyWrite { .. } | SecretStoreWrite { .. } => {
                "failed to write secret material".to_owned()
            }
            AgeKeyParse { .. } => "age key is malformed".to_owned(),
            SecretStoreEncrypt(_) => "failed to encrypt secret store".to_owned(),
            SecretStoreDecrypt(_) => "failed to decrypt secret store".to_owned(),
            SecretStorePlaintextParse(_)
            | SecretStorePlaintextSerialize(_)
            | SecretStorePlaintextNotUtf8 { .. } => "secret store plaintext is invalid".to_owned(),
            SecretNotFound { .. } => "secret was not found".to_owned(),
            MissingSessionKey { .. } => "secret store is missing session key reference".to_owned(),
            MissingAdminKey { .. } => "secret store is missing admin key reference".to_owned(),
            MissingSupabaseServiceRoleKey { .. } => {
                "secret store is missing Supabase service-role key reference".to_owned()
            }
            InvalidSupabaseUrl { .. } => {
                "[logging.supabase].url must start with `https://`".to_owned()
            }
            InvalidSupabaseSchema { .. } => {
                "[logging.supabase].schema is not a safe Postgres identifier".to_owned()
            }
            CloudflareManagedNotImplemented => {
                "Cloudflare managed provisioning is not implemented yet; use generated mode"
                    .to_owned()
            }
            InvalidCloudflareMode { .. } => "invalid Cloudflare edge mode".to_owned(),
            InvalidCloudflareExposure { .. } => "invalid Cloudflare exposure mode".to_owned(),
            InvalidCloudflaredDeployment { .. } => {
                "invalid cloudflared deployment mode".to_owned()
            }
            InvalidCloudflareHostname { .. } => "invalid Cloudflare hostname".to_owned(),
            InvalidCloudflareTunnelName { .. } => {
                "invalid Cloudflare tunnel name".to_owned()
            }
            InvalidCloudflareTunnelId { .. } => "invalid Cloudflare tunnel id".to_owned(),
            SupabaseSinkHttp { status, .. } => {
                format!("Supabase sink rejected upload with HTTP {status}")
            }
            SupabaseSinkUnknownTable { table } => {
                format!("Supabase sink received a row for unknown source table `{table}`")
            }
            StdinRead { .. } => "failed to read stdin".to_owned(),
            MissingSection { section } => format!("missing required section `{section}`"),
            MissingField { field } => format!("{field} is required"),
            InvalidWorkspaceSourceField { field, source_type } => {
                format!("{field} is not valid when workspace.source.type is {source_type}")
            }
            InvalidConfigFieldForType {
                field,
                type_field,
                type_value,
            } => {
                format!("{field} is not valid when {type_field} is {type_value}")
            }
            InvalidSocketAddress { field } => format!("{field} must be a socket address"),
            NonZeroRequired { field } => format!("{field} must be greater than zero"),
            PathMustBeAbsolute { field } => format!("{field} must be absolute"),
            PathContainsParentDir { field } => format!("{field} must not contain `..` segments"),
            InvalidWorkspaceSourceType => {
                "workspace.source.type must be one of none, git, s3".to_owned()
            }
            InvalidAgentRestart => "agent.restart must be one of never, on-crash".to_owned(),
            InvalidExpectedSha256 => {
                "agent.expected_sha256 must be exactly 64 lowercase hex characters".to_owned()
            }
            InvalidAgentInstallType => {
                "agent.install.type must be `shell` (the only operator-facing install type)".to_owned()
            }
            UrlMustBeHttp { field } => format!("{field} must start with http:// or https://"),
            UrlMustBeHttps { field } => format!("{field} must start with https://"),
            AuthRefsNotDistinct => {
                "auth.session_key_ref and auth.admin_key_ref must be different names".to_owned()
            }
            SecretReservedForAuth { .. } => "secret is reserved for auth".to_owned(),
            ImportChangesAuthRef { .. } => {
                "config import would change auth key references".to_owned()
            }
            ServeBind { .. } => "failed to bind HTTP listener".to_owned(),
            ServeIo { .. } => "HTTP server error".to_owned(),
            ServeRefusedAsRoot => "refusing to run as root without explicit opt-in".to_owned(),
            ServeRootRequiresAdminKey => {
                "running as root requires a non-empty admin API key".to_owned()
            }
            AgentNotConfigured => {
                "agent is not configured; declare [agent].id matching a registry entry, or provide an [agent.install] shell recipe"
                    .to_owned()
            }
            AgentInstallerFailed { exit, .. } => match exit {
                Some(code) => format!("agent installer exited with status {code}"),
                None => "agent installer terminated without an exit status".to_owned(),
            },
            AgentInstallerCreatesMissing { name } => {
                format!("agent installer ran but `creates = {name}` did not resolve afterwards")
            }
            AgentInstallerTimeout => "agent installer hit the configured timeout".to_owned(),
            AgentRegistryMissing { id } => {
                format!("ACP registry does not contain agent `{id}`")
            }
            AgentUnsupported { name } => {
                format!("{name} is not currently supported. Please try a different agent.")
            }
            RegistryLoad { reason } => format!("agent registry could not be loaded: {reason}"),
            GithubReleaseFetch { repo, .. } => {
                format!("failed to query GitHub Releases for {repo}")
            }
            GithubReleaseAssetNotFound { repo, pattern } => {
                format!("no release asset for {repo} matched pattern `{pattern}`")
            }
            GithubReleaseAssetAmbiguous {
                repo,
                pattern,
                matches,
            } => format!(
                "{matches} release assets for {repo} matched pattern `{pattern}`; expected exactly one"
            ),
            GithubReleaseArchiveExtract { repo, reason } => {
                format!("failed to extract release archive from {repo}: {reason}")
            }
            GithubReleaseChecksumMismatch {
                repo,
                asset,
                expected,
                actual,
            } => format!(
                "release asset `{asset}` from {repo} failed sha256 verification: expected {expected}, got {actual}"
            ),
            UnsupportedHostArch { arch } => {
                format!("unsupported host architecture `{arch}` for GitHub Release install")
            }
            AgentSha256Mismatch { expected, actual } => {
                format!("agent binary sha256 mismatch: expected {expected}, got {actual}")
            }
            AgentSpawnFailed { .. } => "failed to spawn agent subprocess".to_owned(),
            AgentAlreadyRunning => "agent is already running".to_owned(),
            AgentNotRunning => "agent is not running".to_owned(),
            AgentInitializeFailed { reason } => {
                format!("agent failed to initialize: {reason}")
            }
            AgentNotInitialized => "agent has not been initialized yet".to_owned(),
            AgentUnsupportedCapability { name } => {
                format!("agent does not support `{name}`")
            }
            AgentApiRequest { path, .. } => {
                format!("agent API request to {path} failed")
            }
            AgentApiStatus { path, status, .. } => {
                format!("agent API request to {path} failed with status {status}")
            }
            AgentRequestFailed { method, .. } => {
                format!("agent rejected `{method}` request")
            }
            SessionNotFound { id } => format!("session `{id}` was not found"),
            SessionClosed { id } => format!("session `{id}` is closed"),
            PromptNotFound { id } => format!("prompt `{id}` was not found"),
            PromptSessionMismatch {
                session_id,
                prompt_id,
            } => format!("session `{session_id}` does not own prompt `{prompt_id}`"),
            PromptBodyEmpty => "prompt body must include at least one content block".to_owned(),
            PromptBodyInvalid(_) => "prompt body is not valid ACP content".to_owned(),
            WorkspacePathInvalid { reason, .. } => format!("workspace path is invalid: {reason}"),
            WorkspaceSymlinkEscape { .. } => {
                "workspace path resolves outside the workspace root".to_owned()
            }
            WorkspaceNotFound { requested } => {
                format!("workspace path `{requested}` was not found")
            }
            WorkspaceTooLarge { limit } => {
                format!("workspace file exceeds the {limit}-byte size limit")
            }
            WorkspaceUploadInvalid { reason } => format!("workspace upload is invalid: {reason}"),
            WorkspaceIo { .. } => "workspace I/O failed".to_owned(),
            WorkspaceEncodingInvalid { reason } => {
                format!("workspace file encoding is invalid: {reason}")
            }
            WorkspaceUploadsNotUnderRoot => {
                "workspace.uploads must be inside workspace.root".to_owned()
            }
            InvalidPermissionsMode => {
                "permissions.mode must be one of auto, supervised, locked".to_owned()
            }
            InvalidDurationField { field } => {
                format!("{field} must be a duration like \"10m\", \"5s\", or \"100ms\"")
            }
            InvalidEnvName { name } => {
                format!("env variable name `{name}` is not a valid POSIX identifier")
            }
            CommandNotFound { id } => format!("command `{id}` was not found"),
            CommandDenied { reason } => format!("command rejected by policy: {reason}"),
            CommandCwdOutsideWorkspace { requested } => {
                format!("command cwd `{requested}` resolves outside the workspace root")
            }
            CommandEnvNotAllowed { name } => {
                format!("command env variable `{name}` is not on commands.env_allowlist")
            }
            CommandSpawnFailed { .. } => "failed to spawn command subprocess".to_owned(),
            CommandTimeout => {
                "command timed out before the subprocess produced an exit status".to_owned()
            }
            SecretRefReservedForAuth { ref_name, kind } => {
                format!("secret ref `{ref_name}` (from {kind}) collides with the auth key ref")
            }
            InvalidSecretRefName { name } => format!("secret ref name `{name}` is invalid"),
            DuplicateSecretRef { name } => {
                format!("secret ref `{name}` is declared more than once")
            }
            InvalidTimeoutAction => {
                "permissions.timeout_action must be one of deny, approve".to_owned()
            }
            InvalidTrustedProxy { value } => {
                format!("security.http.trusted_proxies entry `{value}` is not a valid IP address")
            }
            InvalidMcpServer { name, reason } => {
                format!("mcp.servers entry `{name}` is invalid: {reason}")
            }
            DuplicateMcpServer { name } => {
                format!("mcp.servers contains duplicate name `{name}`")
            }
            DependencyMissingName { category } => {
                format!("dependencies.{category} entry has empty name")
            }
            DuplicateDependency { category, name } => {
                format!("dependencies.{category} contains duplicate name `{name}`")
            }
            PermissionNotFound { id } => format!("permission `{id}` was not found"),
            InvalidPermissionTransition { id, from, to } => {
                format!("permission `{id}` cannot transition from `{from}` to `{to}`")
            }
            StateInvalidJson { field, .. } => format!("durable JSON corruption in `{field}`"),
            RateLimited => "rate limit exceeded".to_owned(),
            IpBlocked { .. } => "client IP is temporarily blocked".to_owned(),
            OriginNotAllowed { .. } => "origin is not allowed".to_owned(),
            InvalidParam { field, reason } => format!("invalid parameter `{field}`: {reason}"),
        }
    }

    /// HTTP status code for this error when rendered through the API envelope.
    /// Coarse mapping: client-provided invalid input is 4xx; failures the
    /// server hits internally (filesystem, sqlite, age decrypt) are 5xx.
    pub fn http_status(&self) -> StatusCode {
        use StackError::*;
        match self {
            // Client-supplied bad input
            ConfigToml(_)
            | ConfigSerialize(_)
            | ImportBase64Decode { .. }
            | ImportUtf8 { .. }
            | ResetNotConfirmed
            | InvalidEventPayload
            | InvalidAuthFailurePayload
            | MissingSection { .. }
            | MissingField { .. }
            | InvalidWorkspaceSourceField { .. }
            | InvalidConfigFieldForType { .. }
            | InvalidSocketAddress { .. }
            | NonZeroRequired { .. }
            | PathMustBeAbsolute { .. }
            | PathContainsParentDir { .. }
            | InvalidWorkspaceSourceType
            | InvalidAgentRestart
            | InvalidExpectedSha256
            | InvalidAgentInstallType
            | UrlMustBeHttp { .. }
            | UrlMustBeHttps { .. }
            | AuthRefsNotDistinct
            | SecretReservedForAuth { .. }
            | ImportChangesAuthRef { .. }
            | CloudflareManagedNotImplemented
            | InvalidCloudflareMode { .. }
            | InvalidCloudflareExposure { .. }
            | InvalidCloudflaredDeployment { .. }
            | InvalidCloudflareHostname { .. }
            | InvalidCloudflareTunnelName { .. }
            | InvalidCloudflareTunnelId { .. }
            | InvalidPermissionsMode
            | InvalidDurationField { .. }
            | InvalidEnvName { .. }
            | CommandDenied { .. }
            | CommandCwdOutsideWorkspace { .. }
            | CommandEnvNotAllowed { .. } => StatusCode::BAD_REQUEST,
            // Not found / conflict
            SecretNotFound { .. } => StatusCode::NOT_FOUND,
            ConfigExists { .. } => StatusCode::CONFLICT,
            // Everything else is a server-side fault. Includes startup-time
            // issues (missing keys, unreadable secret store) that can surface
            // if a handler ever rebuilds state mid-flight.
            HomeNotSet
            | ConfigRead { .. }
            | ConfigWrite { .. }
            | ConfigInitialize { .. }
            | AgentConfigProvision { .. }
            | DirectoryCreate { .. }
            | FileCreate { .. }
            | FileRemove { .. }
            | PermissionSet { .. }
            | MissingParentDir { .. }
            | State(_)
            | IncompatibleStateSchema { .. }
            | UnmanagedStateTable { .. }
            | MigrationManifestParse(_)
            | InvalidManifestOrder { .. }
            | ManifestRegistryMismatch { .. }
            | MissingMigratedTable { .. }
            | AgeKeyRead { .. }
            | AgeKeyWrite { .. }
            | AgeKeyParse { .. }
            | SecretStoreRead { .. }
            | SecretStoreWrite { .. }
            | SecretStoreEncrypt(_)
            | SecretStoreDecrypt(_)
            | SecretStorePlaintextParse(_)
            | SecretStorePlaintextSerialize(_)
            | SecretStorePlaintextNotUtf8 { .. }
            | MissingSessionKey { .. }
            | MissingAdminKey { .. }
            | MissingSupabaseServiceRoleKey { .. }
            | InvalidSupabaseUrl { .. }
            | InvalidSupabaseSchema { .. }
            | SupabaseSinkHttp { .. }
            | SupabaseSinkUnknownTable { .. }
            | StdinRead { .. }
            | ServeBind { .. }
            | ServeIo { .. }
            | ServeRefusedAsRoot
            | ServeRootRequiresAdminKey => StatusCode::INTERNAL_SERVER_ERROR,
            // Agent-related: classify client-facing vs internal vs upstream.
            AgentNotConfigured => StatusCode::BAD_REQUEST,
            AgentUnsupported { .. } => StatusCode::BAD_REQUEST,
            AgentAlreadyRunning | AgentNotRunning => StatusCode::CONFLICT,
            AgentNotInitialized => StatusCode::NOT_FOUND,
            AgentUnsupportedCapability { .. } => StatusCode::NOT_IMPLEMENTED,
            // The agent is upstream: handshake failures are gateway errors,
            // not our internal faults.
            AgentInitializeFailed { .. } => StatusCode::BAD_GATEWAY,
            AgentInstallerFailed { .. }
            | AgentInstallerCreatesMissing { .. }
            | AgentInstallerTimeout
            | AgentRegistryMissing { .. }
            | RegistryLoad { .. }
            | GithubReleaseFetch { .. }
            | GithubReleaseAssetNotFound { .. }
            | GithubReleaseAssetAmbiguous { .. }
            | GithubReleaseArchiveExtract { .. }
            | GithubReleaseChecksumMismatch { .. }
            | UnsupportedHostArch { .. }
            | AgentSha256Mismatch { .. }
            | AgentSpawnFailed { .. }
            | AgentApiRequest { .. }
            | AgentApiStatus { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            // Session/prompt errors map to the standard HTTP shapes:
            // not found, conflict for state, gateway for upstream agent errors,
            // and bad request for client-provided malformed payloads.
            SessionNotFound { .. } | PromptNotFound { .. } => StatusCode::NOT_FOUND,
            SessionClosed { .. } | PromptSessionMismatch { .. } => StatusCode::CONFLICT,
            PromptBodyEmpty | PromptBodyInvalid(_) => StatusCode::BAD_REQUEST,
            AgentRequestFailed { .. } => StatusCode::BAD_GATEWAY,
            // Workspace: client-supplied path / encoding / upload-shape problems
            // are 400; missing files are 404; size cap exceeded is 413; the
            // underlying I/O error is an internal fault (the path itself was
            // already validated client-side).
            WorkspacePathInvalid { .. }
            | WorkspaceSymlinkEscape { .. }
            | WorkspaceUploadInvalid { .. }
            | WorkspaceEncodingInvalid { .. } => StatusCode::BAD_REQUEST,
            WorkspaceNotFound { .. } => StatusCode::NOT_FOUND,
            WorkspaceTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            WorkspaceIo { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            WorkspaceUploadsNotUnderRoot => StatusCode::BAD_REQUEST,
            CommandNotFound { .. } => StatusCode::NOT_FOUND,
            CommandSpawnFailed { .. } | CommandTimeout => StatusCode::INTERNAL_SERVER_ERROR,
            SecretRefReservedForAuth { .. }
            | InvalidSecretRefName { .. }
            | DuplicateSecretRef { .. }
            | InvalidTimeoutAction
            | InvalidTrustedProxy { .. }
            | InvalidMcpServer { .. }
            | DuplicateMcpServer { .. }
            | DependencyMissingName { .. }
            | DuplicateDependency { .. }
            | InvalidPermissionTransition { .. } => StatusCode::BAD_REQUEST,
            PermissionNotFound { .. } => StatusCode::NOT_FOUND,
            StateInvalidJson { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            RateLimited | IpBlocked { .. } => StatusCode::TOO_MANY_REQUESTS,
            OriginNotAllowed { .. } => StatusCode::FORBIDDEN,
            InvalidParam { .. } => StatusCode::BAD_REQUEST,
        }
    }
}

pub type Result<T> = std::result::Result<T, StackError>;
