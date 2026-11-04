mod app;
mod cache;
mod cli;
mod config;
mod credentials;
mod host;
mod import;
mod mcp;
mod output;
mod schema;
mod search;
mod tasks;
mod transport;

use clap::Parser;
use std::{
    io::{self, Write},
    process::ExitCode,
};

fn main() -> ExitCode {
    match app::run(cli::Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
                    || cause
                        .downcast_ref::<serde_json::Error>()
                        .is_some_and(|error| {
                            error.io_error_kind() == Some(io::ErrorKind::BrokenPipe)
                        })
            }) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            let code = error
                .downcast_ref::<app::SignalExit>()
                .map_or(1, |signal| signal.0);
            let _ = writeln!(io::stderr().lock(), "cm: {error:#}");
            ExitCode::from(code)
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/config.rs"]
mod config_tests;
#[cfg(test)]
#[path = "../tests/unit/credentials.rs"]
mod credential_tests;
#[cfg(test)]
#[path = "../tests/unit/output.rs"]
mod output_tests;
