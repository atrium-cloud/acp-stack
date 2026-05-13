fn main() {
    acp_stack::tracing_init::init();

    if let Err(error) = acp_stack::cli::run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
