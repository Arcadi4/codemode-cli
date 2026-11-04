use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use crate::import::Client;

#[derive(Parser)]
#[command(
    name = "cm",
    version,
    about = "A stateless MCP host for shell-native agent workflows",
    after_help = "Agents: start with `cm skills read core`. Discover details on demand with `<command> --help`.\nSupports MCP 2026-07-28 only; no sessions, elicitation, or deprecated features.\nStdout is data; stderr is diagnostics. Tool failures exit 1; usage errors exit 2."
)]
pub struct Cli {
    /// Print the full result as JSON (binary content becomes file paths)
    #[arg(long, global = true)]
    pub json: bool,
    /// Re-fetch metadata/resources even while server-provided cache TTLs are fresh
    #[arg(long, global = true)]
    pub refresh: bool,
    /// Limit catalog discovery and name resolution to this configured server
    #[arg(long = "server", short = 's', global = true)]
    pub selected_server: Option<String>,
    /// Maximum request deadline in seconds (shorter source-client timeouts apply)
    #[arg(long, global = true, default_value = "60", value_parser = clap::value_parser!(u64).range(1..))]
    pub timeout: u64,
    /// Maximum encoded server response size in bytes (raise for large media)
    #[arg(long, global = true, default_value = "67108864", value_parser = clap::value_parser!(u64).range(1..))]
    pub max_response_bytes: u64,
    /// Opt into durable asynchronous task handles instead of requiring inline results
    #[arg(long = "tasks", global = true)]
    pub tasks_enabled: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Import another client's server definitions/credentials into the global config
    Import(ImportArgs),
    /// Discover and invoke namespaced MCP tools
    Tools {
        #[command(subcommand)]
        command: Tools,
    },
    /// Discover and retrieve MCP prompts
    Prompts {
        #[command(subcommand)]
        command: Prompts,
    },
    /// Discover and read MCP resources and URI templates
    Resources {
        #[command(subcommand)]
        command: Resources,
    },
    /// Inspect configured MCP servers without maintaining connections
    Servers {
        #[command(subcommand)]
        command: Servers,
    },
    /// Read the built-in guide for agents
    Skills {
        #[command(subcommand)]
        command: Skills,
    },
    /// Poll or cancel durable Tasks extension handles (explicitly opts into Tasks)
    Tasks {
        #[command(subcommand)]
        command: Tasks,
    },
}

#[derive(Args)]
pub struct ImportArgs {
    /// Source client (only its MCP configuration is imported)
    #[arg(value_enum, required_unless_present = "all", conflicts_with = "all")]
    pub client: Option<Client>,
    /// Import every supported client's available configuration
    #[arg(long)]
    pub all: bool,
    /// Read a specific source file instead of the client's standard locations
    #[arg(long, requires = "client")]
    pub path: Option<PathBuf>,
    /// Explicitly replace conflicting global definitions
    #[arg(long)]
    pub force: bool,
}

#[derive(Subcommand)]
pub enum Tools {
    /// List compact tool summaries as JSON; names are <server>_<tool>
    List,
    /// Print a tool's input and output JSON Schemas
    Schema { tool: String },
    /// Print a tool's full definition plus server instructions
    Discover { tool: String },
    /// Invoke a tool; arguments are a JSON object, @file, or - for stdin
    Call {
        tool: String,
        #[arg(default_value = "{}")]
        arguments: String,
    },
}

#[derive(Subcommand)]
pub enum Prompts {
    /// List compact prompt summaries as JSON
    List,
    /// Print a prompt's declared arguments (MCP prompts do not declare JSON Schemas)
    Schema { prompt: String },
    /// Print the full prompt definition plus server instructions
    Discover { prompt: String },
    /// Get a prompt; arguments must be a string-valued JSON object, @file, or -
    Get {
        prompt: String,
        #[arg(default_value = "{}")]
        arguments: String,
    },
    /// Complete a prompt argument using the server's completion capability
    Complete {
        prompt: String,
        argument: String,
        value: String,
        /// Already resolved arguments as a string-valued JSON object, @file, or -
        #[arg(long)]
        context: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Resources {
    /// List compact resource summaries as JSON
    List,
    /// List URI templates as JSON
    Templates,
    /// Print URI/MIME declarations (MCP resources do not declare JSON Schemas)
    Schema {
        server: String,
        uri: String,
        #[arg(long)]
        template: bool,
    },
    /// Print the full resource/template definition plus server instructions
    Discover {
        server: String,
        uri: String,
        #[arg(long)]
        template: bool,
    },
    /// Read an explicit URI from a server (including expanded templates)
    Read { server: String, uri: String },
    /// Complete a resource URI/template argument
    Complete {
        server: String,
        uri: String,
        argument: String,
        value: String,
        /// Already resolved arguments as a string-valued JSON object, @file, or -
        #[arg(long)]
        context: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum Servers {
    /// List server names, transports and configuration origins (no network or secrets)
    List,
    /// Print supported versions, capabilities, identity and server instructions
    Discover { server: String },
}

#[derive(Subcommand)]
pub enum Skills {
    /// List built-in skills
    List,
    /// Read a built-in skill (start with core)
    Read { skill: String },
}

#[derive(Subcommand)]
pub enum Tasks {
    /// Fetch task state/result; respect pollIntervalMs before a subsequent poll
    Get { server: String, task_id: String },
    /// Request cooperative cancellation; acknowledgement does not prove work stopped
    Cancel { server: String, task_id: String },
}
