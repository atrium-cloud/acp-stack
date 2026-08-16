use super::*;

pub fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            // `use_stderr()` is false for DisplayHelp / DisplayVersion — those are not failures.
            if error.use_stderr() {
                record_cli_error_message(&strip_ansi(&error.to_string()));
            }
            error.exit();
        }
    };
    run_cli(cli)
}

fn run_cli(cli: Cli) -> Result<()> {
    // The internal sandbox helper runs inside the sandbox namespaces, where the
    // state DB may be masked, and on success it execs and never returns. Dispatch
    // it before the output/error-recording machinery so a failure never tries to
    // open (or spuriously write) the durable `cli.error` log.
    if let Command::SandboxExec { args } = cli.command {
        return crate::runtime::sandbox::run_exec(args);
    }
    // Same reasoning for the network supervisor: its stdio belongs to the
    // workload (the ACP transport for agent spawns), and it terminates itself
    // mirroring the workload's status, so it must never reach the output or
    // error-recording machinery.
    if let Command::SandboxSupervise { args } = cli.command {
        return crate::runtime::sandbox::supervise::run_supervise(args);
    }
    if let Command::SandboxProviderSupervise { args } = cli.command {
        return crate::runtime::sandbox::supervise::run_provider_supervise(args);
    }
    let output = OutputFormatChoice::new(cli.format);
    let result = match cli.command {
        Command::Completion { shell } => {
            output.reject_json("completion")?;
            let mut command = Cli::command();
            generate(shell, &mut command, "acps", &mut std::io::stdout());
            Ok(())
        }
        Command::Init(args) => {
            output.reject_json("init")?;
            crate::cli::init::run_init_command(*args, InitMode::Operator)
        }
        #[cfg(feature = "dev-tools")]
        Command::Dev { command } => {
            output.reject_json("dev")?;
            match command {
                DevCommand::Init(args) => crate::cli::init::run_init(*args, InitMode::Dev),
                DevCommand::Serve(args) => crate::cli::serve::run_serve(args, ServeMode::Dev),
            }
        }
        Command::Status => crate::cli::status::run_status(output.effective()),
        #[cfg(feature = "stack-self-update")]
        Command::Update { command } => {
            crate::cli::update::run_update_command(command, output.effective())
        }
        Command::Reset(args) => {
            output.reject_json("reset")?;
            crate::cli::reset::run_reset(args)
        }
        Command::Serve(args) => {
            output.reject_json("serve")?;
            crate::cli::serve::run_serve(args, ServeMode::Operator)
        }
        Command::Auth { command } => {
            output.reject_json("auth")?;
            crate::cli::auth::run_auth_command(command)
        }
        Command::Secrets { command } => {
            crate::cli::secrets::run_secrets_command(command, output.effective())
        }
        Command::Config { command } => {
            crate::cli::config::run_config_command(command, output.effective())
        }
        Command::Logging { command } => {
            crate::cli::logging::run_logging_command(command, output.effective())
        }
        Command::Logs { command } => crate::cli::logs::run_logs_command(command, output),
        Command::Agent { command } => crate::cli::agent::run_agent_command(command, output),
        Command::Restart(args) => crate::cli::agent::run_agent_restart(args, output.effective()),
        Command::Workspace { command } => {
            crate::cli::workspace::run_workspace_command(command, output.effective())
        }
        Command::Extensions { command } => {
            crate::cli::extensions::run_extensions_command(command, output.effective())
        }
        Command::Array { command } => {
            crate::cli::array::run_array_command(command, output.effective())
        }
        Command::Skills { command } => {
            crate::cli::skill::run_skill_command(command, output.effective())
        }
        Command::Subagent { command } => {
            output.reject_json("subagent")?;
            crate::cli::subagent::run_subagent_command(command)
        }
        Command::Installer { command } => {
            crate::cli::installer::run_installer_command(command, output.effective())
        }
        Command::Sessions { command } => {
            crate::cli::sessions::run_sessions_command(command, output.effective())
        }
        Command::Deps { command } => {
            crate::cli::deps::run_deps_command(command, output.effective())
        }
        Command::Security { command } => {
            crate::cli::security::run_security_command(command, output)
        }
        Command::Metrics { command } => {
            crate::cli::metrics::run_metrics_command(command, output.effective())
        }
        Command::Ws { command } => crate::cli::ws::run_ws_command(command, output.effective()),
        // Dispatched in the fast path above, before this match.
        Command::SandboxExec { .. }
        | Command::SandboxSupervise { .. }
        | Command::SandboxProviderSupervise { .. } => {
            unreachable!("sandbox helper commands are dispatched before output handling")
        }
    };

    if let Err(error) = &result {
        // `acps reset` dry-run intentionally returns this error to signal the
        // operator must pass `--yes`. The dry-run contract is "exits without
        // touching the filesystem" — recording a `cli.error` row into
        // state.sqlite would violate that, so we skip the durable log for it.
        if !matches!(error, StackError::ResetNotConfirmed) {
            record_cli_error_message(&strip_ansi(&error.to_string()));
        }
    }

    result
}

fn record_cli_error_message(error_message: &str) {
    let Ok(home) = home_dir() else {
        return;
    };
    let state_path = default_state_path(&home);
    if !state_path.exists() {
        return;
    }
    // Repair the existing file's mode before opening, so the error row is not written
    // while the database is still readable by other local users.
    if set_owner_only_file(&state_path).is_err() {
        return;
    }
    let Ok(store) = StateStore::open(&state_path) else {
        return;
    };
    if store.migrate().is_err() {
        return;
    }
    let payload = serde_json::json!({ "error": error_message }).to_string();
    if let Err(log_error) = store.append_event_with_source(
        "error",
        "cli.error",
        crate::state::EVENT_SOURCE_CLI,
        "command failed",
        &payload,
    ) {
        eprintln!("failed to record CLI error: {log_error}");
    }
}
