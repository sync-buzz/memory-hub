mod cli;
mod config;
mod doctor;
mod exit;
mod model;
mod registry;
mod registry_cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run(std::env::args_os()).into()
}
