mod config;
mod import;

use std::{io::{self, Write}, path::PathBuf, process::ExitCode};
use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;

#[derive(Parser)]
#[command(name = "cm", version, about = "Read and import MCP client configurations")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import a client's MCP definitions into the per-user global configuration
    Import {
        #[arg(value_enum, required_unless_present = "all", conflicts_with = "all")]
        client: Option<import::Client>,
        #[arg(long)]
        all: bool,
        #[arg(long, requires = "client")]
        path: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    /// Inspect configured servers without starting them or exposing credentials
    Servers {
        #[command(subcommand)]
        command: Servers,
    },
}

#[derive(Subcommand)]
enum Servers { List }

fn main() -> ExitCode {
    let result = run(Cli::parse());
    finish(result)
}

fn finish(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.chain().any(|cause| cause.downcast_ref::<io::Error>().is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "cm: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let result = match cli.command {
        Command::Import { client, all: _, path, force } => {
            let clients = client.map_or_else(|| import::Client::value_variants().to_vec(), |client| vec![client]);
            let report = config::Config::import(&clients, &import::ImportOptions { force, path })?;
            for warning in &report.warnings { eprintln!("cm: {warning}"); }
            let skipped: Vec<_> = report.skipped.iter().map(|skip| json!({"name":skip.name,"reason":skip.reason})).collect();
            json!({"config":report.config_path,"sources":report.sources,"added":report.added,"updated":report.updated,"unchanged":report.unchanged,"skipped":skipped})
        }
        Command::Servers { command: Servers::List } => {
            let config = config::Config::load()?;
            for warning in &config.warnings { eprintln!("cm: {warning}"); }
            serde_json::to_value(config.summaries())?
        }
    };
    let mut stdout = io::stdout().lock();
    serde_json::to_writer_pretty(&mut stdout, &result)?;
    writeln!(stdout)?;
    Ok(())
}
