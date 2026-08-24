use super::*;

macro_rules! init_println {
    ($output:expr, $($arg:tt)*) => {
        if $output.is_text() {
            println!($($arg)*);
        } else if $output.is_hosted() {
            prompt::emit_progress(format!($($arg)*));
        }
    };
}

mod context;
mod finalize;
mod phase_headless;
mod phase_install;
mod phase_provider;
mod phase_secrets;
mod phase_workspace;
mod preflight;
mod setup;
mod stage;
mod steps;

use context::*;
use finalize::*;
use phase_headless::*;
use phase_install::*;
use phase_provider::*;
use phase_secrets::*;
use phase_workspace::*;
use preflight::*;
pub(super) use preflight::{
    agent_settlement_signals, mcp_applicability_from_probe, mcp_settlement_from_probe,
    prompts_enabled,
};
use setup::*;
use stage::*;
use steps::*;

pub(in crate::cli) fn run_init(args: InitArgs, mode: InitMode) -> Result<()> {
    let output_mode = if args.handoff_json {
        InitOutputMode::HandoffJson
    } else {
        InitOutputMode::Text
    };
    run_init_with_output(args, mode, output_mode)
}

pub(super) fn run_hosted_init(args: InitArgs, mode: InitMode) -> Result<()> {
    run_init_with_output(args, mode, InitOutputMode::Hosted)
}

pub(in crate::cli) fn run_init_command(command: InitCommand, mode: InitMode) -> Result<()> {
    match command.command {
        Some(InitSubcommand::Serve(args)) => serve::run_init_serve(args),
        None => run_init(command.args, mode),
    }
}

/// The init flow, in order. Call order below `InitFlow::begin` is the
/// authority on step sequence, never the durable ordinals, which are only
/// matching keys for `--resume`.
fn run_init_with_output(
    mut args: InitArgs,
    mode: InitMode,
    output_mode: InitOutputMode,
) -> Result<()> {
    // Hosted init always rotates, since plaintext keys travel only in the
    // result frame. Folded in BEFORE the run record is written so a later
    // `--resume` replays the rotation instead of preserving invalidated keys.
    args.rotate_keys = args.rotate_keys || matches!(output_mode, InitOutputMode::Hosted);

    let base = prepare_init_base(&mut args, mode, output_mode)?;
    let setup = stage_init_config(args, base, output_mode)?;

    let mut flow = InitFlow::begin(setup)?;
    run_secrets_phase(&mut flow)?;
    run_agent_install_step(&mut flow)?;
    run_native_config_import_step(&mut flow)?;
    run_agent_skills_install_step(&mut flow)?;
    run_workspace_materialize_step(&mut flow)?;
    run_deps_apply_step(&mut flow)?;
    run_capability_probe_step(&mut flow)?;
    run_mcp_configure_step(&mut flow)?;
    run_provider_configure_step(&mut flow)?;
    configure_stack_update(&mut flow)?;
    configure_agent_update(&mut flow)?;
    run_agent_headless_config_step(&mut flow)?;
    run_edge_artifacts_step(&mut flow)?;
    run_init_complete_step(&mut flow)?;
    print_init_summary(&flow);
    run_testflight_step(&mut flow)?;
    finalize_init_run(&mut flow)
}

#[cfg(test)]
mod tests;
