mod cli;
mod config;
mod doctor;
mod exit;
mod model;
mod registry;
mod registry_cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    install_tracing();
    cli::run(std::env::args_os()).into()
}

/// Send `tracing` somewhere, on stderr and only when asked.
///
/// Without a subscriber every `debug!` in the engine is compiled in and thrown
/// away, which is the state this was in: the search channel could describe
/// exactly why it returned what it did, and nothing was listening.
///
/// **Stderr, never stdout.** The MCP session owns stdout, one JSON message per
/// line; a log line written there is a protocol error at the other end.
///
/// Silent by default. `RUST_LOG=memory_hub_index=debug` turns on one channel,
/// `RUST_LOG=debug` all of them, and an unset variable leaves the engine as
/// quiet as it has always been.
fn install_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("off"));
    // A second initialisation is not a failure worth reporting: nothing above
    // this line has logged anything yet, and the process that set one up
    // already meant to.
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
