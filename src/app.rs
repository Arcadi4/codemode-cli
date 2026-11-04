use std::{
    io::{self, Read, Write},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    cli::{Cli, Command, Prompts, Resources, Servers, Skills, Tools},
    config::Config,
    host::{CatalogKind, Host},
    import, output,
};

const CORE_SKILL: &str = include_str!("../skills/core/SKILL.md");

#[derive(Debug)]
pub struct SignalExit(pub u8);
impl std::fmt::Display for SignalExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("interrupted")
    }
}
impl std::error::Error for SignalExit {}

pub fn run(cli: Cli) -> Result<()> {
    let Cli {
        command,
        json,
        refresh,
        selected_server,
        timeout,
        max_response_bytes,
    } = cli;
    match command {
        Command::Import(args) => {
            let clients = if args.all {
                import::Client::value_variants().to_vec()
            } else {
                vec![args.client.context("a source client is required")?]
            };
            let report = Config::import(
                &clients,
                &import::ImportOptions {
                    force: args.force,
                    path: args.path,
                },
            )?;
            for warning in &report.warnings {
                eprintln!("cm: {warning}");
            }
            let skipped: Vec<_> = report
                .skipped
                .iter()
                .map(|skip| json!({"name":skip.name,"reason":skip.reason}))
                .collect();
            write_json(
                &json!({"config":report.config_path,"sources":report.sources,"added":report.added,"updated":report.updated,"unchanged":report.unchanged,"skipped":skipped}),
                &mut io::stdout().lock(),
            )
        }
        Command::Skills { command } => {
            let mut stdout = io::stdout().lock();
            match command {
                Skills::List => write_json(
                    &json!([{"name":"core","description":"Start here: discover MCP capabilities and compose calls with shell tools"}]),
                    &mut stdout,
                ),
                Skills::Read { skill } => {
                    ensure!(
                        skill == "core",
                        "unknown skill {skill:?}; use `cm skills list`"
                    );
                    if json {
                        write_json(&json!({"name":"core","content":CORE_SKILL}), &mut stdout)
                    } else {
                        stdout.write_all(CORE_SKILL.as_bytes())?;
                        Ok(())
                    }
                }
            }
        }
        Command::Servers {
            command: Servers::List,
        } => {
            let config = load_config()?;
            if let Some(server) = &selected_server {
                ensure!(config.get(server).is_some(), "unknown server {server:?}");
            }
            let summaries: Vec<_> = config
                .summaries()
                .into_iter()
                .filter(|summary| {
                    selected_server
                        .as_ref()
                        .is_none_or(|server| server == &summary.name)
                })
                .collect();
            write_json(&summaries, &mut io::stdout().lock())
        }
        command => {
            // Consume stdin before starting servers or installing async signal handlers.
            let payload = prepare_payload(&command)?;
            let config = load_config()?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async move {
                let max_bytes = usize::try_from(max_response_bytes).context("response limit is too large for this platform")?;
                let mut host = Host::new(config, Duration::from_secs(timeout), refresh, max_bytes);
                let mut stdout = io::stdout().lock();
                let result = tokio::select! {
                    result = execute(&mut host, command, payload, selected_server.as_deref(), json, &mut stdout) => result,
                    signal = termination() => Err(SignalExit(signal?).into()),
                };
                host.close().await;
                result?;
                stdout.flush()?;
                Ok(())
            })
        }
    }
}

fn load_config() -> Result<Config> {
    let config = Config::load()?;
    for warning in &config.warnings {
        eprintln!("cm: {warning}");
    }
    Ok(config)
}

fn prepare_payload(command: &Command) -> Result<Option<Value>> {
    match command {
        Command::Tools {
            command: Tools::Call { arguments, .. },
        } => Ok(Some(arguments_json(arguments, false)?)),
        Command::Prompts {
            command: Prompts::Get { arguments, .. },
        } => Ok(Some(arguments_json(arguments, true)?)),
        Command::Prompts {
            command: Prompts::Complete { context, .. },
        }
        | Command::Resources {
            command: Resources::Complete { context, .. },
        } => context
            .as_deref()
            .map(|raw| arguments_json(raw, true))
            .transpose(),
        _ => Ok(None),
    }
}

fn arguments_json(raw: &str, strings: bool) -> Result<Value> {
    let value: Value = if raw == "-" {
        let mut bytes = Vec::new();
        io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .context("cannot read JSON from stdin")?;
        serde_json::from_slice(&bytes).context("stdin must contain one JSON object")?
    } else if let Some(path) = raw.strip_prefix('@') {
        serde_json::from_slice(
            &std::fs::read(path).with_context(|| format!("cannot read argument file {path:?}"))?,
        )
        .context("argument file must contain one JSON object")?
    } else {
        serde_json::from_str(raw)
            .context("arguments must be a JSON object, @file, or - for stdin")?
    };
    let object = value
        .as_object()
        .context("arguments must be a JSON object")?;
    ensure!(
        !strings || object.values().all(Value::is_string),
        "prompt/completion arguments must have string values"
    );
    Ok(value)
}

async fn execute(
    host: &mut Host,
    command: Command,
    payload: Option<Value>,
    selected: Option<&str>,
    json_output: bool,
    stdout: &mut impl Write,
) -> Result<()> {
    match command {
        Command::Tools { command } => match command {
            Tools::List => list(host, CatalogKind::Tools, selected, stdout).await,
            Tools::Schema { tool } => {
                let entry = host.locate(CatalogKind::Tools, &tool, selected).await?;
                write_json(&entry.schema(CatalogKind::Tools), stdout)
            }
            Tools::Discover { tool } => {
                discover(host, CatalogKind::Tools, &tool, selected, stdout).await
            }
            Tools::Call { tool, .. } => {
                let entry = host.locate(CatalogKind::Tools, &tool, selected).await?;
                let result = host
                    .call_tool(entry, payload.context("missing tool arguments")?)
                    .await?;
                let failed = output::is_tool_error(&result);
                output::render(result, output::OutputKind::Tool, json_output, stdout)?;
                if failed {
                    bail!("tool {tool:?} reported an error");
                }
                Ok(())
            }
        },
        Command::Prompts { command } => match command {
            Prompts::List => list(host, CatalogKind::Prompts, selected, stdout).await,
            Prompts::Schema { prompt } => {
                let entry = host.locate(CatalogKind::Prompts, &prompt, selected).await?;
                write_json(&entry.schema(CatalogKind::Prompts), stdout)
            }
            Prompts::Discover { prompt } => {
                discover(host, CatalogKind::Prompts, &prompt, selected, stdout).await
            }
            Prompts::Get { prompt, .. } => {
                let entry = host.locate(CatalogKind::Prompts, &prompt, selected).await?;
                let result = host
                    .get_prompt(entry, payload.context("missing prompt arguments")?)
                    .await?;
                output::render(result, output::OutputKind::Prompt, json_output, stdout)
            }
            Prompts::Complete {
                prompt,
                argument,
                value,
                ..
            } => {
                let entry = host.locate(CatalogKind::Prompts, &prompt, selected).await?;
                let reference = json!({"type":"ref/prompt","name":entry.native_name()});
                let result = host
                    .complete(&entry.server, reference, argument, value, payload)
                    .await?;
                write_json(
                    if json_output {
                        &result
                    } else {
                        &result["completion"]
                    },
                    stdout,
                )
            }
        },
        Command::Resources { command } => match command {
            Resources::List => list(host, CatalogKind::Resources, selected, stdout).await,
            Resources::Templates => list(host, CatalogKind::Templates, selected, stdout).await,
            Resources::Schema {
                server,
                uri,
                template,
            } => {
                let kind = if template {
                    CatalogKind::Templates
                } else {
                    CatalogKind::Resources
                };
                let entry = host.locate_resource(&server, &uri, template).await?;
                write_json(&entry.schema(kind), stdout)
            }
            Resources::Discover {
                server,
                uri,
                template,
            } => {
                let entry = host.locate_resource(&server, &uri, template).await?;
                write_json(&host.full_specification(entry).await?, stdout)
            }
            Resources::Read { server, uri } => {
                let result = host.read_resource(&server, uri).await?;
                output::render(result, output::OutputKind::Resources, json_output, stdout)
            }
            Resources::Complete {
                server,
                uri,
                argument,
                value,
                ..
            } => {
                let result = host
                    .complete(
                        &server,
                        json!({"type":"ref/resource","uri":uri}),
                        argument,
                        value,
                        payload,
                    )
                    .await?;
                write_json(
                    if json_output {
                        &result
                    } else {
                        &result["completion"]
                    },
                    stdout,
                )
            }
        },
        Command::Servers {
            command: Servers::Discover { server },
        } => write_json(host.discover(&server).await?, stdout),
        _ => unreachable!("local commands execute before the runtime starts"),
    }
}

async fn list(
    host: &mut Host,
    kind: CatalogKind,
    selected: Option<&str>,
    stdout: &mut impl Write,
) -> Result<()> {
    let entries = host.catalog(kind, selected).await?;
    let summaries: Vec<_> = entries.iter().map(|entry| entry.summary()).collect();
    write_json(&summaries, stdout)
}

async fn discover(
    host: &mut Host,
    kind: CatalogKind,
    name: &str,
    selected: Option<&str>,
    stdout: &mut impl Write,
) -> Result<()> {
    let entry = host.locate(kind, name, selected).await?;
    let specification = host.full_specification(entry).await?;
    write_json(&specification, stdout)
}

fn write_json(value: &impl Serialize, stdout: &mut impl Write) -> Result<()> {
    serde_json::to_writer_pretty(&mut *stdout, value)?;
    writeln!(stdout)?;
    Ok(())
}

#[cfg(unix)]
async fn termination() -> Result<u8> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => { result?; Ok(130) },
        _ = terminate.recv() => Ok(143),
    }
}

#[cfg(not(unix))]
async fn termination() -> Result<u8> {
    tokio::signal::ctrl_c().await?;
    Ok(130)
}
