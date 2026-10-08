fn main() {
    if let Err(error) = acp_stack::runtime::sandbox::clear_ambient_capabilities_at_startup() {
        eprintln!("{error}");
        std::process::exit(1);
    }
    acp_stack::tracing_init::init();

    if let Err(error) = acp_stack::cli::run() {
        eprintln!("{error}");
        if let Some(hint) = error.remediation_hint() {
            eprintln!("hint: {hint}");
        }
        std::process::exit(1);
    }
}
