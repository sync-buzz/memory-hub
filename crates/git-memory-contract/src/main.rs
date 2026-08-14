use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use git_memory_contract::{FakeServerTarget, ReleaseBinaryTarget, ServerTarget, run_contract};

#[derive(Debug, Parser)]
#[command(about = "Run the Git Memory black-box behavioral contract")]
struct Cli {
    /// A shipped git-memory executable (invoked as `git-memory mcp`).
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "fake_binary",
        required_unless_present = "fake_binary"
    )]
    release_binary: Option<PathBuf>,

    /// The deterministic fake server executable included with this harness.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "release_binary",
        required_unless_present = "release_binary"
    )]
    fake_binary: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = Output::Human)]
    output: Output,
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum Output {
    #[default]
    Human,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let target: Box<dyn ServerTarget> = if let Some(binary) = cli.release_binary {
        Box::new(ReleaseBinaryTarget::new(binary))
    } else if let Some(binary) = cli.fake_binary {
        Box::new(FakeServerTarget::new(binary))
    } else {
        unreachable!("clap requires exactly one target")
    };
    let report = run_contract(target.as_ref());
    let render = match cli.output {
        Output::Human => render_human(&report),
        Output::Json => render_json(&report),
    };
    if let Err(error) = render {
        eprintln!("git-memory-contract: unable to render report: {error}");
        return ExitCode::from(70);
    }
    if report.passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn render_human(report: &git_memory_contract::ContractReport) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(output, "Git Memory contract target: {}", report.target)?;
    for scenario in &report.scenarios {
        if scenario.passed {
            writeln!(output, "[ok] {}", scenario.name)?;
        } else {
            writeln!(
                output,
                "[error] {}: {}",
                scenario.name,
                scenario.failure.as_deref().unwrap_or("unknown failure")
            )?;
        }
    }
    writeln!(
        output,
        "Result: {}",
        if report.passed { "passed" } else { "failed" }
    )
}

fn render_json(report: &git_memory_contract::ContractReport) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, report).map_err(io::Error::other)?;
    writeln!(output)
}
