use http::StatusCode;
use std::path::PathBuf;

mod agent_install;
mod agent_runtime;
mod archive;
mod auth_http;
mod command;
mod config;
mod dispatch;
mod download;
mod edge;
mod extensions;
mod node_runtime;
mod permission;
mod sandbox;
mod secrets;
mod security;
mod serve;
mod session;
mod state;
mod supabase;
mod workload_fs;
mod workspace;
mod workspace_source;

pub use self::agent_runtime::{AcpErrorDetail, AgentRequestContext};

// CONSTANTS

/// Cap on an error report kept in a persisted row or event payload.
pub const PERSISTED_ERROR_MAX_BYTES: usize = 2048;

use self::agent_install::stack_update_rollback_suffix;
use self::workspace::workspace_command_failed_message;

#[derive(Debug, thiserror::Error)]
pub enum StackError {
    // === config / io / import ===
    #[error("HOME is not set; cannot resolve default config path")]
    HomeNotSet,

    #[error(
        "fixture build refuses HOME {path}: it is outside the temp dir; set ACP_STACK_TEST_DISPOSABLE_HOST=1 only on a throwaway host"
    )]
    HomeNotIsolated { path: PathBuf },

    #[error(
        "fixture build refuses non-loopback URL {url}; set ACP_STACK_TEST_DISPOSABLE_HOST=1 only on a throwaway host"
    )]
    FixtureEgressRefused { url: String },

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

    #[error("provider `{provider}` model catalog fetch failed: {reason}")]
    ProviderModelCatalog { provider: String, reason: String },

    #[error("native Agent config import failed ({code})")]
    NativeAgentConfig { code: &'static str },

    /// `detail` is internal context for the daemon log and CLI output, kept out of the public
    /// message: a parse error's category and location, or the cause a bare code would drop. It
    /// never carries native config values.
    #[error("native Agent config import failed ({code}): {detail}")]
    NativeAgentConfigDetailed { code: &'static str, detail: String },

    #[error("native Agent config import failed ({code})")]
    NativeAgentConfigOperationFailed { code: String },

    #[error("sandbox setup failed: {reason}")]
    SandboxFailed { reason: String },

    // === sandbox workload identity ===
    #[error("[workspace.sandbox].workload_user `{name}` does not resolve to a local user")]
    WorkloadUserUnresolved { name: String },

    #[error("[workspace.sandbox].workload_user `{name}` lookup failed: {source}")]
    WorkloadUserLookupFailed {
        name: String,
        source: std::io::Error,
    },

    #[error(
        "[workspace.sandbox].workload_user `{name}` resolves to uid {uid}, the runtime's own uid; declare a separate user"
    )]
    WorkloadUserIsRuntime { name: String, uid: u32 },

    #[error(
        "[workspace.sandbox].workload_user `{name}` resolves to uid 0; declare an unprivileged user"
    )]
    WorkloadUserIsRoot { name: String },

    #[error(
        "[workspace.sandbox].workload_user `{name}` has primary gid {gid}, shared with root or the runtime; give the workload user its own primary group"
    )]
    WorkloadUserSharesGroup { name: String, gid: u32 },

    #[error(
        "[workspace.sandbox].workload_user requires mode = \"unshare\" or \"off\"; mode \"{mode}\" cannot run the workload as another user"
    )]
    WorkloadUserModeUnsupported { mode: &'static str },

    #[error(
        "[workspace.sandbox].require_network_provider is set but no network-provider extension is declared; refusing to spawn with host networking"
    )]
    NetworkProviderRequired,

    #[error("{subject} is not reachable by the workload user at {path}: {reason}")]
    WorkloadUnreachable {
        subject: String,
        path: PathBuf,
        reason: String,
    },

    #[error(
        "{subject} at {path} is writable by the workload user; refusing to run it as the runtime"
    )]
    WorkloadWritableExecutable { subject: String, path: PathBuf },

    // === extensions ===
    #[error("no managed-state extension named `{name}` is declared")]
    ExtensionNamespaceUnknown { name: String },

    #[error("managed-state apply for `{namespace}` conflicts: {reason}")]
    ExtensionRevisionConflict { namespace: String, reason: String },

    #[error(
        "managed-state namespace `{namespace}` cannot take ownership of provider `{provider_id}`: {reason}"
    )]
    ExtensionStateOwnership {
        namespace: String,
        provider_id: String,
        reason: String,
    },

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

    #[error("decrypted secret store plaintext is invalid: {reason}")]
    SecretStorePlaintextInvalid { reason: String },

    #[error("secret `{name}` was not found in the secret store")]
    SecretNotFound { name: String },

    #[error(
        "provider secret `{env_ref}` is missing and the declared managed credential push cannot deliver it for provider `{provider_id}`: the push writes only the env vars the agent reads for this provider. Store it with `acps secrets set {env_ref}` before init, or reference the provider's push-delivered api-key variable."
    )]
    ProviderSecretNotPushDeliverable {
        provider_id: String,
        env_ref: String,
    },

    #[error(
        "provider credential change failed ({original}); credential catalog rollback also failed ({rollback})"
    )]
    ProviderCredentialRollbackFailed { original: String, rollback: String },

    #[error(
        "secret store is non-empty but does not contain the Supabase secret API key reference `{name}`"
    )]
    MissingSupabaseApiKey { name: String },

    #[error(
        "secret store is non-empty but does not contain the Supabase Postgres writer DB URL reference `{name}`"
    )]
    MissingSupabaseDbUrl { name: String },

    // === supabase logging sink ===
    #[error(
        "[logging.supabase].url must start with `https://` when external logging is enabled; got `{url}`"
    )]
    InvalidSupabaseUrl { url: String },

    #[error(
        "[logging.supabase].schema must be a safe Postgres identifier matching `^[a-z_][a-z0-9_]{{0,62}}$`; got `{schema}`"
    )]
    InvalidSupabaseSchema { schema: String },

    #[error(
        "[logging.supabase].table_prefix must be empty or a safe Postgres identifier prefix matching `^[a-z_][a-z0-9_]*$`, with enough room for mirrored table names; got `{prefix}`"
    )]
    InvalidSupabaseTablePrefix { prefix: String },

    // === edge (cloudflare) ===
    #[error("edge.cloudflare.mode must be one of generated, managed; got `{mode}`")]
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

    #[error("Cloudflare managed provisioning failed at {operation}: {reason}")]
    CloudflareManagedProvision {
        operation: &'static str,
        reason: String,
    },

    #[error("Cloudflare API rejected {operation}: HTTP {status} {body}")]
    CloudflareApiStatus {
        operation: &'static str,
        status: u16,
        body: String,
    },

    // === supabase sink runtime ===
    #[error("Supabase sink rejected upload: {status} {body}")]
    SupabaseSinkHttp { status: u16, body: String },

    #[error("Supabase sink received a row for unknown source table `{table}`; refusing to upload")]
    SupabaseSinkUnknownTable { table: String },

    #[error("Supabase CLI setup failed at `{command}` with status {status}: {stderr_tail}")]
    SupabaseCliFailed {
        command: String,
        status: String,
        stderr_tail: String,
    },

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

    /// `body` is the redacted, bounded response body.
    #[error("download from {url} failed with HTTP status {status}: {body}")]
    SafeDownloadHttpStatus {
        url: String,
        status: u16,
        body: String,
    },

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

    // === node runtime (managed Node.js) ===
    #[error("Node.js archive `{archive}` sha256 mismatch: expected {expected}, got {actual}")]
    NodeRuntimeChecksumMismatch {
        archive: String,
        expected: String,
        actual: String,
    },

    #[error("managed Node.js runtime install failed: {reason}")]
    NodeRuntimeInstallFailed { reason: String },

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

    #[error("{field} must start with https://")]
    UrlMustBeHttps { field: &'static str },

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

    #[error("refusing to run as root; use `acps dev serve --allow-root` only for development")]
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

    #[error("agent installer produced `{path}` but it cannot be spawned on this host: {source}")]
    AgentInstallerBinaryUnrunnable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("agent installer prerequisites missing for `{agent_id}` {step}: {tools:?}")]
    AgentInstallerPrerequisitesMissing {
        agent_id: String,
        step: String,
        tools: Vec<String>,
    },

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

    #[error(
        "config has legacy `agent.id = \"placeholder\"`; select a real agent with `acps init --fresh --agent <id>`"
    )]
    AgentPlaceholderConfigured,

    #[error("init run state is corrupted: {reason}")]
    InitRunCorrupted { reason: String },

    #[error("init step `{kind}` panicked: {message}")]
    InitStepPanicked { kind: String, message: String },

    #[error(
        "dependency apply produced failing actions: {summary}; inspect `acps installer history --agent deps_apply` (apply_run_id={apply_run_id})"
    )]
    DepsApplyFailed {
        summary: String,
        apply_run_id: String,
        /// Retry command for the CLI surface that raised the error.
        retry_command: &'static str,
    },

    #[error("a dependency apply is already running (apply_run_id={apply_run_id})")]
    DepsApplyInFlight { apply_run_id: String },

    #[error("no dependency apply run matches `{apply_run_id}`")]
    DepsApplyRunNotFound { apply_run_id: String },

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

    #[error("skill `{skill}` is not installed")]
    SkillNotInstalled { skill: String },

    #[error("skill source `{alias}` is not configured")]
    SkillSourceNotConfigured { alias: String },

    #[error("all install paths failed: {summary}")]
    AgentInstallAllPathsFailed { summary: String },

    #[error("failed to inspect agent binary `{path}`: {source}")]
    AgentBinaryInspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A version pin on an agent CLI whose install lanes cannot fetch a chosen release.
    #[error("agent `{agent_id}` cannot install version `{version}` of its CLI: {reason}")]
    AgentVersionUnsupported {
        agent_id: String,
        version: String,
        reason: &'static str,
    },

    #[error("requests to {domain} are rate limited; retry in {retry_after_secs}s")]
    DomainRateLimited {
        domain: String,
        retry_after_secs: u64,
    },

    /// `body` is the redacted, bounded body of a non-2xx response; it precedes `source` so
    /// [`report`] still recognizes the trailing source.
    #[error("failed to query GitHub Releases for {repo}{}: {source}", response_body_note(.body))]
    GithubReleaseFetch {
        repo: String,
        #[source]
        source: reqwest::Error,
        body: Option<String>,
    },

    #[error("failed to query npm registry for `{package}`{}: {source}", response_body_note(.body))]
    NpmRegistryFetch {
        package: String,
        #[source]
        source: reqwest::Error,
        body: Option<String>,
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

    #[error(
        "failed to replace {path} during stack update binary swap: {source}{}",
        stack_update_rollback_suffix(rollback_errors)
    )]
    StackUpdateBinarySwap {
        path: PathBuf,
        source: std::io::Error,
        /// Empty when the previous binaries were restored cleanly.
        rollback_errors: Vec<String>,
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

    #[error("agent switch conflict: {reason}")]
    AgentSwitchConflict { reason: String },

    #[error("agent switch journal at {path} is corrupt: {reason}")]
    AgentSwitchJournalCorrupt { path: PathBuf, reason: String },

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

    #[error("array {action} failed for {failed} of {total} target(s): {summary}")]
    ArrayTargetsFailed {
        action: &'static str,
        failed: usize,
        total: usize,
        summary: String,
    },

    /// `message` is acp-stack's own description, or the adapter's redacted message when
    /// `context.acp_error` carries the adapter's JSON-RPC error.
    #[error(
        "{}",
        agent_runtime::agent_request_failed_display(method, message, context)
    )]
    AgentRequestFailed {
        method: &'static str,
        message: String,
        context: Box<AgentRequestContext>,
    },

    /// Carries only a status code and a vetted `'static` label, so raw
    /// upstream text (URLs, headers, bodies, secrets) never reaches the state
    /// store or events.
    #[error("inference endpoint returned {status_code} ({reason_category})")]
    InferenceRequestFailed {
        status_code: u16,
        reason_category: &'static str,
    },

    /// `code` is the machine channel `agent test --format json` reports
    /// instead of `reason`, which embeds workspace paths and spawn argv.
    #[error("agent test failed at {stage}: {reason}")]
    AgentTestFailed {
        stage: String,
        reason: String,
        code: &'static str,
    },

    // === session / prompt ===
    #[error("session `{id}` was not found")]
    SessionNotFound { id: String },

    #[error(
        "cannot rename {count} session(s) from target `{old_target_id}` to `{new_target_id}`: the new target already has session(s) with the same agent session id"
    )]
    SessionTargetRenameConflict {
        old_target_id: String,
        new_target_id: String,
        count: usize,
    },

    #[error("session `{id}` is closed")]
    SessionClosed { id: String },

    /// The id names a session `POST /v1/sessions/{id}/delete` removed; its
    /// tombstone outlives the rows until the retention window prunes it.
    #[error("session `{id}` was deleted")]
    SessionDeleted { id: String },

    /// The running adapter never opened this session and advertises neither
    /// `session/resume` nor `session/load`, so there is no way to re-attach it
    /// before dispatching a session-scoped request.
    #[error(
        "session `{id}` cannot be re-attached: the agent advertises neither `session/resume` nor `session/load`"
    )]
    SessionReattachUnsupported { id: String },

    /// Distinguishes a cursor that no longer resolves (for example after a
    /// checkpoint restore rolled the state database back) from a cursor
    /// sitting at the head, which legitimately pages to an empty result.
    #[error("session `{session_id}` has no event `{cursor_id}` to page after")]
    SessionEventCursorUnknown {
        session_id: String,
        cursor_id: String,
    },

    #[error("session `{session_id}` already has a prompt in flight")]
    PromptInFlight { session_id: String },

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

    #[error("session cwd is invalid: {reason}")]
    SessionCwdInvalid {
        reason: &'static str,
        #[source]
        source: Option<std::io::Error>,
    },

    #[error("model `{model}` does not support prompt input modality `{modality}`")]
    PromptUnsupportedModality { model: String, modality: String },

    // === workspace (runtime path access) ===
    #[error("workspace path `{requested}` is invalid: {reason}")]
    WorkspacePathInvalid { reason: String, requested: String },

    #[error("workspace path `{requested}` goes through a symlink that is not allowed")]
    WorkspaceSymlinkEscape { requested: String },

    #[error("workspace path `{requested}` was not found")]
    WorkspaceNotFound { requested: String },

    #[error("workspace parent directory for `{requested}` was not found")]
    WorkspaceParentNotFound { requested: String },

    #[error("workspace file exceeds the {limit}-byte size limit")]
    WorkspaceTooLarge { limit: u64 },

    #[error("workspace upload is invalid: {reason}")]
    WorkspaceUploadInvalid {
        reason: &'static str,
        #[source]
        source: Option<axum::extract::multipart::MultipartError>,
    },

    #[error("permission denied for workspace path `{requested}`: {source}")]
    WorkspacePermissionDenied {
        requested: String,
        #[source]
        source: std::io::Error,
    },

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

    #[error("{field} must be a duration like \"10m\", \"5s\", \"1d\", \"4w\", or \"100ms\"")]
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
        "secret ref name `{name}` is invalid; use ASCII letters, digits, and underscores, and do not start with a digit"
    )]
    InvalidSecretRefName { name: String },

    #[error("secret ref name `{name}` is declared more than once across the config")]
    DuplicateSecretRef { name: String },

    #[error("secret template at `{field}` is invalid: {reason}")]
    SecretTemplateInvalid {
        field: &'static str,
        reason: &'static str,
    },

    #[error("`{field}` declares env var `{name}` more than once")]
    DuplicateEnvVarName { field: &'static str, name: String },

    #[error("mcp header `{header}` must set exactly one of `value_ref` or `value`")]
    InvalidHeaderValueSource { header: String },

    // === permission / mcp / dependencies runtime + config ===
    #[error("permissions.timeout_action must be one of deny, approve")]
    InvalidTimeoutAction,

    #[error("permissions.acp_prompt_action must be one of ask, approve")]
    InvalidAcpPromptAction,

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

    #[error("security finding severity must be one of \"warning\"|\"critical\", got {severity:?}")]
    SecurityFindingSeverityInvalid { severity: String },

    // === auth_http (HTTP-edge auth) ===
    #[error("rate limit exceeded; retry later")]
    RateLimited,

    #[error("IP `{ip}` is temporarily blocked due to repeated auth failures")]
    IpBlocked { ip: String },

    #[error("Origin `{origin}` is not in the configured allowlist")]
    OriginNotAllowed { origin: String },

    #[error("request body exceeds the configured size limit")]
    RequestTooLarge {
        #[source]
        source: axum::extract::multipart::MultipartError,
    },

    // === config import shape ===
    #[error("config import exceeds {limit}-byte size limit ({actual} bytes)")]
    ImportTooLarge { limit: usize, actual: usize },

    #[error("unsupported config version {version}; this binary only supports version 1")]
    UnsupportedConfigVersion { version: u64 },

    #[error(
        "secret ref at `{field}` looks like an inline secret value rather than a reference name"
    )]
    SecretRefLooksLikeValue { field: &'static str },

    // === workload fs ===
    #[error("workload path {path} is invalid: {reason}")]
    WorkloadFsInvalidPath { path: PathBuf, reason: &'static str },

    #[error("workload path {path} contains a symlink; refusing to follow it")]
    WorkloadFsSymlinkRefused { path: PathBuf },

    #[error("workload file {path} has more than one hard link; refusing it")]
    WorkloadFsHardLinkRefused { path: PathBuf },

    #[error("workload path {path} is not a regular file")]
    WorkloadFsNotRegular { path: PathBuf },

    #[error("workload path {path} was not found")]
    WorkloadFsNotFound { path: PathBuf },

    #[error("workload path {path} already exists")]
    WorkloadFsAlreadyExists { path: PathBuf },

    #[error("workload destination {path} exists and is not an empty directory")]
    WorkloadFsDestinationNotEmpty { path: PathBuf },

    #[error("workload file {path} is owned by uid {actual_uid}, expected uid {expected_uid}")]
    WorkloadFsOwnerMismatch {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },

    #[error("symlink chain at {path} loops or exceeds {max_hops} hops")]
    WorkloadFsSymlinkLoop { path: PathBuf, max_hops: usize },

    #[error("{path} is writable by the workload identity")]
    WorkloadFsWorkloadWritable { path: PathBuf },

    #[error("{path} is not readable and executable by the workload identity")]
    WorkloadFsWorkloadUnreadable { path: PathBuf },

    #[error("failed to enter workload filesystem credentials: {reason}")]
    WorkloadFsCredentialsFailed { reason: String },

    #[error("workload filesystem job failed: {reason}")]
    WorkloadFsExecutorFailed { reason: String },

    #[error("workload filesystem job did not finish within {timeout:?}")]
    WorkloadFsTimeout { timeout: std::time::Duration },

    #[error("workload filesystem {operation} on {path} failed: {source}")]
    WorkloadFsIo {
        path: PathBuf,
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, StackError>;

fn response_body_note(body: &Option<String>) -> String {
    body.as_deref()
        .map(|body| format!(" (response body: {body})"))
        .unwrap_or_default()
}

/// `error`'s Display text followed by each `source()` in its chain, joined by `: `. A source the
/// report already ends with is skipped, since many variants end in `: {source}`; a source that
/// merely appears elsewhere is kept, as repeating a cause beats dropping one.
pub fn report(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !cause_text.is_empty() && !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// A failed native config lock task, with the panic or cancellation kept in the report.
pub fn native_config_lock_task_failed(error: &tokio::task::JoinError) -> StackError {
    StackError::NativeAgentConfigDetailed {
        code: if error.is_panic() {
            "agent.native_config_lock_task_panicked"
        } else {
            "agent.native_config_lock_task_cancelled"
        },
        detail: report(error),
    }
}

/// [`report`] as kept in a persisted row or event payload: redacted, then cut at
/// [`PERSISTED_ERROR_MAX_BYTES`]. Rows and events outlive the daemon log and reach API readers.
pub fn persisted_report(error: &(dyn std::error::Error + 'static)) -> String {
    crate::redaction::bounded(
        &crate::redaction::redact_text(&report(error)),
        PERSISTED_ERROR_MAX_BYTES,
    )
    .into_owned()
}

#[cfg(test)]
mod tests;
