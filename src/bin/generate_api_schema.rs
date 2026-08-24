//! Dev tool: regenerate the published `/v1` JSON Schema contract from the wire DTOs. Not shipped in release artifacts.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acp_stack::schema_export::{self, META_PATH, SCHEMA_PATH};

enum Mode {
    Write,
    Check,
    Coverage,
}

fn main() -> ExitCode {
    let mode = match parse_args() {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("Error: {message}");
            return ExitCode::FAILURE;
        }
    };
    let result = match mode {
        Mode::Write => run(false),
        Mode::Check => run(true),
        Mode::Coverage => report_coverage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the single optional mode flag, rejecting anything unrecognized.
fn parse_args() -> Result<Mode, String> {
    let mut mode = Mode::Write;
    for argument in std::env::args().skip(1) {
        mode = match argument.as_str() {
            "--check" => Mode::Check,
            "--coverage" => Mode::Coverage,
            other => return Err(format!("unexpected argument: {other}")),
        };
    }
    Ok(mode)
}

/// Print request/response coverage and fail if any handler wire type is absent from the schema.
fn report_coverage() -> Result<(), Box<dyn std::error::Error>> {
    let report = schema_export::coverage_report();
    for namespace in [&report.request, &report.response] {
        println!(
            "{:>8}: {}/{} covered ({:.0}%)",
            namespace.namespace,
            namespace.covered(),
            namespace.used.len(),
            namespace.ratio() * 100.0,
        );
        for missing in &namespace.uncovered {
            println!("          UNCOVERED {missing}");
        }
    }
    if report.is_complete() {
        Ok(())
    } else {
        Err("schema does not cover every handler wire type".into())
    }
}

fn run(check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let outputs = [
        (
            SCHEMA_PATH,
            schema_export::render(&schema_export::acps_schema()),
        ),
        (
            META_PATH,
            schema_export::render(&schema_export::acps_schema_meta()),
        ),
    ];

    if check {
        let mut stale = Vec::new();
        for (relative, generated) in &outputs {
            let path = manifest_path(relative);
            let current = std::fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            if &current != generated {
                stale.push(*relative);
            }
        }
        if stale.is_empty() {
            println!("schema up to date");
            return Ok(());
        }
        return Err(format!(
            "stale schema files: {}\nregenerate with: cargo run --features dev-tools --bin generate-api-schema",
            stale.join(", ")
        )
        .into());
    }

    for (relative, generated) in &outputs {
        let path = manifest_path(relative);
        std::fs::write(&path, generated)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn manifest_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}
