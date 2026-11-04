use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{self, LoadOptions, ServerConfig};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SECRET_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Client {
    Vscode,
    Copilot,
    Cursor,
    Opencode,
    Claude,
    Codex,
    Devin,
}

const ALL_CLIENTS: [Client; 7] = [
    Client::Vscode,
    Client::Copilot,
    Client::Cursor,
    Client::Opencode,
    Client::Claude,
    Client::Codex,
    Client::Devin,
];

impl Client {
    pub fn all() -> &'static [Client] {
        &ALL_CLIENTS
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Client::Vscode => "vscode",
            Client::Copilot => "copilot",
            Client::Cursor => "cursor",
            Client::Opencode => "opencode",
            Client::Claude => "claude",
            Client::Codex => "codex",
            Client::Devin => "devin",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Client::Vscode => "VS Code",
            Client::Copilot => "GitHub Copilot",
            Client::Cursor => "Cursor",
            Client::Opencode => "OpenCode",
            Client::Claude => "Claude",
            Client::Codex => "Codex",
            Client::Devin => "Devin",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "vscode" | "vs-code" | "code" => Some(Client::Vscode),
            "copilot" | "github-copilot" => Some(Client::Copilot),
            "cursor" => Some(Client::Cursor),
            "opencode" | "open-code" => Some(Client::Opencode),
            "claude" | "claude-code" | "claude-desktop" => Some(Client::Claude),
            "codex" => Some(Client::Codex),
            "devin" => Some(Client::Devin),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    pub force: bool,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct ImportReport {
    pub config_path: PathBuf,
    pub sources: Vec<PathBuf>,
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub skipped: Vec<ImportSkip>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ImportSkip {
    pub name: String,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub name: String,
    pub config: ServerConfig,
    pub client: Client,
    pub paths: Vec<PathBuf>,
}

pub(crate) struct Discovery {
    pub candidates: Vec<Candidate>,
    pub sources: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub expected: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Parser {
    VsCode,
    McpServers,
    ClaudeUser,
    ClaudeProject,
    OpenCode,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Syntax {
    Vscode,
    Portable,
    Claude,
    Opencode,
    Devin,
    Literal,
}

#[derive(Clone, Debug)]
struct SourceSpec {
    client: Client,
    path: PathBuf,
    base: PathBuf,
    workspace: PathBuf,
    priority: u8,
    merge: bool,
    parser: Parser,
    syntax: Syntax,
}

#[derive(Clone, Debug, Default)]
struct ServerDraft {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    cwd: Option<String>,
    url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    bearer_token_env_var: Option<String>,
    env_file: Option<String>,
    timeout_ms: Option<u64>,
    disabled: Option<bool>,
    kind: Option<DraftKind>,
    warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DraftKind {
    Stdio,
    Http,
}

impl ServerDraft {
    fn warn(&mut self, message: impl Into<String>) {
        self.warnings.push(message.into());
    }

    fn same_definition(&self, other: &Self, include_disabled: bool) -> bool {
        self.command == other.command
            && self.args == other.args
            && self.env == other.env
            && self.cwd == other.cwd
            && self.url == other.url
            && self.headers == other.headers
            && self.bearer_token_env_var == other.bearer_token_env_var
            && self.env_file == other.env_file
            && self.timeout_ms == other.timeout_ms
            && self.kind == other.kind
            && (!include_disabled || self.disabled == other.disabled)
    }

    fn merge_from(&mut self, higher: ServerDraft) {
        let higher_remote = higher.url.is_some() && higher.command.is_none();
        let higher_stdio = higher.command.is_some() && higher.url.is_none();
        if higher_remote {
            self.command = None;
            self.args = None;
            self.cwd = None;
            self.env_file = None;
        }
        if higher_stdio {
            self.url = None;
            self.headers = None;
            self.bearer_token_env_var = None;
        }
        if higher.command.is_some() {
            self.command = higher.command;
        }
        if higher.args.is_some() {
            self.args = higher.args;
        }
        if higher.cwd.is_some() {
            self.cwd = higher.cwd;
        }
        if higher.url.is_some() {
            self.url = higher.url;
        }
        if higher.bearer_token_env_var.is_some() {
            self.bearer_token_env_var = higher.bearer_token_env_var;
        }
        if higher.env_file.is_some() {
            self.env_file = higher.env_file;
        }
        if higher.timeout_ms.is_some() {
            self.timeout_ms = higher.timeout_ms;
        }
        if higher.disabled.is_some() {
            self.disabled = higher.disabled;
        }
        if higher.kind.is_some() {
            self.kind = higher.kind;
        }
        if let Some(higher_env) = higher.env {
            let env = self.env.get_or_insert_with(BTreeMap::new);
            env.extend(higher_env);
        }
        if let Some(higher_headers) = higher.headers {
            let headers = self.headers.get_or_insert_with(BTreeMap::new);
            headers.extend(higher_headers);
        }
        self.warnings.extend(higher.warnings);
    }

    fn finish(self, name: &str, path: &Path) -> Result<ServerConfig> {
        let disabled = self.disabled.unwrap_or(false);
        match self.kind {
            Some(DraftKind::Http) => {
                if self.url.is_none() {
                    bail!(
                        "{}: server '{name}': HTTP server requires 'url'",
                        path.display()
                    );
                }
                if self.command.is_some() {
                    bail!(
                        "{}: server '{name}': HTTP server cannot also define 'command'",
                        path.display()
                    );
                }
            }
            Some(DraftKind::Stdio) => {
                if self.command.is_none() {
                    bail!(
                        "{}: server '{name}': stdio server requires 'command'",
                        path.display()
                    );
                }
                if self.url.is_some() {
                    bail!(
                        "{}: server '{name}': stdio server cannot also define 'url'",
                        path.display()
                    );
                }
            }
            None => {}
        }
        if self.command.is_some() && self.url.is_some() {
            bail!(
                "{}: server '{name}': defines both 'command' and 'url'",
                path.display()
            );
        }
        if self.command.is_none() && self.url.is_none() && !disabled {
            bail!(
                "{}: server '{name}': has neither 'command' nor 'url'",
                path.display()
            );
        }
        if !disabled
            && self
                .headers
                .as_ref()
                .is_some_and(|headers| !headers.is_empty())
            && self.url.is_none()
        {
            bail!(
                "{}: server '{name}': 'headers' requires a remote HTTP server",
                path.display()
            );
        }
        if self.bearer_token_env_var.is_some() && self.url.is_none() && !disabled {
            bail!(
                "{}: server '{name}': 'bearer_token_env_var' requires a remote HTTP server",
                path.display()
            );
        }
        let config = ServerConfig {
            command: self.command.filter(|value| !value.is_empty()),
            args: self.args.unwrap_or_default(),
            env: self.env.unwrap_or_default(),
            cwd: self.cwd.filter(|value| !value.is_empty()),
            url: self.url.filter(|value| !value.is_empty()),
            headers: self.headers.unwrap_or_default(),
            bearer_token_env_var: self.bearer_token_env_var.filter(|value| !value.is_empty()),
            env_file: self.env_file.filter(|value| !value.is_empty()),
            timeout_ms: self.timeout_ms,
            disabled,
            extra: BTreeMap::new(),
        };
        config
            .validate()
            .map_err(|error| anyhow!("{}: server '{name}': {error}", path.display()))?;
        Ok(config)
    }
}

pub(crate) fn discover(opts: &LoadOptions, clients: &[Client]) -> Result<Discovery> {
    let mut specs = Vec::new();
    let mut expected = BTreeSet::new();
    for client in clients {
        let (client_specs, client_expected) = client_sources(opts, *client);
        specs.extend(client_specs);
        expected.extend(client_expected);
    }
    dedup_specs(&mut specs);
    expected.extend(specs.iter().map(|spec| spec.path.clone()));

    let mut sources = BTreeSet::new();
    let mut parsed = Vec::new();
    let home_path = home(opts);
    for spec in specs {
        if !spec.path.is_file() {
            continue;
        }
        sources.insert(spec.path.clone());
        let parsed_source = parse_source(&spec, home_path.as_deref())?;
        parsed.push((spec, parsed_source));
    }

    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    for client in clients {
        let client_sources: Vec<(&SourceSpec, &ParsedSource)> = parsed
            .iter()
            .filter(|(spec, _)| spec.client == *client)
            .map(|(spec, source)| (spec, source))
            .collect();
        let (client_candidates, client_warnings) = finalize_client(*client, opts, &client_sources)?;
        candidates.extend(client_candidates);
        warnings.extend(client_warnings);
    }
    candidates.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then(left.client.cmp(&right.client))
            .then(left.paths.cmp(&right.paths))
    });
    warnings.sort();
    warnings.dedup();

    Ok(Discovery {
        candidates,
        sources: sources.into_iter().collect(),
        warnings,
        expected: expected.into_iter().collect(),
    })
}

pub(crate) fn discover_override(
    opts: &LoadOptions,
    client: Client,
    path: &Path,
) -> Result<Discovery> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        opts.cwd.join(path)
    };
    if !path.is_file() {
        bail!("source file does not exist: {}", path.display());
    }
    let (base, workspace) = source_base_for_path(&path, &opts.cwd);
    let mut specs = Vec::new();
    match client {
        Client::Claude
            if path.file_name().and_then(|name| name.to_str()) == Some(".claude.json") =>
        {
            specs.push(SourceSpec {
                client,
                path: path.clone(),
                base: path.parent().unwrap_or(&opts.cwd).to_path_buf(),
                workspace: opts.cwd.clone(),
                priority: 10,
                merge: false,
                parser: Parser::ClaudeUser,
                syntax: Syntax::Claude,
            });
            specs.push(SourceSpec {
                client,
                path: path.clone(),
                base: opts.cwd.clone(),
                workspace: opts.cwd.clone(),
                priority: 20,
                merge: false,
                parser: Parser::ClaudeProject,
                syntax: Syntax::Claude,
            });
        }
        _ => specs.push(SourceSpec {
            client,
            path: path.clone(),
            base,
            workspace,
            priority: 10,
            merge: matches!(client, Client::Opencode | Client::Codex | Client::Devin),
            parser: parser_for_client(client),
            syntax: syntax_for_client(client),
        }),
    }
    let mut parsed = Vec::new();
    let home_path = home(opts);
    for spec in &specs {
        parsed.push((spec.clone(), parse_source(spec, home_path.as_deref())?));
    }
    let references: Vec<(&SourceSpec, &ParsedSource)> =
        parsed.iter().map(|(spec, source)| (spec, source)).collect();
    let (mut candidates, warnings) = finalize_client(client, opts, &references)?;
    candidates.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Discovery {
        candidates,
        sources: vec![path.clone()],
        warnings,
        expected: vec![path],
    })
}

fn dedup_specs(specs: &mut Vec<SourceSpec>) {
    let mut seen = BTreeSet::new();
    specs.retain(|spec| {
        seen.insert((
            spec.client,
            spec.path.clone(),
            spec.priority,
            spec.parser as u8,
        ))
    });
}

fn parser_for_client(client: Client) -> Parser {
    match client {
        Client::Vscode => Parser::VsCode,
        Client::Copilot | Client::Cursor | Client::Devin | Client::Claude => Parser::McpServers,
        Client::Opencode => Parser::OpenCode,
        Client::Codex => Parser::Codex,
    }
}

fn syntax_for_client(client: Client) -> Syntax {
    match client {
        Client::Vscode | Client::Cursor => Syntax::Vscode,
        Client::Copilot => Syntax::Portable,
        Client::Opencode => Syntax::Opencode,
        Client::Claude => Syntax::Claude,
        Client::Codex => Syntax::Literal,
        Client::Devin => Syntax::Devin,
    }
}

fn source_base_for_path(path: &Path, cwd: &Path) -> (PathBuf, PathBuf) {
    let parent = path.parent().unwrap_or(cwd);
    let is_project_dir = parent
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".vscode" | ".cursor" | ".devin" | ".codex" | ".github"
            )
        });
    if is_project_dir {
        let project = parent.parent().unwrap_or(parent);
        (project.to_path_buf(), project.to_path_buf())
    } else {
        (parent.to_path_buf(), parent.to_path_buf())
    }
}

fn client_sources(opts: &LoadOptions, client: Client) -> (Vec<SourceSpec>, Vec<PathBuf>) {
    let mut specs = Vec::new();
    let mut expected = Vec::new();
    match client {
        Client::Vscode => {
            if let Some(path) = vscode_user_config(opts) {
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::VsCode,
                    Syntax::Vscode,
                    false,
                );
            }
            for path in vscode_profile_configs(opts) {
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::VsCode,
                    Syntax::Vscode,
                    false,
                );
            }
            let expected_path = opts.cwd.join(".vscode").join("mcp.json");
            if let Some((path, root)) = find_upward_file(&opts.cwd, ".vscode/mcp.json") {
                push_workspace(
                    &mut specs,
                    client,
                    path,
                    root,
                    Parser::VsCode,
                    Syntax::Vscode,
                    false,
                );
            }
            expected.push(expected_path);
        }
        Client::Copilot => {
            if let Some(path) = copilot_config(opts) {
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::McpServers,
                    Syntax::Portable,
                    false,
                );
            }
            for relative in [".mcp.json", ".github/mcp.json"] {
                let expected_path = opts.cwd.join(relative);
                if let Some((path, root)) = find_upward_file(&opts.cwd, relative) {
                    push_workspace(
                        &mut specs,
                        client,
                        path,
                        root,
                        Parser::McpServers,
                        Syntax::Portable,
                        false,
                    );
                }
                expected.push(expected_path);
            }
        }
        Client::Cursor => {
            if let Some(home) = home(opts) {
                let path = home.join(".cursor").join("mcp.json");
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::McpServers,
                    Syntax::Vscode,
                    false,
                );
            }
            let expected_path = opts.cwd.join(".cursor").join("mcp.json");
            if let Some((path, root)) = find_upward_file(&opts.cwd, ".cursor/mcp.json") {
                push_workspace(
                    &mut specs,
                    client,
                    path,
                    root,
                    Parser::McpServers,
                    Syntax::Vscode,
                    false,
                );
            }
            expected.push(expected_path);
        }
        Client::Opencode => {
            let project_root = find_upward_file(&opts.cwd, "opencode.json")
                .or_else(|| find_upward_file(&opts.cwd, "opencode.jsonc"))
                .map(|(_, root)| root)
                .unwrap_or_else(|| opts.cwd.clone());
            if let Some(config_dir) = xdg_config(opts) {
                let base = config_dir.join("opencode");
                let json = base.join("opencode.json");
                let jsonc = base.join("opencode.jsonc");
                expected.push(json.clone());
                expected.push(jsonc.clone());
                let path = if json.is_file() {
                    Some(json)
                } else if jsonc.is_file() {
                    Some(jsonc)
                } else {
                    None
                };
                if let Some(path) = path {
                    specs.push(SourceSpec {
                        client,
                        path,
                        base: project_root.clone(),
                        workspace: project_root.clone(),
                        priority: 10,
                        merge: true,
                        parser: Parser::OpenCode,
                        syntax: Syntax::Opencode,
                    });
                }
            }
            if let Some(custom) = opts
                .env
                .get("OPENCODE_CONFIG")
                .filter(|value| !value.is_empty())
            {
                let path = if Path::new(custom).is_absolute() {
                    PathBuf::from(custom)
                } else {
                    opts.cwd.join(custom)
                };
                expected.push(path.clone());
                specs.push(SourceSpec {
                    client,
                    path,
                    base: project_root.clone(),
                    workspace: project_root.clone(),
                    priority: 20,
                    merge: true,
                    parser: Parser::OpenCode,
                    syntax: Syntax::Opencode,
                });
            }
            let expected_json = opts.cwd.join("opencode.json");
            let expected_jsonc = opts.cwd.join("opencode.jsonc");
            if let Some((path, root)) = find_upward_file(&opts.cwd, "opencode.json")
                .or_else(|| find_upward_file(&opts.cwd, "opencode.jsonc"))
            {
                specs.push(SourceSpec {
                    client,
                    path,
                    base: root.clone(),
                    workspace: root,
                    priority: 30,
                    merge: true,
                    parser: Parser::OpenCode,
                    syntax: Syntax::Opencode,
                });
            }
            expected.push(expected_json);
            expected.push(expected_jsonc);
        }
        Client::Claude => {
            if let Some(path) = claude_desktop_config(opts) {
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::McpServers,
                    Syntax::Literal,
                    false,
                );
            }
            let workspace_root = find_upward_file(&opts.cwd, ".mcp.json")
                .map(|(_, root)| root)
                .unwrap_or_else(|| opts.cwd.clone());
            if let Some(path) = claude_code_config(opts) {
                expected.push(path.clone());
                specs.push(SourceSpec {
                    client,
                    path: path.clone(),
                    base: path.parent().unwrap_or(&opts.cwd).to_path_buf(),
                    workspace: workspace_root.clone(),
                    priority: 20,
                    merge: false,
                    parser: Parser::ClaudeUser,
                    syntax: Syntax::Claude,
                });
                specs.push(SourceSpec {
                    client,
                    path,
                    base: opts.cwd.clone(),
                    workspace: opts.cwd.clone(),
                    priority: 30,
                    merge: false,
                    parser: Parser::ClaudeProject,
                    syntax: Syntax::Claude,
                });
            }
            let expected_project = opts.cwd.join(".mcp.json");
            if let Some((path, root)) = find_upward_file(&opts.cwd, ".mcp.json") {
                push_workspace(
                    &mut specs,
                    client,
                    path,
                    root,
                    Parser::McpServers,
                    Syntax::Claude,
                    false,
                );
            }
            expected.push(expected_project);
        }
        Client::Codex => {
            if let Some(config_home) = codex_home(opts) {
                let path = config_home.join("config.toml");
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::Codex,
                    Syntax::Literal,
                    true,
                );
            }
            let expected_path = opts.cwd.join(".codex").join("config.toml");
            if let Some((path, root)) = find_upward_file(&opts.cwd, ".codex/config.toml") {
                push_workspace(
                    &mut specs,
                    client,
                    path,
                    root,
                    Parser::Codex,
                    Syntax::Literal,
                    true,
                );
            }
            expected.push(expected_path);
        }
        Client::Devin => {
            if let Some(config_dir) = devin_config_dir(opts) {
                let path = config_dir.join("devin").join("mcp_config.json");
                expected.push(path.clone());
                push_user(
                    &mut specs,
                    client,
                    path,
                    opts.cwd.clone(),
                    Parser::McpServers,
                    Syntax::Devin,
                    true,
                );
            }
            let expected_project = opts.cwd.join(".devin").join("mcp_config.json");
            let expected_local = opts.cwd.join(".devin").join("mcp_config.local.json");
            if let Some((_, root)) = find_upward_file(&opts.cwd, ".devin/mcp_config.json")
                .or_else(|| find_upward_file(&opts.cwd, ".devin/mcp_config.local.json"))
            {
                for (relative, priority) in [
                    (".devin/mcp_config.json", 20),
                    (".devin/mcp_config.local.json", 30),
                ] {
                    let path = root.join(relative);
                    specs.push(SourceSpec {
                        client,
                        path,
                        base: root.clone(),
                        workspace: root.clone(),
                        priority,
                        merge: true,
                        parser: Parser::McpServers,
                        syntax: Syntax::Devin,
                    });
                }
            }
            expected.push(expected_project);
            expected.push(expected_local);
        }
    }
    (specs, expected)
}

#[allow(clippy::too_many_arguments)]
fn push_user(
    specs: &mut Vec<SourceSpec>,
    client: Client,
    path: PathBuf,
    workspace: PathBuf,
    parser: Parser,
    syntax: Syntax,
    merge: bool,
) {
    let base = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| workspace.clone());
    specs.push(SourceSpec {
        client,
        path,
        base,
        workspace,
        priority: 10,
        merge,
        parser,
        syntax,
    });
}

#[allow(clippy::too_many_arguments)]
fn push_workspace(
    specs: &mut Vec<SourceSpec>,
    client: Client,
    path: PathBuf,
    root: PathBuf,
    parser: Parser,
    syntax: Syntax,
    merge: bool,
) {
    specs.push(SourceSpec {
        client,
        path,
        base: root.clone(),
        workspace: root,
        priority: 20,
        merge,
        parser,
        syntax,
    });
}

fn home(opts: &LoadOptions) -> Option<PathBuf> {
    opts.home.clone().or_else(|| {
        opts.env
            .get("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                opts.env
                    .get("USERPROFILE")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
    })
}

fn xdg_config(opts: &LoadOptions) -> Option<PathBuf> {
    opts.env
        .get("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| Some(home(opts)?.join(".config")))
}

fn appdata(opts: &LoadOptions) -> Option<PathBuf> {
    opts.env
        .get("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn vscode_user_config(opts: &LoadOptions) -> Option<PathBuf> {
    Some(vscode_user_dir(opts)?.join("mcp.json"))
}

fn vscode_user_dir(opts: &LoadOptions) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(
            home(opts)?
                .join("Library")
                .join("Application Support")
                .join("Code")
                .join("User"),
        )
    } else if cfg!(windows) {
        Some(appdata(opts)?.join("Code").join("User"))
    } else {
        Some(xdg_config(opts)?.join("Code").join("User"))
    }
}

fn vscode_profile_configs(opts: &LoadOptions) -> Vec<PathBuf> {
    let Some(profiles) = vscode_user_dir(opts).map(|dir| dir.join("profiles")) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(profiles) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path().join("mcp.json"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn copilot_config(opts: &LoadOptions) -> Option<PathBuf> {
    if let Some(home) = opts
        .env
        .get("COPILOT_HOME")
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(home).join("mcp-config.json"));
    }
    Some(home(opts)?.join(".copilot").join("mcp-config.json"))
}

fn claude_desktop_config(opts: &LoadOptions) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(
            home(opts)?
                .join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    } else if cfg!(windows) {
        Some(
            appdata(opts)?
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    } else {
        Some(
            xdg_config(opts)?
                .join("Claude")
                .join("claude_desktop_config.json"),
        )
    }
}

fn claude_config_dir(opts: &LoadOptions) -> Option<PathBuf> {
    if let Some(dir) = opts
        .env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|value| !value.is_empty())
    {
        return Some(PathBuf::from(dir));
    }
    Some(home(opts)?.join(".claude"))
}

fn claude_code_config(opts: &LoadOptions) -> Option<PathBuf> {
    if opts
        .env
        .get("CLAUDE_CONFIG_DIR")
        .is_some_and(|value| !value.is_empty())
    {
        Some(claude_config_dir(opts)?.join(".claude.json"))
    } else {
        Some(home(opts)?.join(".claude.json"))
    }
}

fn codex_home(opts: &LoadOptions) -> Option<PathBuf> {
    if let Some(dir) = opts.env.get("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    Some(home(opts)?.join(".codex"))
}

fn devin_config_dir(opts: &LoadOptions) -> Option<PathBuf> {
    if cfg!(windows) {
        appdata(opts)
    } else {
        xdg_config(opts)
    }
}

fn find_upward_file(start: &Path, relative: &str) -> Option<(PathBuf, PathBuf)> {
    let mut current = Some(start);
    while let Some(directory) = current {
        let candidate = directory.join(relative);
        if candidate.is_file() {
            return Some((candidate, directory.to_path_buf()));
        }
        if directory.join(".git").exists() {
            break;
        }
        current = directory.parent();
    }
    None
}

struct ParsedSource {
    drafts: BTreeMap<String, ServerDraft>,
    warnings: Vec<String>,
}

fn parse_source(spec: &SourceSpec, home: Option<&Path>) -> Result<ParsedSource> {
    match spec.parser {
        Parser::Codex => parse_codex(spec, home),
        Parser::VsCode => parse_json_servers_key(spec, "servers", Some("mcpServers"), home),
        Parser::McpServers => parse_json_servers_key(spec, "mcpServers", Some("servers"), home),
        Parser::ClaudeUser => parse_claude_user(spec, home),
        Parser::ClaudeProject => parse_claude_project(spec, home),
        Parser::OpenCode => parse_opencode(spec, home),
    }
}

fn read_source(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path).with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.len() > MAX_SOURCE_BYTES {
        bail!(
            "{}: configuration is larger than {} MiB",
            path.display(),
            MAX_SOURCE_BYTES / (1024 * 1024)
        );
    }
    let mut text = fs::read_to_string(path)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    if text.starts_with('\u{feff}') {
        text.remove(0);
    }
    Ok(text)
}

fn parse_json_value(text: &str, path: &Path) -> Result<Value> {
    json5::from_str(text).map_err(|_| anyhow!("{}: invalid JSON/JSONC", path.display()))
}

fn parse_json_servers_key(
    spec: &SourceSpec,
    key: &str,
    alternate: Option<&str>,
    home: Option<&Path>,
) -> Result<ParsedSource> {
    let text = read_source(&spec.path)?;
    let value = parse_json_value(&text, &spec.path)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{}: top-level value must be an object", spec.path.display()))?;
    let (servers, warnings) = select_servers_object(object, key, alternate, &spec.path)?;
    let drafts = parse_server_entries(spec, servers, home)?;
    Ok(ParsedSource { drafts, warnings })
}

fn select_servers_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    key: &str,
    alternate: Option<&str>,
    path: &Path,
) -> Result<(&'a serde_json::Map<String, Value>, Vec<String>)> {
    if let Some(value) = object.get(key) {
        return match value.as_object() {
            Some(servers) => Ok((servers, Vec::new())),
            None => bail!("{}: '{key}' must be an object", path.display()),
        };
    }
    if let Some(alternate) = alternate
        && let Some(value) = object.get(alternate)
    {
        let servers = value
            .as_object()
            .ok_or_else(|| anyhow!("{}: '{alternate}' must be an object", path.display()))?;
        return Ok((
            servers,
            vec![format!(
                "{}: using '{alternate}' instead of expected '{key}'",
                path.display()
            )],
        ));
    }
    Ok((&*EMPTY_OBJECT, Vec::new()))
}

static EMPTY_OBJECT: std::sync::LazyLock<serde_json::Map<String, Value>> =
    std::sync::LazyLock::new(serde_json::Map::new);

fn parse_claude_user(spec: &SourceSpec, home: Option<&Path>) -> Result<ParsedSource> {
    parse_json_servers_key(spec, "mcpServers", None, home)
}

fn parse_claude_project(spec: &SourceSpec, home: Option<&Path>) -> Result<ParsedSource> {
    let text = read_source(&spec.path)?;
    let value = parse_json_value(&text, &spec.path)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{}: top-level value must be an object", spec.path.display()))?;
    let Some(projects) = object.get("projects").and_then(Value::as_object) else {
        return Ok(ParsedSource {
            drafts: BTreeMap::new(),
            warnings: Vec::new(),
        });
    };
    let entry = find_project_entry(projects, &spec.workspace);
    let Some(entry) = entry else {
        return Ok(ParsedSource {
            drafts: BTreeMap::new(),
            warnings: Vec::new(),
        });
    };
    let servers = entry
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let drafts = parse_server_entries(spec, &servers, home)?;
    Ok(ParsedSource {
        drafts,
        warnings: Vec::new(),
    })
}

fn find_project_entry<'a>(
    projects: &'a serde_json::Map<String, Value>,
    project: &Path,
) -> Option<&'a serde_json::Map<String, Value>> {
    if let Some(entry) = projects
        .get(&project.to_string_lossy().into_owned())
        .and_then(Value::as_object)
    {
        return Some(entry);
    }
    let canonical = fs::canonicalize(project).ok();
    for (key, value) in projects {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let key_path = Path::new(key);
        if key_path == project {
            return Some(entry);
        }
        if let (Some(canonical), Ok(key_canonical)) = (&canonical, fs::canonicalize(key_path))
            && *canonical == key_canonical
        {
            return Some(entry);
        }
    }
    None
}

fn parse_opencode(spec: &SourceSpec, home: Option<&Path>) -> Result<ParsedSource> {
    let text = read_source(&spec.path)?;
    let value = parse_json_value(&text, &spec.path)?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("{}: top-level value must be an object", spec.path.display()))?;
    let Some(mcp) = object.get("mcp").and_then(Value::as_object) else {
        return Ok(ParsedSource {
            drafts: BTreeMap::new(),
            warnings: Vec::new(),
        });
    };
    let servers = if let Some(nested) = mcp.get("servers").and_then(Value::as_object) {
        let mcp_looks_like_servers = mcp.values().filter_map(Value::as_object).any(|entry| {
            entry.contains_key("type") || entry.contains_key("command") || entry.contains_key("url")
        });
        if mcp_looks_like_servers { mcp } else { nested }
    } else {
        mcp
    };
    let drafts = parse_server_entries(spec, servers, home)?;
    Ok(ParsedSource {
        drafts,
        warnings: Vec::new(),
    })
}

fn parse_server_entries(
    spec: &SourceSpec,
    servers: &serde_json::Map<String, Value>,
    home: Option<&Path>,
) -> Result<BTreeMap<String, ServerDraft>> {
    let mut drafts = BTreeMap::new();
    for (name, value) in servers {
        if name.trim().is_empty() {
            bail!("{}: server name cannot be empty", spec.path.display());
        }
        let draft = parse_json_server(spec, name, value, home)
            .with_context(|| format!("{}: server '{name}'", spec.path.display()))?;
        drafts.insert(name.clone(), draft);
    }
    Ok(drafts)
}

#[derive(Deserialize, Default)]
struct JsonDraft {
    #[serde(default)]
    command: Option<Value>,
    #[serde(default)]
    args: Option<Vec<Value>>,
    #[serde(default)]
    env: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    environment: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "serverUrl", alias = "server_url")]
    server_url: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    http_headers: Option<BTreeMap<String, Value>>,
    #[serde(default, rename = "bearerTokenEnvVar", alias = "bearer_token_env_var")]
    bearer_token_env_var: Option<String>,
    #[serde(default, rename = "envFile")]
    env_file: Option<String>,
    #[serde(default, rename = "env_file")]
    env_file_snake: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    env_http_headers: Option<BTreeMap<String, Value>>,
    #[serde(default, rename = "headersHelper")]
    headers_helper: Option<Value>,
    #[serde(default)]
    http_headers_helper: Option<Value>,
    #[serde(default)]
    oauth: Option<Value>,
    #[serde(default, rename = "oauthClientId")]
    oauth_client_id: Option<String>,
    #[serde(default, rename = "oauthClientSecret")]
    oauth_client_secret: Option<String>,
    #[serde(default, rename = "oauthResource")]
    oauth_resource: Option<String>,
    #[serde(default)]
    auth: Option<Value>,
}

fn parse_json_server(
    spec: &SourceSpec,
    name: &str,
    value: &Value,
    home: Option<&Path>,
) -> Result<ServerDraft> {
    let raw: JsonDraft =
        serde_json::from_value(value.clone()).map_err(|_| anyhow!("invalid server entry shape"))?;
    let mut draft = ServerDraft::default();
    let context = ConvertContext {
        base: &spec.base,
        workspace: &spec.workspace,
        home,
    };

    let mut command_args = None;
    match raw.command {
        Some(Value::String(command)) => {
            let command = convert_text(&command, spec.syntax, &context, name, "command")?;
            draft.command = Some(normalize_command(&command, &context));
        }
        Some(Value::Array(values)) => {
            let mut parts = Vec::new();
            for (index, value) in values.into_iter().enumerate() {
                let value = value_to_string(value, &format!("command[{index}]"))?;
                parts.push(convert_text(
                    &value,
                    spec.syntax,
                    &context,
                    name,
                    &format!("command[{index}]"),
                )?);
            }
            if parts.is_empty() {
                bail!("'command' array is empty");
            }
            command_args = Some(parts);
        }
        Some(Value::Null) | None => {}
        Some(_) => bail!("'command' must be a string or array"),
    }
    if let Some(mut parts) = command_args {
        let mut first = parts.remove(0);
        first = normalize_command(&first, &context);
        draft.command = Some(first);
        let mut args = if parts.is_empty() { Vec::new() } else { parts };
        if let Some(extra) = raw.args {
            for (index, value) in extra.into_iter().enumerate() {
                let value = value_to_string(value, &format!("args[{index}]"))?;
                args.push(convert_text(
                    &value,
                    spec.syntax,
                    &context,
                    name,
                    &format!("args[{index}]"),
                )?);
            }
        }
        draft.args = Some(args);
    } else if let Some(values) = raw.args {
        let mut args = Vec::new();
        for (index, value) in values.into_iter().enumerate() {
            let value = value_to_string(value, &format!("args[{index}]"))?;
            args.push(convert_text(
                &value,
                spec.syntax,
                &context,
                name,
                &format!("args[{index}]"),
            )?);
        }
        draft.args = Some(args);
    }

    let mut env = BTreeMap::new();
    for source in [raw.environment, raw.env].into_iter().flatten() {
        for (key, value) in source {
            if value.is_null() {
                continue;
            }
            let value = value_to_string(value, &format!("env.{key}"))?;
            env.insert(
                key.clone(),
                convert_text(&value, spec.syntax, &context, name, &format!("env.{key}"))?,
            );
        }
    }
    if !env.is_empty() {
        draft.env = Some(env);
    }

    if let Some(cwd) = raw.cwd {
        let cwd = convert_text(&cwd, spec.syntax, &context, name, "cwd")?;
        draft.cwd = Some(normalize_cwd(&cwd, &context));
    }
    let url = match (raw.url, raw.server_url) {
        (Some(left), Some(right)) if left != right => {
            bail!("'url' and 'serverUrl' disagree")
        }
        (Some(url), _) | (None, Some(url)) => Some(url),
        (None, None) => None,
    };
    if let Some(url) = url.filter(|url| !url.is_empty()) {
        draft.url = Some(convert_text(&url, spec.syntax, &context, name, "url")?);
    }
    let mut headers = BTreeMap::new();
    for source in [raw.http_headers, raw.headers].into_iter().flatten() {
        for (key, value) in source {
            if value.is_null() {
                continue;
            }
            let value = value_to_string(value, &format!("headers.{key}"))?;
            headers.insert(
                key.clone(),
                convert_text(
                    &value,
                    spec.syntax,
                    &context,
                    name,
                    &format!("headers.{key}"),
                )?,
            );
        }
    }
    if let Some(env_headers) = raw.env_http_headers {
        for (header, env_name) in env_headers {
            let env_name = value_to_string(env_name, &format!("env_http_headers.{header}"))?;
            headers.insert(header, format!("${{{env_name}}}"));
        }
    }
    if !headers.is_empty() {
        draft.headers = Some(headers);
    }
    if let Some(env_file) = raw.env_file.or(raw.env_file_snake) {
        let value = convert_text(&env_file, spec.syntax, &context, name, "envFile")?;
        draft.env_file = Some(normalize_cwd(&value, &context));
    }
    draft.bearer_token_env_var = raw.bearer_token_env_var.filter(|value| !value.is_empty());
    draft.timeout_ms = raw.timeout_ms.or(raw.timeout);
    draft.disabled = raw.disabled.or_else(|| raw.enabled.map(|enabled| !enabled));
    let kind_value = match (raw.kind.as_deref(), raw.transport.as_deref()) {
        (Some(left), Some(right)) if !left.eq_ignore_ascii_case(right) => {
            bail!("'type' and 'transport' disagree")
        }
        (Some(value), _) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    draft.kind = match kind_value {
        Some(value) => Some(parse_client_kind(value)?),
        None if draft.url.is_some() && draft.command.is_none() => Some(DraftKind::Http),
        None if draft.command.is_some() && draft.url.is_none() => Some(DraftKind::Stdio),
        None => None,
    };
    match (draft.kind, draft.command.is_some(), draft.url.is_some()) {
        (Some(DraftKind::Http), true, _) => {
            bail!("HTTP server cannot also define 'command'")
        }
        (Some(DraftKind::Stdio), _, true) => {
            bail!("stdio server cannot also define 'url'")
        }
        _ => {}
    }
    if draft.bearer_token_env_var.is_some()
        && draft.url.is_none()
        && !draft.disabled.unwrap_or(false)
    {
        bail!("'bearerTokenEnvVar' requires a remote HTTP server");
    }
    if let Some(headers) = draft.headers.as_mut()
        && draft.bearer_token_env_var.is_some()
        && contains_header(headers, "authorization")
    {
        headers.retain(|key, _| !key.eq_ignore_ascii_case("authorization"));
        draft.warn("Authorization header removed because bearer_token_env_var takes precedence");
    }
    if raw.headers_helper.is_some() || raw.http_headers_helper.is_some() {
        draft.warn(
            "dynamic header helper ignored; configure static headers in the cm global config",
        );
    }
    match spec.client {
        Client::Cursor if raw.auth.is_some() => draft.warn(
            "static OAuth configuration is not executed by cm; configure an Authorization header",
        ),
        Client::Opencode
            if raw
                .oauth
                .as_ref()
                .is_some_and(|oauth| !oauth.is_boolean() || oauth.as_bool() == Some(true)) =>
        {
            draft.warn(
                "OAuth authorization flow is not performed by cm; an existing unexpired token is reused if available",
            );
        }
        Client::Devin
            if raw.oauth.is_some()
                || raw.oauth_client_id.is_some()
                || raw.oauth_client_secret.is_some()
                || raw.oauth_resource.is_some() =>
        {
            draft.warn(
                "OAuth authorization flow is not performed by cm; an existing unexpired token is reused if available",
            );
        }
        _ => {}
    }
    Ok(draft)
}

fn contains_header(headers: &BTreeMap<String, String>, name: &str) -> bool {
    headers.keys().any(|key| key.eq_ignore_ascii_case(name))
}

fn parse_client_kind(value: &str) -> Result<DraftKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" | "local" => Ok(DraftKind::Stdio),
        "http" | "streamable-http" | "streamable_http" | "streamablehttp" | "remote" => {
            Ok(DraftKind::Http)
        }
        "sse" => bail!("deprecated SSE transport is unsupported; use streamable HTTP"),
        "ws" | "websocket" => bail!("WebSocket transport is unsupported; use streamable HTTP"),
        "sdk" => bail!("SDK-only server entries cannot be imported"),
        other => bail!("unsupported transport type '{other}'"),
    }
}

fn value_to_string(value: Value, field: &str) -> Result<String> {
    match value {
        Value::String(value) => Ok(value),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Null => Ok(String::new()),
        Value::Array(_) | Value::Object(_) => {
            bail!("'{field}' must be a string, number, or boolean")
        }
    }
}

fn parse_codex(spec: &SourceSpec, home: Option<&Path>) -> Result<ParsedSource> {
    let text = read_source(&spec.path)?;
    let value: toml::Value =
        toml::from_str(&text).map_err(|_| anyhow!("{}: invalid TOML", spec.path.display()))?;
    let table = value
        .as_table()
        .ok_or_else(|| anyhow!("{}: top-level value must be a table", spec.path.display()))?;
    let Some(servers) = table.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Ok(ParsedSource {
            drafts: BTreeMap::new(),
            warnings: Vec::new(),
        });
    };
    let mut drafts = BTreeMap::new();
    for (name, value) in servers {
        if name.trim().is_empty() {
            bail!("{}: server name cannot be empty", spec.path.display());
        }
        let table = value
            .as_table()
            .ok_or_else(|| anyhow!("{}: server '{name}' must be a table", spec.path.display()))?;
        let draft = parse_codex_server(spec, name, table, home)
            .with_context(|| format!("{}: server '{name}'", spec.path.display()))?;
        drafts.insert(name.clone(), draft);
    }
    Ok(ParsedSource {
        drafts,
        warnings: Vec::new(),
    })
}

fn parse_codex_server(
    spec: &SourceSpec,
    name: &str,
    table: &toml::map::Map<String, toml::Value>,
    home: Option<&Path>,
) -> Result<ServerDraft> {
    let context = ConvertContext {
        base: &spec.base,
        workspace: &spec.workspace,
        home,
    };
    let mut draft = ServerDraft::default();
    let command = table
        .get("command")
        .and_then(toml::Value::as_str)
        .map(|command| convert_text(command, Syntax::Literal, &context, name, "command"))
        .transpose()?;
    draft.command = command.map(|command| normalize_command(&command, &context));
    if let Some(args) = table.get("args").and_then(toml::Value::as_array) {
        let mut values = Vec::new();
        for (index, value) in args.iter().enumerate() {
            let value =
                toml_string(value).ok_or_else(|| anyhow!("'args[{index}]' must be a string"))?;
            values.push(convert_text(
                value,
                Syntax::Literal,
                &context,
                name,
                &format!("args[{index}]"),
            )?);
        }
        draft.args = Some(values);
    }
    if let Some(env) = table.get("env").and_then(toml::Value::as_table) {
        let mut values = BTreeMap::new();
        for (key, value) in env {
            let value =
                toml_string(value).ok_or_else(|| anyhow!("'env.{key}' must be a string"))?;
            values.insert(
                key.clone(),
                convert_text(
                    value,
                    Syntax::Literal,
                    &context,
                    name,
                    &format!("env.{key}"),
                )?,
            );
        }
        draft.env = Some(values);
    }
    if let Some(cwd) = table.get("cwd").and_then(toml::Value::as_str) {
        let value = convert_text(cwd, Syntax::Literal, &context, name, "cwd")?;
        draft.cwd = Some(normalize_cwd(&value, &context));
    }
    draft.url = table
        .get("url")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|url| convert_text(url, Syntax::Literal, &context, name, "url"))
        .transpose()?;
    draft.command = draft.command.filter(|value| !value.is_empty());
    let mut headers = BTreeMap::new();
    if let Some(values) = table.get("http_headers").and_then(toml::Value::as_table) {
        for (key, value) in values {
            let value = toml_string(value)
                .ok_or_else(|| anyhow!("'http_headers.{key}' must be a string"))?;
            headers.insert(
                key.clone(),
                convert_text(
                    value,
                    Syntax::Literal,
                    &context,
                    name,
                    &format!("http_headers.{key}"),
                )?,
            );
        }
    }
    if let Some(values) = table
        .get("env_http_headers")
        .and_then(toml::Value::as_table)
    {
        for (key, value) in values {
            let env_name = toml_string(value)
                .ok_or_else(|| anyhow!("'env_http_headers.{key}' must be a string"))?;
            headers.insert(key.clone(), format!("${{{env_name}}}"));
        }
    }
    draft.bearer_token_env_var = table
        .get("bearer_token_env_var")
        .and_then(toml::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if draft.bearer_token_env_var.is_some() && contains_header(&headers, "authorization") {
        headers.retain(|key, _| !key.eq_ignore_ascii_case("authorization"));
        draft.warn("Authorization header removed because bearer_token_env_var takes precedence");
    }
    if !headers.is_empty() {
        draft.headers = Some(headers);
    }
    draft.disabled = table
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .map(|enabled| !enabled);
    draft.timeout_ms = table
        .get("startup_timeout_ms")
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .or_else(|| {
            table
                .get("startup_timeout_sec")
                .and_then(toml::Value::as_float)
                .map(|value| (value * 1000.0) as u64)
        })
        .or_else(|| {
            table
                .get("startup_timeout_sec")
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| value.saturating_mul(1000))
        })
        .or_else(|| {
            table
                .get("tool_timeout_sec")
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok())
                .map(|value| value.saturating_mul(1000))
        });
    if table.get("http_headers_helper").is_some() {
        draft.warn(
            "dynamic header helper ignored; configure static headers in the cm global config",
        );
    }
    if table.get("oauth").is_some() {
        draft.warn(
            "OAuth authorization flow is not performed by cm; an existing unexpired token is reused if available",
        );
    }
    draft.kind = if draft.url.is_some() && draft.command.is_none() {
        Some(DraftKind::Http)
    } else if draft.command.is_some() && draft.url.is_none() {
        Some(DraftKind::Stdio)
    } else {
        None
    };
    Ok(draft)
}

fn toml_string(value: &toml::Value) -> Option<&str> {
    value.as_str()
}

struct ConvertContext<'a> {
    base: &'a Path,
    workspace: &'a Path,
    home: Option<&'a Path>,
}

fn convert_text(
    value: &str,
    syntax: Syntax,
    context: &ConvertContext<'_>,
    server: &str,
    field: &str,
) -> Result<String> {
    match syntax {
        Syntax::Literal => Ok(escape_literal(value)),
        Syntax::Opencode => convert_opencode_refs(value, context, server, field),
        Syntax::Vscode => convert_vscode_refs(value, context, server, field),
        Syntax::Portable => {
            convert_dollar_refs(value, DollarMode::Portable, context, server, field)
        }
        Syntax::Claude => convert_dollar_refs(value, DollarMode::Claude, context, server, field),
        Syntax::Devin => convert_dollar_refs(value, DollarMode::Devin, context, server, field),
    }
}

#[derive(Clone, Copy)]
enum DollarMode {
    Portable,
    Claude,
    Devin,
}

fn convert_dollar_refs(
    value: &str,
    mode: DollarMode,
    context: &ConvertContext<'_>,
    server: &str,
    field: &str,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find("${") {
        output.push_str(&rest[..index]);
        let tail = &rest[index + 2..];
        let Some(end) = tail.find('}') else {
            output.push_str(&escape_literal(rest));
            return Ok(output);
        };
        let inner = &tail[..end];
        if let Some(name) = inner.strip_prefix("input:") {
            bail!(
                "server '{server}': interactive input variable '{name}' in {field} is unsupported; \
                 export an environment variable and reference it instead"
            );
        }
        if let Some(file) = inner.strip_prefix("file:") {
            if matches!(mode, DollarMode::Devin) {
                let path = read_substitution_file(file, context)?;
                output.push_str(&escape_literal(&path));
            } else {
                bail!(
                    "server '{server}': file substitution in {field} is unsupported; \
                     use an environment variable"
                );
            }
        } else if let Some(inner) = inner.strip_prefix("env:") {
            output.push_str(&format!("${{{inner}}}"));
        } else if matches!(mode, DollarMode::Devin) {
            output.push_str(&escape_literal(&format!("${{{inner}}}")));
        } else {
            output.push_str(&format!("${{{inner}}}"));
        }
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn convert_vscode_refs(
    value: &str,
    context: &ConvertContext<'_>,
    server: &str,
    field: &str,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find("${") {
        output.push_str(&rest[..index]);
        let tail = &rest[index + 2..];
        let Some(end) = tail.find('}') else {
            output.push_str(&escape_literal(rest));
            return Ok(output);
        };
        let inner = &tail[..end];
        if let Some(name) = inner.strip_prefix("input:") {
            bail!(
                "server '{server}': interactive input variable '{name}' in {field} is unsupported; \
                 export an environment variable and reference it instead"
            );
        }
        if let Some(name) = inner.strip_prefix("env:") {
            output.push_str(&format!("${{{name}}}"));
        } else if inner == "workspaceFolder" {
            output.push_str(&escape_literal(&context.workspace.to_string_lossy()));
        } else if inner == "workspaceFolderBasename" {
            let name = context
                .workspace
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            output.push_str(&escape_literal(&name));
        } else if inner == "userHome" {
            let home = context.home.ok_or_else(|| {
                anyhow!("server '{server}': home directory is not set for {field}")
            })?;
            output.push_str(&escape_literal(&home.to_string_lossy()));
        } else if inner == "pathSeparator" || inner == "/" {
            output.push_str(&escape_literal(std::path::MAIN_SEPARATOR_STR));
        } else if inner.starts_with("workspaceFolder:") {
            bail!(
                "server '{server}': multi-root workspace variable in {field} is unsupported; \
                 use an absolute path"
            );
        } else {
            output.push_str(&escape_literal(&format!("${{{inner}}}")));
        }
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn convert_opencode_refs(
    value: &str,
    context: &ConvertContext<'_>,
    server: &str,
    field: &str,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let env_index = rest.find("{env:");
        let file_index = rest.find("{file:");
        let next = match (env_index, file_index) {
            (Some(left), Some(right)) if left < right => Some((left, false)),
            (Some(_), Some(right)) => Some((right, true)),
            (Some(index), None) => Some((index, false)),
            (None, Some(index)) => Some((index, true)),
            (None, None) => None,
        };
        let Some((index, is_file)) = next else {
            output.push_str(&escape_literal(rest));
            break;
        };
        output.push_str(&escape_literal(&rest[..index]));
        let prefix = if is_file { "{file:" } else { "{env:" };
        let tail = &rest[index + prefix.len()..];
        let Some(end) = tail.find('}') else {
            output.push_str(&escape_literal(&rest[index..]));
            break;
        };
        let inner = &tail[..end];
        if is_file {
            let path = read_substitution_file(inner, context)?;
            output.push_str(&escape_literal(&path));
        } else {
            output.push_str(&format!("${{{inner}}}"));
        }
        rest = &tail[end + 1..];
    }
    let _ = (server, field);
    Ok(output)
}

fn escape_literal(value: &str) -> String {
    value.replace("${", "$${")
}

fn read_substitution_file(value: &str, context: &ConvertContext<'_>) -> Result<String> {
    let home_env = context
        .home
        .map(|home| BTreeMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())]))
        .unwrap_or_default();
    let path = config::resolve_source_path(value, context.base, &home_env);
    let metadata = fs::metadata(&path)
        .with_context(|| format!("cannot read substituted file {}", path.display()))?;
    if metadata.len() > MAX_SECRET_FILE_BYTES {
        bail!(
            "substituted file {} is larger than {} MiB",
            path.display(),
            MAX_SECRET_FILE_BYTES / (1024 * 1024)
        );
    }
    let value = fs::read_to_string(&path)
        .with_context(|| format!("substituted file {} is not valid UTF-8", path.display()))?;
    Ok(value.trim_end_matches(&['\r', '\n'][..]).to_string())
}

fn normalize_cwd(value: &str, context: &ConvertContext<'_>) -> String {
    normalize_path(value, context)
}

fn normalize_command(value: &str, context: &ConvertContext<'_>) -> String {
    if !config::looks_like_path(value) {
        return value.to_string();
    }
    normalize_path(value, context)
}

fn normalize_path(value: &str, context: &ConvertContext<'_>) -> String {
    if value.contains("${") {
        return value.to_string();
    }
    let home_env = context
        .home
        .map(|home| BTreeMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())]))
        .unwrap_or_default();
    config::resolve_source_path(value, context.base, &home_env)
        .to_string_lossy()
        .into_owned()
}

fn finalize_client(
    client: Client,
    opts: &LoadOptions,
    sources: &[(&SourceSpec, &ParsedSource)],
) -> Result<(Vec<Candidate>, Vec<String>)> {
    let mut warnings = Vec::new();
    let mut by_name: BTreeMap<String, Vec<(&SourceSpec, &ServerDraft)>> = BTreeMap::new();
    for (spec, parsed) in sources {
        warnings.extend(parsed.warnings.iter().cloned());
        for (name, draft) in &parsed.drafts {
            by_name
                .entry(name.clone())
                .or_default()
                .push((*spec, draft));
        }
    }
    let mut candidates = Vec::new();
    for (name, mut layers) in by_name {
        layers.sort_by(|(left, _), (right, _)| {
            left.priority
                .cmp(&right.priority)
                .then(left.path.cmp(&right.path))
        });
        let paths: Vec<PathBuf> = layers
            .iter()
            .map(|(spec, _)| spec.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let merge = layers.iter().any(|(spec, _)| spec.merge);
        let selections: Vec<(ServerDraft, Vec<PathBuf>)> = if merge {
            let mut merged: Option<ServerDraft> = None;
            let mut index = 0;
            while index < layers.len() {
                let priority = layers[index].0.priority;
                let mut group = Vec::new();
                while index < layers.len() && layers[index].0.priority == priority {
                    group.push(layers[index]);
                    index += 1;
                }
                let first = group[0].1;
                for (_, draft) in group.iter().skip(1) {
                    if !first.same_definition(draft, true) {
                        let details = group
                            .iter()
                            .map(|(spec, _)| spec.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        bail!(
                            "conflicting same-precedence definitions for MCP server '{name}' in \
                             {client_label} configs: {details}",
                            client_label = client.display_name()
                        );
                    }
                }
                match merged.as_mut() {
                    Some(current) => current.merge_from((*first).clone()),
                    None => merged = Some((*first).clone()),
                }
            }
            vec![(merged.expect("at least one layer"), paths.clone())]
        } else {
            let highest = layers
                .iter()
                .map(|(spec, _)| spec.priority)
                .max()
                .unwrap_or_default();
            let top: Vec<(&SourceSpec, &ServerDraft)> = layers
                .iter()
                .filter(|(spec, _)| spec.priority == highest)
                .map(|(spec, draft)| (*spec, *draft))
                .collect();
            let conflicting = top
                .iter()
                .skip(1)
                .any(|(_, draft)| !top[0].1.same_definition(draft, true));
            // Independent profiles can define different credentials for the same name.
            // Require a caller choice; do not select an arbitrary profile.
            if conflicting && client == Client::Vscode && highest == 10 {
                let mut groups: Vec<(ServerDraft, Vec<PathBuf>)> = Vec::new();
                for (spec, draft) in top {
                    match groups
                        .iter_mut()
                        .find(|(existing, _)| existing.same_definition(draft, true))
                    {
                        Some((_, group_paths)) => group_paths.push(spec.path.clone()),
                        None => groups.push(((*draft).clone(), vec![spec.path.clone()])),
                    }
                }
                for (_, group_paths) in &mut groups {
                    group_paths.sort();
                    group_paths.dedup();
                }
                groups
            } else {
                let selected = top[0].1;
                for (_, draft) in top.iter().skip(1) {
                    if !selected.same_definition(draft, true) {
                        let details = top
                            .iter()
                            .map(|(spec, _)| spec.path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        bail!(
                            "conflicting same-precedence definitions for MCP server '{name}' in \
                             {client_label} configs: {details}",
                            client_label = client.display_name()
                        );
                    }
                }
                if let Some((higher_path, _)) = top.first() {
                    for (spec, draft) in &layers {
                        if spec.priority < highest && !selected.same_definition(draft, true) {
                            warnings.push(format!(
                                "{}: server '{name}' overridden by higher-precedence {}",
                                spec.path.display(),
                                higher_path.path.display()
                            ));
                        }
                    }
                }
                vec![((*selected).clone(), paths.clone())]
            }
        };
        for (selected, selected_paths) in selections {
            for warning in &selected.warnings {
                warnings.push(format!(
                    "{}: server '{name}': {warning}",
                    selected_paths
                        .first()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                ));
            }
            let path = selected_paths
                .first()
                .cloned()
                .unwrap_or_else(|| opts.cwd.clone());
            let mut config = selected.finish(&name, &path)?;
            if client == Client::Claude {
                let project = find_upward_file(&opts.cwd, ".mcp.json")
                    .map(|(_, root)| root)
                    .unwrap_or_else(|| opts.cwd.clone());
                let (disabled_names, mut settings_warnings) = claude_disabled_names(opts, &project);
                warnings.append(&mut settings_warnings);
                if disabled_names.contains(&name) {
                    config.disabled = true;
                    warnings.push(format!(
                        "{}: server '{name}' disabled by Claude settings",
                        path.display()
                    ));
                }
            }
            candidates.push(Candidate {
                name: name.clone(),
                config,
                client,
                paths: selected_paths,
            });
        }
    }
    Ok((candidates, warnings))
}

fn claude_disabled_names(opts: &LoadOptions, project: &Path) -> (BTreeSet<String>, Vec<String>) {
    let mut names = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut paths = Vec::new();
    if let Some(config_dir) = claude_config_dir(opts) {
        paths.push(config_dir.join("settings.json"));
    }
    paths.push(project.join(".claude").join("settings.json"));
    paths.push(project.join(".claude").join("settings.local.json"));
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let text = match read_source(&path) {
            Ok(text) => text,
            Err(error) => {
                warnings.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        let value = match parse_json_value(&text, &path) {
            Ok(value) => value,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        for key in ["disabledMcpServers", "disabledMcpjsonServers"] {
            if let Some(values) = object.get(key).and_then(Value::as_array) {
                for value in values {
                    if let Some(name) = value.as_str() {
                        names.insert(name.to_string());
                    }
                }
            }
        }
    }
    (names, warnings)
}
