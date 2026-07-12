use std::io::{self, IsTerminal};
use std::path::Path;

use serde_json::Value;

use super::{AgentConfigArgs, AgentConfigCommand, AgentConfigImportArgs, AgentConfigInspectArgs};
use crate::cli::core::{
    CliMethod, OutputFormat, daemon_base_url, daemon_request, print_json, resolve_admin_key,
};
use crate::config::{Config, IMPORT_SIZE_LIMIT};
use crate::error::{Result, StackError};

pub(super) fn run_agent_config(args: AgentConfigArgs, output: OutputFormat) -> Result<()> {
    match args.command {
        AgentConfigCommand::Inspect(args) => run_inspect(args, output),
        AgentConfigCommand::Import(args) => run_import(args, output),
    }
}

fn run_inspect(args: AgentConfigInspectArgs, output: OutputFormat) -> Result<()> {
    let key = resolve_admin_key(args.admin_key, io::stdin().is_terminal())?;
    let (filename, content) = read_native_config(&args.path)?;
    let response = request_inspection(&key, &filename, &content)?;
    let inspection = response.get("data").unwrap_or(&response);
    print_inspection(inspection, output)
}

fn run_import(args: AgentConfigImportArgs, output: OutputFormat) -> Result<()> {
    let key = resolve_admin_key(args.admin_key, io::stdin().is_terminal())?;
    let (filename, content) = read_native_config(&args.path)?;
    let inspection_response = request_inspection(&key, &filename, &content)?;
    let inspection = inspection_response
        .get("data")
        .unwrap_or(&inspection_response);
    let revision = inspection
        .get("revision")
        .and_then(Value::as_str)
        .ok_or_else(|| StackError::AgentInitializeFailed {
            reason: "native config inspection response omitted revision".to_owned(),
        })?;
    let config = Config::load_from_default_path()?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let request = serde_json::json!({
        "revision": revision,
        "selected_managed_field_ids": args.managed_fields,
        "executable_settings_acknowledged": args.acknowledge_executable_settings,
    });
    let runtime = cli_runtime()?;
    let response = runtime.block_on(daemon_request(
        &base_url,
        CliMethod::Post,
        "/v1/agent/config/native/import",
        &key,
        Some(&request),
    ))?;
    let operation = response.get("data").unwrap_or(&response);
    let failed_code = failed_operation_code(operation);
    if output.is_json() {
        print_json(operation)?;
    } else {
        println!(
            "operation: {}",
            operation
                .get("operation_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        println!(
            "status: {}",
            operation
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        if let Some(code) = operation.pointer("/error/code").and_then(Value::as_str) {
            println!("error: {code}");
        }
        if operation
            .pointer("/restart/queued")
            .and_then(Value::as_bool)
            == Some(true)
        {
            println!("restart: queued until restart blockers clear");
        } else if operation
            .pointer("/restart/restarted")
            .and_then(Value::as_bool)
            == Some(true)
        {
            println!("restart: completed");
        }
    }
    if let Some(code) = failed_code {
        return Err(StackError::NativeAgentConfigOperationFailed { code });
    }
    Ok(())
}

fn failed_operation_code(operation: &Value) -> Option<String> {
    (operation.get("status").and_then(Value::as_str) == Some("failed")).then(|| {
        operation
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("native_config_import_failed")
            .to_owned()
    })
}

fn request_inspection(key: &str, filename: &str, content: &str) -> Result<Value> {
    let config = Config::load_from_default_path()?;
    let base_url = daemon_base_url(config.api.public_url.as_deref(), &config.api.bind)?;
    let request = serde_json::json!({
        "filename": filename,
        "content": content,
    });
    cli_runtime()?.block_on(daemon_request(
        &base_url,
        CliMethod::Post,
        "/v1/agent/config/native/inspect",
        key,
        Some(&request),
    ))
}

fn print_inspection(inspection: &Value, output: OutputFormat) -> Result<()> {
    if output.is_json() {
        return print_json(inspection);
    }
    println!(
        "harness: {}",
        inspection
            .get("harness")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    println!(
        "format: {}",
        inspection
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    println!(
        "revision: {}",
        inspection
            .get("revision")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    print_managed_entries(inspection.get("managed_fields"));
    print_path_entries("blocked", inspection.get("blocked_fields"));
    print_string_entries("unmanaged", inspection.get("unmanaged_field_paths"));
    print_string_entries("executable", inspection.get("executable_categories"));
    print_string_entries("warning", inspection.get("warnings"));
    Ok(())
}

fn print_managed_entries(value: Option<&Value>) {
    let Some(values) = value.and_then(Value::as_array) else {
        return;
    };
    for value in values {
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let id = value.get("id").and_then(Value::as_str).unwrap_or("unknown");
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let compatible = value
            .get("compatible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!("managed: {path} (id={id}, kind={kind}, compatible={compatible})");
    }
}

fn print_path_entries(label: &str, value: Option<&Value>) {
    let Some(values) = value.and_then(Value::as_array) else {
        return;
    };
    for value in values {
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let detail = value
            .get("id")
            .or_else(|| value.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        println!("{label}: {path} ({detail})");
    }
}

fn print_string_entries(label: &str, value: Option<&Value>) {
    let Some(values) = value.and_then(Value::as_array) else {
        return;
    };
    for value in values.iter().filter_map(Value::as_str) {
        println!("{label}: {value}");
    }
}

fn read_native_config(path: &Path) -> Result<(String, String)> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(StackError::InvalidParam {
            field: "path",
            reason: "native config source must be a regular file".to_owned(),
        });
    }
    if metadata.len() > IMPORT_SIZE_LIMIT as u64 {
        return Err(StackError::NativeAgentConfig {
            code: "native_config_too_large",
        });
    }
    let filename = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| StackError::InvalidParam {
            field: "path",
            reason: "native config filename must be valid UTF-8".to_owned(),
        })?
        .to_owned();
    let content = std::fs::read_to_string(path).map_err(|source| StackError::ConfigRead {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((filename, content))
}

fn cli_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| StackError::ServeIo { source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_operation_preserves_typed_error_code() {
        let operation = serde_json::json!({
            "status": "failed",
            "error": { "code": "native_config_rollback_failed" }
        });
        let code = failed_operation_code(&operation).expect("failed code");
        let error = StackError::NativeAgentConfigOperationFailed { code };
        assert_eq!(error.error_code(), "native_config_rollback_failed");
    }
}
