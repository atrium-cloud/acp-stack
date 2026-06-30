use std::path::Path;

use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};

use crate::config::{
    self, CodeSourceConfig, Config, DataSourceConfig, SandboxMode, derive_code_source_name,
    derive_data_source_name,
};
use crate::error::{Result, StackError};
use crate::fs_util::{atomic_write_owner_only, home_dir};
use crate::runtime::sandbox;
use crate::runtime::workspace_sources::workspace_init::{
    MaterializeOutcome, MaterializeReport, materialize_workspace,
};
use crate::secrets::SecretStore;

use super::core::{OutputFormat, print_json};

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    /// Print configured workspace paths, source counts, and sandbox mode.
    Status,
    /// Manage code sources under workspace.root/usr/code.
    CodeSource {
        #[command(subcommand)]
        command: CodeSourceCommand,
    },
    /// Manage data sources under workspace.root/usr/data.
    DataSource {
        #[command(subcommand)]
        command: DataSourceCommand,
    },
    /// Sync configured code and data sources into the workspace.
    Sync,
    /// Inspect or change the workspace sandbox used by supervised agents.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum CodeSourceCommand {
    /// List configured code sources.
    List,
    /// Add a git code source and sync by default.
    Add(CodeSourceAddArgs),
}

#[derive(Debug, Args, Clone)]
pub struct CodeSourceAddArgs {
    /// Git repository URL or absolute local repository path.
    #[arg(long)]
    repo: String,
    /// Git branch to check out.
    #[arg(long)]
    branch: Option<String>,
    /// Secret ref used as the git credential.
    #[arg(long = "credential-ref")]
    credential_ref: Option<String>,
    /// Destination directory name under workspace.root/usr/code.
    #[arg(long)]
    name: Option<String>,
    /// Write config without running workspace sync.
    #[arg(long = "no-sync")]
    no_sync: bool,
}

#[derive(Debug, Subcommand)]
pub enum DataSourceCommand {
    /// List configured data sources.
    List,
    /// Add a local, HTTPS, or S3 data source and sync by default.
    Add(Box<DataSourceAddArgs>),
}

#[derive(Debug, Args, Clone)]
pub struct DataSourceAddArgs {
    /// Source type.
    #[arg(long = "type", value_enum)]
    source_type: DataSourceKindArg,
    /// Destination directory name under workspace.root/usr/data.
    #[arg(long)]
    name: Option<String>,
    /// Absolute local path for type=local.
    #[arg(long)]
    path: Option<String>,
    /// HTTPS URL for type=https.
    #[arg(long)]
    url: Option<String>,
    /// Expected sha256 for type=https downloads.
    #[arg(long = "expected-sha256")]
    expected_sha256: Option<String>,
    /// Maximum download bytes for type=https or type=s3.
    #[arg(long = "max-download-bytes")]
    max_download_bytes: Option<u64>,
    /// Maximum extracted bytes for type=https archives.
    #[arg(long = "max-extracted-bytes")]
    max_extracted_bytes: Option<u64>,
    /// Bucket for type=s3.
    #[arg(long)]
    bucket: Option<String>,
    /// Prefix for type=s3.
    #[arg(long)]
    prefix: Option<String>,
    /// Region for type=s3.
    #[arg(long)]
    region: Option<String>,
    /// Secret ref for the S3 access key.
    #[arg(long = "access-key-ref")]
    access_key_ref: Option<String>,
    /// Secret ref for the S3 secret key.
    #[arg(long = "secret-key-ref")]
    secret_key_ref: Option<String>,
    /// Write config without running workspace sync.
    #[arg(long = "no-sync")]
    no_sync: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DataSourceKindArg {
    Local,
    Https,
    S3,
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommand {
    /// Print configured workspace sandbox mode and wrapper argv.
    Status,
    /// Set the workspace sandbox mode and custom wrapper argv.
    Set(SandboxSetArgs),
}

#[derive(Debug, Args, Clone)]
pub struct SandboxSetArgs {
    /// Sandbox mode used for the supervised agent harness.
    #[arg(long, value_enum)]
    mode: SandboxModeArg,
    /// Wrapper argv entry for mode=custom. Repeat to build the full argv.
    #[arg(long = "wrapper-arg")]
    wrapper_args: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SandboxModeArg {
    Off,
    Unshare,
    Bwrap,
    Custom,
}

pub(super) fn run_workspace_command(
    command: WorkspaceCommand,
    output_format: OutputFormat,
) -> Result<()> {
    match command {
        WorkspaceCommand::Status => run_workspace_status(output_format),
        WorkspaceCommand::CodeSource { command } => run_code_source(command, output_format),
        WorkspaceCommand::DataSource { command } => run_data_source(command, output_format),
        WorkspaceCommand::Sync => run_workspace_sync(output_format),
        WorkspaceCommand::Sandbox { command } => run_sandbox(command, output_format),
    }
}

fn run_workspace_status(output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    if output.is_json() {
        print_json(&json!({
            "root": config.workspace.root,
            "uploads": config.workspace.uploads,
            "default_shell": config.workspace.default_shell,
            "runtime_user": config.workspace.runtime_user,
            "max_file_bytes": config.workspace.max_file_bytes,
            "sandbox": sandbox_config_json(&config),
            "code_source_count": config.workspace.code_sources.len(),
            "data_source_count": config.workspace.data_sources.len(),
        }))?;
        return Ok(());
    }

    println!("workspace status");
    println!("root: {}", config.workspace.root);
    println!("uploads: {}", config.workspace.uploads);
    println!("default_shell: {}", config.workspace.default_shell);
    println!("runtime_user: {}", config.workspace.runtime_user);
    println!("max_file_bytes: {}", config.workspace.max_file_bytes);
    println!(
        "sandbox: {}",
        sandbox_mode_label(config.workspace.sandbox.mode)
    );
    println!("code_sources: {}", config.workspace.code_sources.len());
    println!("data_sources: {}", config.workspace.data_sources.len());
    Ok(())
}

fn run_code_source(command: CodeSourceCommand, output: OutputFormat) -> Result<()> {
    match command {
        CodeSourceCommand::List => {
            let config = Config::load_from_default_path()?;
            print_code_sources(&config, output)
        }
        CodeSourceCommand::Add(args) => {
            let mut config = Config::load_from_default_path()?;
            let source = code_source_from_args(&args);
            let name = add_code_source_to_config(&mut config, source)?;
            write_default_config(&config)?;
            run_optional_sync_after_add("code", &name, !args.no_sync, &config, output)
        }
    }
}

fn run_data_source(command: DataSourceCommand, output: OutputFormat) -> Result<()> {
    match command {
        DataSourceCommand::List => {
            let config = Config::load_from_default_path()?;
            print_data_sources(&config, output)
        }
        DataSourceCommand::Add(args) => {
            let mut config = Config::load_from_default_path()?;
            let source = data_source_from_args(&args);
            let name = add_data_source_to_config(&mut config, source)?;
            write_default_config(&config)?;
            run_optional_sync_after_add("data", &name, !args.no_sync, &config, output)
        }
    }
}

fn run_workspace_sync(output: OutputFormat) -> Result<()> {
    let config = Config::load_from_default_path()?;
    let report = sync_workspace(&config)?;
    print_sync_report(&report, output)
}

fn run_sandbox(command: SandboxCommand, output: OutputFormat) -> Result<()> {
    match command {
        SandboxCommand::Status => {
            let config = Config::load_from_default_path()?;
            print_sandbox_status(&config, output)
        }
        SandboxCommand::Set(args) => {
            let mut config = Config::load_from_default_path()?;
            apply_sandbox_set(&mut config, &args)?;
            write_default_config(&config)?;
            print_sandbox_set_result(&config, output)
        }
    }
}

fn run_optional_sync_after_add(
    lane: &'static str,
    name: &str,
    should_sync: bool,
    config: &Config,
    output: OutputFormat,
) -> Result<()> {
    if !should_sync {
        if output.is_json() {
            print_json(&json!({
                "added": true,
                "lane": lane,
                "name": name,
                "sync": "skipped",
            }))?;
        } else {
            println!("workspace config updated");
            println!("{lane}_source: {name}");
            println!("sync skipped; run `acps workspace sync`");
        }
        return Ok(());
    }

    if !output.is_json() {
        println!("workspace config updated");
        println!("{lane}_source: {name}");
    }
    match sync_workspace(config) {
        Ok(report) => {
            if output.is_json() {
                print_json(&json!({
                    "added": true,
                    "lane": lane,
                    "name": name,
                    "sync": sync_report_json(&report),
                }))?;
            } else {
                print_sync_report(&report, output)?;
            }
            Ok(())
        }
        Err(error) => {
            if !output.is_json() {
                eprintln!("workspace sync failed after config update");
                eprintln!("retry: acps workspace sync");
            }
            Err(error)
        }
    }
}

fn sync_workspace(config: &Config) -> Result<MaterializeReport> {
    let home = home_dir()?;
    let secrets = SecretStore::open(&home)?;
    materialize_workspace(&config.workspace, &secrets, None)
}

fn add_code_source_to_config(config: &mut Config, source: CodeSourceConfig) -> Result<String> {
    config.workspace.code_sources.push(source);
    let validated = validate_candidate_config(config)?;
    *config = validated;
    let source = config
        .workspace
        .code_sources
        .last()
        .ok_or(StackError::MissingField {
            field: "workspace.code_sources",
        })?;
    derive_code_source_name(source).map_err(|reason| StackError::WorkspaceCodeSourceInvalid {
        index: config.workspace.code_sources.len().saturating_sub(1),
        reason,
    })
}

fn add_data_source_to_config(config: &mut Config, source: DataSourceConfig) -> Result<String> {
    config.workspace.data_sources.push(source);
    let validated = validate_candidate_config(config)?;
    *config = validated;
    let source = config
        .workspace
        .data_sources
        .last()
        .ok_or(StackError::MissingField {
            field: "workspace.data_sources",
        })?;
    derive_data_source_name(source).map_err(|reason| StackError::WorkspaceDataSourceInvalid {
        index: config.workspace.data_sources.len().saturating_sub(1),
        reason,
    })
}

fn apply_sandbox_set(config: &mut Config, args: &SandboxSetArgs) -> Result<()> {
    validate_sandbox_args(args)?;
    let mut sandbox_config = config.workspace.sandbox.clone();
    sandbox_config.mode = args.mode.to_config();
    sandbox_config.wrapper = if args.mode == SandboxModeArg::Custom {
        args.wrapper_args.clone()
    } else {
        Vec::new()
    };
    if sandbox_config.mode != SandboxMode::Off {
        sandbox::preflight(&sandbox_config)
            .map_err(|reason| StackError::SandboxFailed { reason })?;
    }
    config.workspace.sandbox = sandbox_config;
    let validated = validate_candidate_config(config)?;
    *config = validated;
    Ok(())
}

fn validate_sandbox_args(args: &SandboxSetArgs) -> Result<()> {
    if args.mode == SandboxModeArg::Custom && args.wrapper_args.is_empty() {
        return Err(StackError::MissingField {
            field: "--wrapper-arg",
        });
    }
    if args.mode != SandboxModeArg::Custom && !args.wrapper_args.is_empty() {
        return Err(StackError::InvalidParam {
            field: "--wrapper-arg",
            reason: "--wrapper-arg is only valid with --mode custom".to_owned(),
        });
    }
    if args.wrapper_args.iter().any(|arg| arg.trim().is_empty()) {
        return Err(StackError::InvalidParam {
            field: "--wrapper-arg",
            reason: "wrapper arguments must be non-empty".to_owned(),
        });
    }
    Ok(())
}

fn validate_candidate_config(config: &Config) -> Result<Config> {
    let canonical = config.to_canonical_toml()?;
    config::load_config_from_str(&canonical)
}

fn write_default_config(config: &Config) -> Result<()> {
    let config_path = config::default_config_path()?;
    write_config(&config_path, config)
}

fn write_config(path: &Path, config: &Config) -> Result<()> {
    let canonical = config.to_canonical_toml()?;
    config::load_config_from_str(&canonical)?;
    atomic_write_owner_only(path, canonical.as_bytes())
}

fn code_source_from_args(args: &CodeSourceAddArgs) -> CodeSourceConfig {
    CodeSourceConfig {
        source_type: "git".to_owned(),
        repo: Some(args.repo.clone()),
        branch: args.branch.clone(),
        credential_ref: args.credential_ref.clone(),
        name: args.name.clone(),
    }
}

fn data_source_from_args(args: &DataSourceAddArgs) -> DataSourceConfig {
    DataSourceConfig {
        source_type: args.source_type.as_str().to_owned(),
        name: args.name.clone(),
        path: args.path.clone(),
        url: args.url.clone(),
        expected_sha256: args.expected_sha256.clone(),
        max_download_bytes: args.max_download_bytes,
        max_extracted_bytes: args.max_extracted_bytes,
        bucket: args.bucket.clone(),
        prefix: args.prefix.clone(),
        region: args.region.clone(),
        access_key_ref: args.access_key_ref.clone(),
        secret_key_ref: args.secret_key_ref.clone(),
    }
}

fn print_code_sources(config: &Config, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        let sources = config
            .workspace
            .code_sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                json!({
                    "index": index,
                    "name": derive_code_source_name(source).unwrap_or_else(|_| "<invalid>".to_owned()),
                    "type": source.source_type,
                    "repo": source.repo,
                    "branch": source.branch,
                    "credential_ref": source.credential_ref,
                })
            })
            .collect::<Vec<_>>();
        print_json(&json!({ "code_sources": sources }))?;
        return Ok(());
    }

    if config.workspace.code_sources.is_empty() {
        println!("code sources: none");
        return Ok(());
    }
    println!("code sources:");
    for (index, source) in config.workspace.code_sources.iter().enumerate() {
        let name = derive_code_source_name(source).unwrap_or_else(|_| "<invalid>".to_owned());
        println!("{}. {name}", index + 1);
        println!("   type: {}", source.source_type);
        if let Some(repo) = source.repo.as_deref() {
            println!("   repo: {repo}");
        }
        if let Some(branch) = source.branch.as_deref() {
            println!("   branch: {branch}");
        }
        if let Some(credential_ref) = source.credential_ref.as_deref() {
            println!("   credential_ref: {credential_ref}");
        }
    }
    Ok(())
}

fn print_data_sources(config: &Config, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        let sources = config
            .workspace
            .data_sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                json!({
                    "index": index,
                    "name": derive_data_source_name(source).unwrap_or_else(|_| "<invalid>".to_owned()),
                    "type": source.source_type,
                    "path": source.path,
                    "url": source.url,
                    "bucket": source.bucket,
                    "prefix": source.prefix,
                    "region": source.region,
                    "access_key_ref": source.access_key_ref,
                    "secret_key_ref": source.secret_key_ref,
                })
            })
            .collect::<Vec<_>>();
        print_json(&json!({ "data_sources": sources }))?;
        return Ok(());
    }

    if config.workspace.data_sources.is_empty() {
        println!("data sources: none");
        return Ok(());
    }
    println!("data sources:");
    for (index, source) in config.workspace.data_sources.iter().enumerate() {
        let name = derive_data_source_name(source).unwrap_or_else(|_| "<invalid>".to_owned());
        println!("{}. {name}", index + 1);
        println!("   type: {}", source.source_type);
        if let Some(path) = source.path.as_deref() {
            println!("   path: {path}");
        }
        if let Some(url) = source.url.as_deref() {
            println!("   url: {url}");
        }
        if let Some(bucket) = source.bucket.as_deref() {
            println!("   bucket: {bucket}");
        }
        if let Some(prefix) = source.prefix.as_deref() {
            println!("   prefix: {prefix}");
        }
        if let Some(region) = source.region.as_deref() {
            println!("   region: {region}");
        }
    }
    Ok(())
}

fn print_sandbox_status(config: &Config, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&sandbox_config_json(config))?;
        return Ok(());
    }
    println!(
        "workspace sandbox: {}",
        sandbox_mode_label(config.workspace.sandbox.mode)
    );
    if !config.workspace.sandbox.wrapper.is_empty() {
        println!(
            "wrapper: {}",
            shell_words(&config.workspace.sandbox.wrapper)
        );
    }
    println!("restart required for changes: supervised-agent (`acps restart`)");
    Ok(())
}

fn print_sandbox_set_result(config: &Config, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        let mut value = sandbox_config_json(config);
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "restart_required".to_owned(),
                Value::String("supervised-agent".to_owned()),
            );
        }
        print_json(&value)?;
        return Ok(());
    }
    println!("workspace sandbox updated");
    println!(
        "mode: {}",
        sandbox_mode_label(config.workspace.sandbox.mode)
    );
    if !config.workspace.sandbox.wrapper.is_empty() {
        println!(
            "wrapper: {}",
            shell_words(&config.workspace.sandbox.wrapper)
        );
    }
    println!("restart required: supervised-agent (`acps restart`)");
    Ok(())
}

fn print_sync_report(report: &MaterializeReport, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        print_json(&sync_report_json(report))?;
        return Ok(());
    }
    println!("workspace sync: ok");
    println!("root: {}", report.root.display());
    println!("uploads: {}", report.uploads.display());
    print_source_reports("code", &report.code);
    print_source_reports("data", &report.data);
    Ok(())
}

fn print_source_reports(
    label: &str,
    reports: &[crate::runtime::workspace_sources::workspace_init::SourceReport],
) {
    if reports.is_empty() {
        println!("{label}: none");
        return;
    }
    println!("{label}:");
    for report in reports {
        println!(
            "  {}: {} ({})",
            report.name,
            outcome_label(&report.outcome),
            report.destination.display()
        );
    }
}

fn sync_report_json(report: &MaterializeReport) -> Value {
    json!({
        "root": report.root.display().to_string(),
        "uploads": report.uploads.display().to_string(),
        "code": source_reports_json(&report.code),
        "data": source_reports_json(&report.data),
    })
}

fn source_reports_json(
    reports: &[crate::runtime::workspace_sources::workspace_init::SourceReport],
) -> Vec<Value> {
    reports
        .iter()
        .map(|report| {
            json!({
                "name": report.name,
                "destination": report.destination.display().to_string(),
                "outcome": outcome_label(&report.outcome),
                "log_dir": report.log_dir.as_ref().map(|path| path.display().to_string()),
            })
        })
        .collect()
}

fn sandbox_config_json(config: &Config) -> Value {
    json!({
        "mode": sandbox_mode_label(config.workspace.sandbox.mode),
        "wrapper": config.workspace.sandbox.wrapper,
        "restart_required_for_changes": "supervised-agent",
    })
}

fn outcome_label(outcome: &MaterializeOutcome) -> &'static str {
    match outcome {
        MaterializeOutcome::Created => "created",
        MaterializeOutcome::Verified => "verified",
    }
}

fn sandbox_mode_label(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::Off => "off",
        SandboxMode::Unshare => "unshare",
        SandboxMode::Bwrap => "bwrap",
        SandboxMode::Custom => "custom",
    }
}

fn shell_words(values: &[String]) -> String {
    values
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'-' | b'.' | b'/' | b':' | b'=' | b',' | b'@' | b'+'
                )
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl DataSourceKindArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Https => "https",
            Self::S3 => "s3",
        }
    }
}

impl SandboxModeArg {
    fn to_config(self) -> SandboxMode {
        match self {
            Self::Off => SandboxMode::Off,
            Self::Unshare => SandboxMode::Unshare,
            Self::Bwrap => SandboxMode::Bwrap,
            Self::Custom => SandboxMode::Custom,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_config() -> Config {
        config::load_config_from_str(include_str!(
            "../../tests/fixtures/valid-opencode-stack.toml"
        ))
        .expect("fixture parses")
    }

    #[test]
    fn code_source_add_updates_config() {
        let mut config = fixture_config();
        let name = add_code_source_to_config(
            &mut config,
            CodeSourceConfig {
                source_type: "git".to_owned(),
                repo: Some("https://github.com/example/app.git".to_owned()),
                branch: Some("main".to_owned()),
                credential_ref: None,
                name: None,
            },
        )
        .expect("source added");

        assert_eq!(name, "app");
        assert_eq!(config.workspace.code_sources.len(), 1);
        assert_eq!(
            config.workspace.code_sources[0].repo.as_deref(),
            Some("https://github.com/example/app.git")
        );
    }

    #[test]
    fn code_source_add_rejects_duplicate_destination_name() {
        let mut config = fixture_config();
        add_code_source_to_config(
            &mut config,
            CodeSourceConfig {
                source_type: "git".to_owned(),
                repo: Some("https://github.com/example/app.git".to_owned()),
                branch: None,
                credential_ref: None,
                name: Some("app".to_owned()),
            },
        )
        .expect("first source");

        let error = add_code_source_to_config(
            &mut config,
            CodeSourceConfig {
                source_type: "git".to_owned(),
                repo: Some("https://github.com/example/other.git".to_owned()),
                branch: None,
                credential_ref: None,
                name: Some("app".to_owned()),
            },
        )
        .expect_err("duplicate source must fail");

        assert!(
            error
                .to_string()
                .contains("duplicate destination name `app`")
        );
    }

    #[test]
    fn data_source_add_updates_config() {
        let mut config = fixture_config();
        let name = add_data_source_to_config(
            &mut config,
            DataSourceConfig {
                source_type: "local".to_owned(),
                name: None,
                path: Some("/tmp/dataset".to_owned()),
                url: None,
                expected_sha256: None,
                max_download_bytes: None,
                max_extracted_bytes: None,
                bucket: None,
                prefix: None,
                region: None,
                access_key_ref: None,
                secret_key_ref: None,
            },
        )
        .expect("source added");

        assert_eq!(name, "dataset");
        assert_eq!(config.workspace.data_sources.len(), 1);
        assert_eq!(
            config.workspace.data_sources[0].path.as_deref(),
            Some("/tmp/dataset")
        );
    }

    #[test]
    fn data_source_add_validates_source_shape() {
        let mut config = fixture_config();
        let error = add_data_source_to_config(
            &mut config,
            DataSourceConfig {
                source_type: "local".to_owned(),
                name: None,
                path: Some("relative/path".to_owned()),
                url: None,
                expected_sha256: None,
                max_download_bytes: None,
                max_extracted_bytes: None,
                bucket: None,
                prefix: None,
                region: None,
                access_key_ref: None,
                secret_key_ref: None,
            },
        )
        .expect_err("relative local path must fail");

        assert!(error.to_string().contains("must be absolute"));
    }

    #[test]
    fn sandbox_set_custom_requires_wrapper() {
        let mut config = fixture_config();
        let error = apply_sandbox_set(
            &mut config,
            &SandboxSetArgs {
                mode: SandboxModeArg::Custom,
                wrapper_args: Vec::new(),
            },
        )
        .expect_err("custom wrapper must be required");

        assert!(error.to_string().contains("--wrapper-arg"));
    }

    #[test]
    fn sandbox_set_off_clears_wrapper_and_preserves_extra_paths() {
        let mut config = fixture_config();
        config.workspace.sandbox.mode = SandboxMode::Custom;
        config.workspace.sandbox.wrapper = vec!["sandboxer".to_owned()];
        config.workspace.sandbox.mask_paths = vec!["/private".to_owned()];
        config.workspace.sandbox.allow_paths = vec!["/cache".to_owned()];

        apply_sandbox_set(
            &mut config,
            &SandboxSetArgs {
                mode: SandboxModeArg::Off,
                wrapper_args: Vec::new(),
            },
        )
        .expect("off sandbox");

        assert_eq!(config.workspace.sandbox.mode, SandboxMode::Off);
        assert!(config.workspace.sandbox.wrapper.is_empty());
        assert_eq!(config.workspace.sandbox.mask_paths, vec!["/private"]);
        assert_eq!(config.workspace.sandbox.allow_paths, vec!["/cache"]);
    }

    #[test]
    fn no_sync_flag_is_carried_by_source_args() {
        let args = CodeSourceAddArgs {
            repo: "https://github.com/example/app.git".to_owned(),
            branch: None,
            credential_ref: None,
            name: None,
            no_sync: true,
        };
        let source = code_source_from_args(&args);

        assert!(args.no_sync);
        assert_eq!(source.source_type, "git");
        assert_eq!(
            source.repo.as_deref(),
            Some("https://github.com/example/app.git")
        );
    }

    #[test]
    fn shell_words_quotes_wrapper_arguments_for_display() {
        assert_eq!(
            shell_words(&[
                "systemd-run".to_owned(),
                "--property=BindPaths=/tmp/source path".to_owned(),
                "it's-ok".to_owned(),
            ]),
            "systemd-run '--property=BindPaths=/tmp/source path' 'it'\\''s-ok'"
        );
    }
}
