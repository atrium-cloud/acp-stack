use http::StatusCode;
use std::path::PathBuf;

mod agent_install;
mod agent_runtime;
mod archive;
mod auth_http;
mod command;
mod config;
mod download;
mod edge;
mod permission;
mod secrets;
mod security;
mod serve;
mod session;
mod state;
mod supabase;
mod workspace;
mod workspace_source;

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    // === config / io / import ===
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

    // === state / migrations ===
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

    // === secrets / age key store ===
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
        "secret store is non-empty but does not contain the Supabase secret API key reference `{name}`"
    )]
    MissingSupabaseApiKey { name: String },

    // === supabase logging sink ===
    #[error(
        "[logging.supabase].url must start with `https://` when external logging is enabled; got `{url}`"
    )]
    InvalidSupabaseUrl { url: String },

    #[error(
        "[logging.supabase].schema must be a safe Postgres identifier matching `^[a-z_][a-z0-9_]{{0,62}}$`; got `{schema}`"
    )]
    InvalidSupabaseSchema { schema: String },

    // === edge (cloudflare) ===
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

    // === supabase sink runtime ===
    #[error("Supabase sink rejected upload: {status} {body}")]
    SupabaseSinkHttp { status: u16, body: String },

    #[error("Supabase sink received a row for unknown source table `{table}`; refusing to upload")]
    SupabaseSinkUnknownTable { table: String },

    // === stdin / generic config ===
    #[error("failed to read stdin: {source}")]
    StdinRead { source: std::io::Error },

    #[error("missing required section `{section}`")]
    MissingSection { section: &'static str },

    #[error("{field} is required")]
    MissingField { field: &'static str },

    // === workspace_source (init-time materialization) ===
    #[error("workspace.code_sources[{index}]: {reason}")]
    WorkspaceCodeSourceInvalid { index: usize, reason: String },

    #[error("workspace.data_sources[{index}]: {reason}")]
    WorkspaceDataSourceInvalid { index: usize, reason: String },

    #[error(
        "workspace destination `{dest}` is not empty and is not a known acp-stack source directory"
    )]
    WorkspaceDestinationNotEmpty { dest: String },

    #[error("workspace destination `{dest}` is outside workspace.root `{root}`")]
    WorkspaceDestinationOutsideRoot { dest: String, root: String },

    #[error("workspace materialization failed: {reason}")]
    WorkspaceMaterializeFailed { reason: String },

    #[error("{}", workspace_command_failed_message(command, *exit, stderr_tail))]
    WorkspaceCommandFailed {
        command: &'static str,
        exit: Option<i32>,
        stderr_tail: String,
    },

    // === download (https fetch) ===
    #[error("download exceeded the {limit}-byte size limit")]
    SafeDownloadTooLarge { limit: u64 },

    #[error("download URL `{url}` is not allowed (only https:// is permitted)")]
    SafeDownloadInsecureRedirect { url: String },

    #[error("download from {url} failed with HTTP status {status}")]
    SafeDownloadHttpStatus { url: String, status: u16 },

    #[error("download from {url} failed: {reason}")]
    SafeDownloadFailed { url: String, reason: String },

    #[error("downloaded content sha256 mismatch: expected {expected}, got {actual}")]
    SafeDownloadChecksumMismatch { expected: String, actual: String },

    // === archive (tar/zip extraction) ===
    #[error("archive contained an unsafe {kind}: `{name}`")]
    ArchiveUnsafeEntry { kind: &'static str, name: String },

    #[error("archive format is not supported")]
    ArchiveUnsupportedFormat,

    #[error("archive extracted output exceeded the {limit}-byte size limit")]
    ArchiveTooLarge { limit: u64 },

    #[error("archive read failed: {reason}")]
    ArchiveReadFailed { reason: String },

    // === config: generic shape validators ===
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

    // === serve (http listener) ===
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

    // === agent install / registry / release assets ===
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

    #[error("failed to persist installer log at {path}: {source}")]
    AgentInstallerLogPersist {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("ACP registry does not contain agent `{id}`")]
    AgentRegistryMissing { id: String },

    #[error("init run state is corrupted: {reason}")]
    InitRunCorrupted { reason: String },

    #[error("{name} is not currently supported. Please try a different agent.")]
    AgentUnsupported { name: String },

    #[error(
        "one or more managed agent components are stale or missing; re-run `acps agent install` to upgrade"
    )]
    AgentCheckStale,

    #[error("agent registry could not be loaded: {reason}")]
    RegistryLoad { reason: String },

    #[error("invalid skill source `{source_id}`")]
    SkillInstallInvalidSource { source_id: String },

    #[error("skill source `{source_id}` is not available")]
    SkillInstallSourceMissing { source_id: String },

    #[error("invalid skill name `{name}`")]
    SkillInstallInvalidName { name: String },

    #[error("skill `{skill}` was not found in source `{source_id}`")]
    SkillInstallSkillMissing { source_id: String, skill: String },

    #[error("skill install target conflict at {path}: {reason}")]
    SkillInstallTargetConflict { path: PathBuf, reason: String },

    #[error("skill install failed: {reason}")]
    SkillInstallFailed { reason: String },

    #[error("failed to query GitHub Releases for {repo}: {source}")]
    GithubReleaseFetch {
        repo: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("failed to query npm registry for `{package}`: {source}")]
    NpmRegistryFetch {
        package: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("npm registry returned an empty version for `{package}`")]
    NpmRegistryEmptyVersion { package: String },

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

    // === agent runtime / lifecycle ===
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

    /// Agent's upstream inference endpoint surfaced an HTTP failure. Carries
    /// only the parsed status code and a vetted `'static` reason label, so the
    /// raw upstream message (URLs, headers, bodies, secrets) never reaches the
    /// state store or events.
    #[error("inference endpoint returned {status_code} ({reason_category})")]
    InferenceRequestFailed {
        status_code: u16,
        reason_category: &'static str,
    },

    #[error("agent test failed at {stage}: {reason}")]
    AgentTestFailed { stage: String, reason: String },

    // === session / prompt ===
    #[error("session `{id}` was not found")]
    SessionNotFound { id: String },

    #[error("session `{id}` is closed")]
    SessionClosed { id: String },

    #[error("session `{id}` is {status} and must be loaded or resumed before prompting")]
    SessionNotActive { id: String, status: String },

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

    // === workspace (runtime path access) ===
    #[error("workspace path `{requested}` is invalid: {reason}")]
    WorkspacePathInvalid { reason: String, requested: String },

    #[error("workspace path `{requested}` resolves outside the workspace root")]
    WorkspaceSymlinkEscape { requested: String },

    #[error("workspace path `{requested}` was not found")]
    WorkspaceNotFound { requested: String },

    #[error("workspace parent directory for `{requested}` was not found")]
    WorkspaceParentNotFound { requested: String },

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

    // === permissions / mcp / dependencies config ===
    #[error("permissions.mode must be one of auto, supervised, locked")]
    InvalidPermissionsMode,

    #[error("{field} must be a duration like \"10m\", \"5s\", or \"100ms\"")]
    InvalidDurationField { field: &'static str },

    #[error("env variable name `{name}` is not a valid POSIX identifier")]
    InvalidEnvName { name: String },

    // === command gateway ===
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

    // === secrets: ref-shape validation ===
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

    // === permission / mcp / dependencies runtime + config ===
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

    // === state-layer JSON corruption ===
    #[error("durable JSON corruption in `{field}`: {reason}")]
    StateInvalidJson { field: &'static str, reason: String },

    // === security self-check history ===
    #[error("security run `{id}` was not found")]
    SecurityRunNotFound { id: String },

    #[error("security run `{run_id}` finding {ordinal} has unreadable details_json: {source}")]
    SecurityFindingDetailsCorrupt {
        run_id: String,
        ordinal: i64,
        source: serde_json::Error,
    },

    // === auth_http (HTTP-edge auth) ===
    #[error("rate limit exceeded; retry later")]
    RateLimited,

    #[error("IP `{ip}` is temporarily blocked due to repeated auth failures")]
    IpBlocked { ip: String },

    #[error("Origin `{origin}` is not in the configured allowlist")]
    OriginNotAllowed { origin: String },

    // === config import shape ===
    #[error("config import exceeds {limit}-byte size limit ({actual} bytes)")]
    ImportTooLarge { limit: usize, actual: usize },

    #[error("unsupported config version {version}; this binary only supports version 1")]
    UnsupportedConfigVersion { version: u64 },

    #[error(
        "secret ref at `{field}` looks like an inline secret value rather than a reference name"
    )]
    SecretRefLooksLikeValue { field: &'static str },
}

fn workspace_command_failed_message(command: &str, exit: Option<i32>, stderr_tail: &str) -> String {
    match exit {
        Some(code) => format!("`{command}` exited with status {code}: {stderr_tail}"),
        None => format!("`{command}` exited without a status: {stderr_tail}"),
    }
}

impl StackError {
    /// Dotted-namespace code suitable for the HTTP error envelope at
    /// `docs/specs/api/api.md:20-42`. Delegates to per-domain helpers so the
    /// variant-to-code table lives next to the matching domain.
    pub fn error_code(&self) -> &'static str {
        config::error_code(self)
            .or_else(|| state::error_code(self))
            .or_else(|| security::error_code(self))
            .or_else(|| secrets::error_code(self))
            .or_else(|| supabase::error_code(self))
            .or_else(|| edge::error_code(self))
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

pub type Result<T> = std::result::Result<T, StackError>;

#[cfg(test)]
mod tests {
    use super::StackError;

    #[test]
    fn workspace_command_failure_display_formats_exit_status_plainly() {
        let exited = StackError::WorkspaceCommandFailed {
            command: "git clone",
            exit: Some(128),
            stderr_tail: "repository not found".to_owned(),
        }
        .to_string();
        assert_eq!(
            exited,
            "`git clone` exited with status 128: repository not found"
        );
        assert!(
            !exited.contains("Some("),
            "exit status must not expose Option debug formatting: {exited}"
        );

        let signaled = StackError::WorkspaceCommandFailed {
            command: "git clone",
            exit: None,
            stderr_tail: "terminated by signal".to_owned(),
        }
        .to_string();
        assert_eq!(
            signaled,
            "`git clone` exited without a status: terminated by signal"
        );
    }
}
