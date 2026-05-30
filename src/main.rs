fn main() {
    acp_stack::tracing_init::init();

    if let Err(error) = acp_stack::cli::run() {
        eprintln!("{error}");
        if let Some(hint) = error.remediation_hint() {
            eprintln!("hint: {hint}");
        }
        std::process::exit(1);
    }
}
