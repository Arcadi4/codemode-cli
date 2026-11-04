use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::import::{self, Client, ImportOptions, ImportReport};

pub const REDACTED: &str = "<redacted>";

const FINGERPRINT_VERSION: &str = "codemode-resolved-v1";
const MAX_CONFIG_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Stdio,
    Http,
}

/// Normalized MCP server definition used by the global `mcpServers` file.
#[derive(Clone, PartialEq, Serialize)]
pub struct ServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub disabled: bool,
    #[serde(flatten, default)]
    pub extra: BTreeMap<String, Value>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl ServerConfig {
    pub fn transport_kind(&self) -> TransportKind {
        if self.url.is_some() && self.command.is_none() {
            TransportKind::Http
        } else {
            TransportKind::Stdio
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.command.is_some() && self.url.is_some() {
            bail!("server defines both 'command' and 'url'");
        }
        if self.disabled {
            return Ok(());
        }
        match (&self.command, &self.url) {
            (Some(command), None) => {
                if command.trim().is_empty() {
                    bail!("server 'command' is empty");
                }
                if self.bearer_token_env_var.is_some() {
                    bail!("'bearer_token_env_var' requires a remote HTTP server");
                }
            }
            (None, Some(url)) => {
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    bail!("unsupported MCP URL scheme; only http:// and https:// are supported");
                }
                if url.trim().is_empty() {
                    bail!("server 'url' is empty");
                }
                if self.env_file.is_some() {
                    bail!("'env_file' is only valid for stdio servers");
                }
            }
            (None, None) => bail!("server has neither 'command' nor 'url'"),
            (Some(_), Some(_)) => unreachable!(),
        }
        if !self.headers.is_empty() && self.url.is_none() {
            bail!("'headers' requires a remote HTTP server");
        }
        Ok(())
    }


    pub(crate) fn same_definition(&self, other: &Self, include_disabled: bool) -> bool {
        self.command == other.command
            && self.args == other.args
            && self.env == other.env
            && self.cwd == other.cwd
            && self.url == other.url
            && self.headers == other.headers
            && self.bearer_token_env_var == other.bearer_token_env_var
            && self.env_file == other.env_file
            && self.timeout_ms == other.timeout_ms
            && (!include_disabled || self.disabled == other.disabled)
    }

    pub(crate) fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.command != other.command {
            fields.push("command");
        }
        if self.args != other.args {
            fields.push("args");
        }
        if self.env != other.env {
            fields.push("env");
        }
        if self.cwd != other.cwd {
            fields.push("cwd");
        }
        if self.url != other.url {
            fields.push("url");
        }
        if self.headers != other.headers {
            fields.push("headers");
        }
        if self.bearer_token_env_var != other.bearer_token_env_var {
            fields.push("bearer_token_env_var");
        }
        if self.env_file != other.env_file {
            fields.push("env_file");
        }
        if self.timeout_ms != other.timeout_ms {
            fields.push("timeout_ms");
        }
        fields
    }

    fn resolve(&self, name: &str, opts: &ResolveOptions) -> Result<ResolvedServer> {
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => bail!("server '{name}' defines both command and url"),
            (None, None) => bail!("server '{name}' has neither command nor url"),
            (Some(_), None) => self.resolve_stdio(name, opts),
            (None, Some(_)) => self.resolve_http(name, opts),
        }
    }

    fn resolve_stdio(&self, name: &str, opts: &ResolveOptions) -> Result<ResolvedServer> {
        let command = self
            .command
            .as_deref()
            .ok_or_else(|| anyhow!("server '{name}' has no command"))?;
        let command = render(
            command,
            &opts.env,
            &opts.cwd,
            name,
            "command",
        )?;
        if command.trim().is_empty() {
            bail!("server '{name}': command is empty after environment substitution");
        }
        let effective_cwd = match self.cwd.as_deref() {
            Some(cwd) => {
                let cwd = render(cwd, &opts.env, &opts.cwd, name, "cwd")?;
                Some(resolve_runtime_path(&cwd, &opts.cwd, &opts.env))
            }
            None => None,
        };
        let command = if looks_like_path(&command) && !Path::new(&command).is_absolute() {
            let base = effective_cwd.as_deref().unwrap_or(&opts.cwd);
            base.join(expand_tilde(&command, &opts.env)).to_string_lossy().into_owned()
        } else {
            command
        };

        let mut env = BTreeMap::new();
        if let Some(env_file) = self.env_file.as_deref() {
            let env_path = render(env_file, &opts.env, &opts.cwd, name, "envFile")?;
            let env_path = resolve_runtime_path(&env_path, &opts.cwd, &opts.env);
            let file_env = read_env_file(&env_path)
                .with_context(|| format!("server '{name}': cannot read envFile {}", env_path.display()))?;
            env.extend(file_env);
        }
        for (key, value) in &self.env {
            env.insert(
                key.clone(),
                render(value, &opts.env, &opts.cwd, name, &format!("env.{key}"))?,
            );
        }
        let args = self
            .args
            .iter()
            .enumerate()
            .map(|(index, value)| render(value, &opts.env, &opts.cwd, name, &format!("args[{index}]")))
            .collect::<Result<Vec<_>>>()?;

        let transport = ResolvedTransport::Stdio {
            command,
            args,
            env,
            cwd: effective_cwd,
        };
        Ok(ResolvedServer::new(
            name.to_string(),
            transport,
            self.timeout_ms,
            opts,
        ))
    }

    fn resolve_http(&self, name: &str, opts: &ResolveOptions) -> Result<ResolvedServer> {
        let url = self
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("server '{name}' has no url"))?;
        let url = render(url, &opts.env, &opts.cwd, name, "url")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!(
                "server '{name}': URL must use http:// or https:// after environment substitution"
            );
        }
        let mut headers = BTreeMap::new();
        for (key, value) in &self.headers {
            headers.insert(
                key.clone(),
                render(value, &opts.env, &opts.cwd, name, &format!("headers.{key}"))?,
            );
        }

        let has_authorization = headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("authorization"));
        if let Some(env_var) = self.bearer_token_env_var.as_deref() {
            if has_authorization {
                bail!(
                    "server '{name}' sets both an Authorization header and bearer_token_env_var; \
                     configure only one"
                );
            }
            let token = opts.env.get(env_var).filter(|value| !value.is_empty()).ok_or_else(|| {
                anyhow!(
                    "server '{name}': environment variable '{env_var}' for bearer_token_env_var \
                     is not set"
                )
            })?;
            headers.insert("Authorization".to_string(), format!("Bearer {token}"));
        } else if !has_authorization {
            let stored = opts
                .include_stored_credentials
                .then(|| lookup_stored_token(name, &url, &opts.env))
                .flatten();
            if let Some(StoredToken::Valid { token, .. }) = stored {
                headers.insert("Authorization".to_string(), format!("Bearer {token}"));
            } else {
                let native = if opts.include_native_credentials {
                    crate::credentials::lookup(name, &url, &opts.env)?
                } else {
                    None
                };
                if let Some(token) = native {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    if token
                        .expires_at
                        .is_some_and(|expiry| expiry <= now.saturating_add(30))
                    {
                        bail!(
                            "server '{name}': stored {} OAuth token has expired; sign in with that client again",
                            token.source
                        );
                    }
                    headers.insert(
                        "Authorization".to_string(),
                        format!("Bearer {}", token.access_token),
                    );
                } else if let Some(StoredToken::Expired { client, login }) = stored {
                    bail!(
                        "server '{name}': stored {client} OAuth token has expired; run `{login}` or configure an Authorization header"
                    );
                }
            }
        }

        let transport = ResolvedTransport::Http { url, headers };
        Ok(ResolvedServer::new(
            name.to_string(),
            transport,
            self.timeout_ms,
            opts,
        ))
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("command", &self.command)
            .field("args", &format!("<{} args>", self.args.len()))
            .field("env", &format!("<{} vars>", self.env.len()))
            .field("cwd", &self.cwd)
            .field("url", &self.url.as_deref().map(redact_url))
            .field("headers", &format!("<{} headers>", self.headers.len()))
            .field("bearer_token_env_var", &self.bearer_token_env_var)
            .field("env_file", &self.env_file)
            .field("timeout_ms", &self.timeout_ms)
            .field("disabled", &self.disabled)
            .field("extra_keys", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawKind {
    Stdio,
    Http,
}

#[derive(Deserialize, Default)]
struct RawServerConfig {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Option<Vec<Value>>,
    #[serde(default)]
    env: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, Value>>,
    #[serde(default, rename = "bearer_token_env_var", alias = "bearerTokenEnvVar")]
    bearer_token_env_var: Option<String>,
    #[serde(default, rename = "env_file", alias = "envFile")]
    env_file: Option<String>,
    #[serde(default, rename = "timeout_ms", alias = "timeout")]
    timeout_ms: Option<u64>,
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default, rename = "headersHelper")]
    headers_helper: Option<Value>,
    #[serde(default)]
    http_headers_helper: Option<Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl RawServerConfig {
    fn into_server(self) -> std::result::Result<ServerConfig, String> {
        if self.headers_helper.is_some() || self.http_headers_helper.is_some() {
            return Err(
                "dynamic header helpers are unsupported; configure static headers in the cm global config"
                    .to_string(),
            );
        }
        let kind = match (self.kind.as_deref(), self.transport.as_deref()) {
            (Some(left), Some(right)) if !left.eq_ignore_ascii_case(right) => {
                return Err("'type' and 'transport' disagree".to_string());
            }
            (Some(value), _) | (None, Some(value)) => Some(normalize_raw_kind(value)?),
            (None, None) => None,
        };
        let command = self.command.filter(|value| !value.is_empty());
        let url = self.url.filter(|value| !value.is_empty());
        match (kind, command.is_some(), url.is_some()) {
            (Some(RawKind::Stdio), false, _) => return Err("stdio server requires 'command'".into()),
            (Some(RawKind::Http), _, false) => return Err("HTTP server requires 'url'".into()),
            _ => {}
        }
        if command.is_some() && url.is_some() {
            return Err("server defines both 'command' and 'url'".into());
        }
        let env = value_map_to_strings(self.env, "env")?;
        let headers = value_map_to_strings(self.headers, "headers")?;
        let args = self
            .args
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, value)| value_to_string(value, &format!("args[{index}]")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let disabled = self
            .disabled
            .or_else(|| self.enabled.map(|enabled| !enabled))
            .unwrap_or(false);
        let config = ServerConfig {
            command,
            args,
            env,
            cwd: self.cwd.filter(|value| !value.is_empty()),
            url,
            headers,
            bearer_token_env_var: self
                .bearer_token_env_var
                .filter(|value| !value.is_empty()),
            env_file: self.env_file.filter(|value| !value.is_empty()),
            timeout_ms: self.timeout_ms,
            disabled,
            extra: self.extra,
        };
        config.validate().map_err(|error| error.to_string())?;
        Ok(config)
    }
}

impl<'de> Deserialize<'de> for ServerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawServerConfig::deserialize(deserializer)?;
        raw.into_server().map_err(serde::de::Error::custom)
    }
}

fn normalize_raw_kind(value: &str) -> std::result::Result<RawKind, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" | "local" => Ok(RawKind::Stdio),
        "http" | "streamable-http" | "streamable_http" | "streamablehttp" | "remote" => {
            Ok(RawKind::Http)
        }
        "sse" => Err(
            "deprecated SSE transport is unsupported; use streamable HTTP (url with type http)"
                .to_string(),
        ),
        "ws" | "websocket" => {
            Err("WebSocket transport is unsupported; use streamable HTTP".to_string())
        }
        "sdk" => Err("SDK-only server entries cannot be imported".to_string()),
        other => Err(format!("unsupported transport type '{other}'")),
    }
}

fn value_map_to_strings(
    map: Option<BTreeMap<String, Value>>,
    field: &str,
) -> std::result::Result<BTreeMap<String, String>, String> {
    let mut result = BTreeMap::new();
    for (key, value) in map.unwrap_or_default() {
        if value.is_null() {
            continue;
        }
        result.insert(key.clone(), value_to_string(value, &format!("{field}.{key}"))?);
    }
    Ok(result)
}

fn value_to_string(value: Value, field: &str) -> std::result::Result<String, String> {
    match value {
        Value::String(value) => Ok(value),
        Value::Number(value) => Ok(value.to_string()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Null => Ok(String::new()),
        Value::Array(_) | Value::Object(_) => {
            Err(format!("'{field}' must be a string, number, or boolean"))
        }
    }
}

/// Where a server definition was loaded from.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigOrigin {
    pub source: OriginSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<Client>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OriginSource {
    Global,
    Client,
}

impl ConfigOrigin {
    fn global(path: &Path) -> Self {
        Self {
            source: OriginSource::Global,
            client: None,
            path: Some(path.to_path_buf()),
        }
    }

    fn client(client: Client, path: &Path) -> Self {
        Self {
            source: OriginSource::Client,
            client: Some(client),
            path: Some(path.to_path_buf()),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ServerSummary {
    pub name: String,
    pub transport: TransportKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub disabled: bool,
    pub origins: Vec<ConfigOrigin>,
}

/// Options controlling global config location, client discovery, and environment.
#[derive(Clone)]
pub struct LoadOptions {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// Explicit global config path. Overrides `CM_CONFIG` and the XDG default.
    pub global_path: Option<PathBuf>,
    /// `None` discovers all clients, `Some(vec![])` disables discovery.
    pub clients: Option<Vec<Client>>,
    pub env: BTreeMap<String, String>,
}

impl LoadOptions {
    pub fn from_env() -> Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine current directory")?;
        let env = process_env();
        let home = home_from_env(&env);
        let global_path = env
            .get("CM_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let clients = match env.get("CM_CLIENTS").map(String::as_str) {
            None | Some("") | Some("all") => None,
            Some(value) if value.eq_ignore_ascii_case("none") => Some(Vec::new()),
            Some(value) => {
                let mut clients = Vec::new();
                for name in value.split(',').map(str::trim).filter(|name| !name.is_empty()) {
                    let client = Client::from_name(name).ok_or_else(|| {
                        anyhow!(
                            "invalid CM_CLIENTS entry '{name}'; expected a comma-separated list of: {}",
                            Client::all()
                                .iter()
                                .map(|client| client.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?;
                    if !clients.contains(&client) {
                        clients.push(client);
                    }
                }
                Some(clients)
            }
        };
        Ok(Self {
            cwd,
            home,
            global_path,
            clients,
            env,
        })
    }

}

#[derive(Clone)]
pub struct ResolveOptions {
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub include_stored_credentials: bool,
}

impl ResolveOptions {
    pub fn from_env() -> Self {
    pub include_native_credentials: bool,
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            env: process_env(),
            cwd,
            include_stored_credentials: true,
        }
    }
}

            include_native_credentials: false,
impl Default for ResolveOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Clone)]
pub enum ResolvedTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

impl fmt::Debug for ResolvedTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", &format!("<{} args>", args.len()))
                .field("env", &format!("<{} vars>", env.len()))
                .field("cwd", cwd)
                .finish(),
            Self::Http { url, headers } => f
                .debug_struct("Http")
                .field("url", &redact_url(url))
                .field("headers", &format!("<{} headers>", headers.len()))
                .finish(),
        }
    }
}

/// Concrete, credential-bearing server definition ready for an MCP client.
#[derive(Clone)]
pub struct ResolvedServer {
    pub name: String,
    pub transport: ResolvedTransport,
    pub timeout_ms: Option<u64>,
    fingerprint: String,
}

impl ResolvedServer {
    fn new(
        name: String,
        transport: ResolvedTransport,
        timeout_ms: Option<u64>,
        opts: &ResolveOptions,
    ) -> Self {
        let fingerprint = fingerprint_resolved(&name, &transport, timeout_ms, opts);
        Self {
            name,
            transport,
            timeout_ms,
            fingerprint,
        }
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl fmt::Debug for ResolvedServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedServer")
            .field("name", &self.name)
            .field("transport", &self.transport)
            .field("timeout_ms", &self.timeout_ms)
            .field("fingerprint", &self.fingerprint)
            .finish()
    }
}

/// Fully discovered configuration for this process.
#[derive(Clone)]
pub struct Config {
    pub servers: BTreeMap<String, ServerConfig>,
    pub disabled: BTreeSet<String>,
    pub warnings: Vec<String>,
    pub global_path: PathBuf,
    all_servers: BTreeMap<String, ServerConfig>,
    origins: BTreeMap<String, Vec<ConfigOrigin>>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let opts = LoadOptions::from_env()?;
        Self::load_from(&opts)
    }

    pub fn load_from(opts: &LoadOptions) -> Result<Self> {
        let global_path = Self::global_path_from(opts)?;
        let mut global = GlobalFile::read(&global_path)?;
        let base = global_path.parent().unwrap_or_else(|| Path::new("."));
        let home = opts
            .home
            .clone()
            .or_else(|| home_from_env(&opts.env));
        for server in global.servers.values_mut() {
            normalize_server_paths(server, base, home.as_deref())?;
        }

        let mut all_servers = BTreeMap::new();
        let mut servers = BTreeMap::new();
        let mut disabled = BTreeSet::new();
        let mut origins = BTreeMap::new();
        for (name, server) in global.servers {
            if server.disabled {
                disabled.insert(name.clone());
            } else {
                servers.insert(name.clone(), server.clone());
            }
            origins.insert(name.clone(), vec![ConfigOrigin::global(&global_path)]);
            all_servers.insert(name, server);
        }

        let clients = opts
            .clients
            .clone()
            .unwrap_or_else(|| Client::all().to_vec());
        let mut warnings = Vec::new();
        if !clients.is_empty() {
            let discovery = import::discover(opts, &clients)?;
            warnings.extend(discovery.warnings);
            let mut by_name: BTreeMap<String, Vec<import::Candidate>> = BTreeMap::new();
            for candidate in discovery.candidates {
                by_name.entry(candidate.name.clone()).or_default().push(candidate);
            }
            for (name, candidates) in by_name {
                if all_servers.contains_key(&name) {
                    continue;
                }
                let enabled: Vec<&import::Candidate> = candidates
                    .iter()
                    .filter(|candidate| !candidate.config.disabled)
                    .collect();
                if enabled.is_empty() {
                    let chosen = candidates.first().expect("nonempty candidate group");
                    disabled.insert(name.clone());
                    origins.insert(
                        name.clone(),
                        chosen
                            .paths
                            .iter()
                            .map(|path| ConfigOrigin::client(chosen.client, path))
                            .collect(),
                    );
                    all_servers.insert(name, chosen.config.clone());
                    continue;
                }
                let first = enabled[0];
                for candidate in enabled.iter().skip(1) {
                    if !first.config.same_definition(&candidate.config, false) {
                        let mut details = Vec::new();
                        for item in &enabled {
                            let paths = item
                                .paths
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join(", ");
                            details.push(format!(
                                "  - {}: {}",
                                item.client.display_name(),
                                paths
                            ));
                        }
                        bail!(
                            "conflicting definitions for MCP server '{name}' from client configs:\n{}\n\
                             Different fields: {}.\n\
                             Define '{name}' in {} to choose one definition, or remove it from one source.",
                            details.join("\n"),
                            first.config.differing_fields(&candidate.config).join(", "),
                            global_path.display()
                        );
                    }
                }
                origins.insert(
                    name.clone(),
                    enabled
                        .iter()
                        .flat_map(|candidate| {
                            candidate
                                .paths
                                .iter()
                                .map(move |path| ConfigOrigin::client(candidate.client, path))
                        })
                        .collect(),
                );
                servers.insert(name.clone(), first.config.clone());
                all_servers.insert(name, first.config.clone());
            }
        }

        Ok(Self {
            servers,
            disabled,
            warnings,
            global_path,
            all_servers,
            origins,
        })
    }


    pub fn global_path_from(opts: &LoadOptions) -> Result<PathBuf> {
        let path = if let Some(path) = &opts.global_path {
            path.clone()
        } else if let Some(path) = opts.env.get("CM_CONFIG").filter(|value| !value.is_empty()) {
            PathBuf::from(path)
        } else {
            let config_dir = xdg_config_dir(opts)?;
            config_dir.join("codemode").join("mcp.json")
        };
        Ok(if path.is_absolute() {
            path
        } else {
            opts.cwd.join(path)
        })
    }

    pub fn get(&self, name: &str) -> Option<&ServerConfig> {
        self.servers.get(name)
    }

    pub fn origins(&self, name: &str) -> &[ConfigOrigin] {
        self.origins
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn summaries(&self) -> Vec<ServerSummary> {
        self.all_servers
            .iter()
            .map(|(name, server)| ServerSummary {
                name: name.clone(),
                transport: server.transport_kind(),
                command: server.command.clone(),
                url: server.url.as_deref().map(safe_endpoint),
                disabled: server.disabled,
                origins: self.origins(name).to_vec(),
            })
            .collect()
    }

    pub fn resolve(&self, name: &str) -> Result<ResolvedServer> {
        self.resolve_with(name, &ResolveOptions::from_env())
    }

    pub fn resolve_with(&self, name: &str, opts: &ResolveOptions) -> Result<ResolvedServer> {
        self.servers
            .get(name)
            .ok_or_else(|| anyhow!("unknown MCP server '{name}'"))?
            .resolve(name, opts)
            .with_context(|| format!("resolving MCP server '{name}'"))
    }

    pub fn import(clients: &[Client], opts: &ImportOptions) -> Result<ImportReport> {
        let load = LoadOptions::from_env()?;
        Self::import_with(&load, clients, opts)
    }

    pub fn import_with(
        load: &LoadOptions,
        clients: &[Client],
        opts: &ImportOptions,
    ) -> Result<ImportReport> {
        if clients.is_empty() {
            bail!("no client specified for import");
        }
        if opts.path.is_some() && clients.len() != 1 {
            bail!("--path can only be used with a single client");
        }
        let discovery = if let Some(path) = opts.path.as_deref() {
            import::discover_override(load, clients[0], path)?
        } else {
            import::discover(load, clients)?
        };
        let config_path = Self::global_path_from(load)?;

        let mut report = ImportReport {
            config_path: config_path.clone(),
            sources: discovery.sources.clone(),
            added: Vec::new(),
            updated: Vec::new(),
            unchanged: Vec::new(),
            skipped: Vec::new(),
            warnings: discovery.warnings.clone(),
        };

        if discovery.sources.is_empty() {
            if clients.len() == 1 {
                let checked = if discovery.expected.is_empty() {
                    "no known configuration paths (home directory is not set)".to_string()
                } else {
                    discovery
                        .expected
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                bail!(
                    "no {} configuration found (checked: {checked})",
                    clients[0].display_name()
                );
            }
            return Ok(report);
        }

        let _lock = lock_config(&config_path)?;
        let mut global = GlobalFile::read(&config_path)?;
        let mut by_name: BTreeMap<String, Vec<import::Candidate>> = BTreeMap::new();
        for candidate in discovery.candidates {
            by_name.entry(candidate.name.clone()).or_default().push(candidate);
        }

        for (name, candidates) in by_name {
            let enabled: Vec<&import::Candidate> = candidates
                .iter()
                .filter(|candidate| !candidate.config.disabled)
                .collect();
            if enabled.is_empty() {
                report.skipped.push(import::ImportSkip {
                    name,
                    reason: "disabled in source".to_string(),
                });
                continue;
            }
            let chosen = unique_candidate(&enabled)?;
            if let Some(existing) = global.servers.get(&name).cloned() {
                if chosen.config.same_definition(&existing, true) {
                    report.unchanged.push(name);
                    continue;
                }
                if !opts.force {
                    bail!(
                        "MCP server '{name}' already exists in {} with a different definition{}; \
                         use --force to overwrite it",
                        config_path.display(),
                        conflict_detail(&existing, &chosen.config)
                    );
                }
                global.servers.insert(name.clone(), chosen.config.clone());
                report.updated.push(name);
                continue;
            }
            global.servers.insert(name.clone(), chosen.config.clone());
            report.added.push(name);
        }

        if !report.added.is_empty() || !report.updated.is_empty() {
            write_global(&config_path, &global)?;
        }

        report.added.sort();
        report.updated.sort();
        report.unchanged.sort();
        report.skipped.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(report)
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("servers", &self.servers.keys().collect::<Vec<_>>())
            .field("disabled", &self.disabled)
            .field("warnings", &self.warnings)
            .field("global_path", &self.global_path)
            .finish()
    }
}

fn unique_candidate<'a>(candidates: &[&'a import::Candidate]) -> Result<&'a import::Candidate> {
    let first = candidates
        .first()
        .copied()
        .ok_or_else(|| anyhow!("no enabled candidate"))?;
    for candidate in candidates.iter().skip(1) {
        if !first.config.same_definition(&candidate.config, false) {
            let details = candidates
                .iter()
                .map(|item| {
                    format!(
                        "{} ({})",
                        item.client.display_name(),
                        item.paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "conflicting imported definitions for MCP server '{}': {details}. \
                 Define it in the cm global config to choose one.",
                first.name
            );
        }
    }
    Ok(first)
}

fn conflict_detail(existing: &ServerConfig, incoming: &ServerConfig) -> String {
    let mut fields = existing.differing_fields(incoming);
    if existing.disabled != incoming.disabled {
        fields.push("disabled");
    }
    if fields.is_empty() {
        String::new()
    } else {
        format!(" (different fields: {})", fields.join(", "))
    }
}

struct GlobalFile {
    servers: BTreeMap<String, ServerConfig>,
    extra: BTreeMap<String, Value>,
}

impl GlobalFile {
    fn read(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                servers: BTreeMap::new(),
                extra: BTreeMap::new(),
            });
        }
        let text = read_text(path)?;
        let value = parse_json_value(&text, path)?;
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("{}: top-level value must be an object", path.display()))?;
        let server_values = match object.get("mcpServers") {
            Some(Value::Object(servers)) => servers,
            Some(_) => bail!("{}: 'mcpServers' must be an object", path.display()),
            None if object.is_empty() => return Ok(Self {
                servers: BTreeMap::new(),
                extra: BTreeMap::new(),
            }),
            None if object.keys().all(|key| key.starts_with('$')) => {
                return Ok(Self {
                    servers: BTreeMap::new(),
                    extra: object
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                });
            }
            None => bail!(
                "{}: missing top-level 'mcpServers' object (FastMCP mcp.json shape)",
                path.display()
            ),
        };
        let mut servers = BTreeMap::new();
        for (name, value) in server_values {
            if name.trim().is_empty() {
                bail!("{}: server name cannot be empty", path.display());
            }
            let raw: RawServerConfig = serde_json::from_value(value.clone()).map_err(|_| {
                anyhow!(
                    "{}: invalid definition shape for server '{name}'",
                    path.display()
                )
            })?;
            let server = raw.into_server().map_err(|error| {
                anyhow!(
                    "{}: invalid definition for server '{name}': {error}",
                    path.display()
                )
            })?;
            server.validate().with_context(|| {
                format!("{}: invalid definition for server '{name}'", path.display())
            })?;
            servers.insert(name.clone(), server);
        }
        let extra = object
            .iter()
            .filter(|(key, _)| key.as_str() != "mcpServers")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self { servers, extra })
    }
}

fn write_global(path: &Path, global: &GlobalFile) -> Result<()> {
    let mut root = serde_json::Map::new();
    for (key, value) in &global.extra {
        root.insert(key.clone(), value.clone());
    }
    root.insert(
        "mcpServers".to_string(),
        serde_json::to_value(&global.servers).context("serializing global MCP config")?,
    );
    let mut text = serde_json::to_string_pretty(&Value::Object(root))
        .context("serializing global MCP config")?;
    text.push('\n');
    write_private_atomic(path, text.as_bytes())
}

fn read_text(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if metadata.len() > MAX_CONFIG_BYTES {
        bail!(
            "{}: configuration is larger than {} MiB",
            path.display(),
            MAX_CONFIG_BYTES / (1024 * 1024)
        );
    }
    let mut file = fs::File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(text)
}

fn parse_json_value(text: &str, path: &Path) -> Result<Value> {
    json5::from_str(text)
        .map_err(|_| anyhow!("{}: invalid JSON/JSONC", path.display()))
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_parent(parent)?;
    let mut temp = tempfile::Builder::new()
        .prefix(".mcp.json.")
        .tempfile_in(parent)
        .with_context(|| format!("cannot create temporary file in {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| "cannot set config permissions")?;
    }
    temp.write_all(contents)
        .with_context(|| format!("cannot write {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("cannot sync {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| anyhow!("cannot replace {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot set permissions on {}", path.display()))?;
    }
    if let Ok(directory) = fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn ensure_private_parent(parent: &Path) -> Result<()> {
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create config directory {}", parent.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if parent.file_name().and_then(|name| name.to_str()) == Some("codemode") {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("cannot secure config directory {}", parent.display()))?;
        }
    }
    Ok(())
}

struct ConfigLock {
    _file: fs::File,
}

fn lock_config(path: &Path) -> Result<ConfigLock> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_private_parent(parent)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mcp.json".to_string());
    let lock_path = parent.join(format!(".{file_name}.lock"));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        .with_context(|| format!("cannot open lock file {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot secure lock file {}", lock_path.display()))?;
    }
    file.lock_exclusive()
        .with_context(|| format!("cannot lock {}", lock_path.display()))?;
    Ok(ConfigLock { _file: file })
}

fn xdg_config_dir(opts: &LoadOptions) -> Result<PathBuf> {
    if let Some(value) = opts.env.get("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            bail!("XDG_CONFIG_HOME must be an absolute path");
        }
        return Ok(path);
    }
    Ok(home_dir(opts)?.join(".config"))
}

fn home_dir(opts: &LoadOptions) -> Result<PathBuf> {
    if let Some(home) = &opts.home {
        return Ok(home.clone());
    }
    home_from_env(&opts.env).ok_or_else(|| {
        anyhow!("home directory is not set; set HOME or pass an explicit config path")
    })
}

fn home_from_env(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env.get("HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if let Some(profile) = env.get("USERPROFILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(profile));
    }
    match (
        env.get("HOMEDRIVE").filter(|value| !value.is_empty()),
        env.get("HOMEPATH").filter(|value| !value.is_empty()),
    ) {
        (Some(drive), Some(path)) => Some(PathBuf::from(format!("{drive}{path}"))),
        _ => None,
    }
}

fn process_env() -> BTreeMap<String, String> {
    std::env::vars_os()
        .filter_map(|(key, value)| {
            Some((key.into_string().ok()?, value.into_string().ok()?))
        })
        .collect()
}

fn os_bytes(value: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().as_bytes().to_vec()
    }
}

fn expand_tilde(value: &str, env: &BTreeMap<String, String>) -> String {
    if value == "~" {
        return home_from_env(env)
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_else(|| value.to_string());
    }
    if let Some(rest) = value.strip_prefix("~/").or_else(|| value.strip_prefix("~\\")) {
        if let Some(home) = home_from_env(env) {
            return home.join(rest).to_string_lossy().into_owned();
        }
    }
    value.to_string()
}

fn resolve_runtime_path(value: &str, cwd: &Path, env: &BTreeMap<String, String>) -> PathBuf {
    let expanded = expand_tilde(value, env);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() || is_windows_absolute(&expanded) {
        path
    } else {
        cwd.join(path)
    }
}

pub(crate) fn looks_like_path(value: &str) -> bool {
    value.starts_with('.')
        || value.starts_with('~')
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains('/')
        || value.contains('\\')
        || is_windows_absolute(value)
}

pub(crate) fn is_windows_absolute(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || value.starts_with("\\\\")
}

pub(crate) fn normalize_server_paths(
    server: &mut ServerConfig,
    base: &Path,
    home: Option<&Path>,
) -> Result<()> {
    let env = BTreeMap::new();
    let home_env = home
        .map(|home| {
            BTreeMap::from([(
                "HOME".to_string(),
                home.to_string_lossy().into_owned(),
            )])
        })
        .unwrap_or(env);
    if let Some(command) = server.command.as_mut() {
        if looks_like_path(command) && !command.contains("${") {
            *command = resolve_source_path(command, base, &home_env).to_string_lossy().into_owned();
        }
    }
    if let Some(cwd) = server.cwd.as_mut() {
        if !cwd.contains("${") {
            *cwd = resolve_source_path(cwd, base, &home_env).to_string_lossy().into_owned();
        }
    }
    if let Some(env_file) = server.env_file.as_mut() {
        if !env_file.contains("${") {
            *env_file = resolve_source_path(env_file, base, &home_env).to_string_lossy().into_owned();
        }
    }
    Ok(())
}

pub(crate) fn resolve_source_path(
    value: &str,
    base: &Path,
    env: &BTreeMap<String, String>,
) -> PathBuf {
    let expanded = expand_tilde(value, env);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() || is_windows_absolute(&expanded) {
        path
    } else {
        base.join(path)
    }
}

fn render(
    value: &str,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    server: &str,
    field: &str,
) -> Result<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('$') {
        output.push_str(&rest[..index]);
        rest = &rest[index..];
        if rest.starts_with("$${") {
            output.push_str("${");
            rest = &rest[3..];
            continue;
        }
        if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                output.push('$');
                rest = &rest[1..];
                continue;
            };
            let inner = &after[..end];
            let replacement = render_reference(inner, env, cwd, server, field)?;
            output.push_str(&replacement);
            rest = &after[end + 1..];
            continue;
        }
        output.push('$');
        rest = &rest[1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn render_reference(
    inner: &str,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    server: &str,
    field: &str,
) -> Result<String> {
    if let Some(name) = inner.strip_prefix("input:") {
        bail!(
            "server '{server}': interactive input variable '{name}' in {field} is unsupported; \
             export an environment variable and reference it instead"
        );
    }
    if inner.starts_with("file:") {
        bail!(
            "server '{server}': file substitution in {field} is unsupported at runtime; \
             use an environment variable or import the client config"
        );
    }
    if inner == "workspaceFolder" {
        return Ok(cwd.to_string_lossy().into_owned());
    }
    if inner == "workspaceFolderBasename" {
        return Ok(cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default());
    }
    if inner == "userHome" {
        return home_from_env(env)
            .map(|home| home.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("server '{server}': home directory is not set for {field}"));
    }
    if inner == "pathSeparator" || inner == "/" {
        return Ok(std::path::MAIN_SEPARATOR.to_string());
    }
    if inner.starts_with("workspaceFolder:") {
        bail!(
            "server '{server}': multi-root workspace variable in {field} is unsupported; \
             use an absolute path"
        );
    }
    let inner = inner.strip_prefix("env:").unwrap_or(inner);
    let (name, default) = match inner.split_once(":-") {
        Some((name, default)) => (name, Some(default)),
        None => (inner, None),
    };
    if name.is_empty() {
        bail!("server '{server}': empty environment variable reference in {field}");
    }
    if let Some(value) = env.get(name) {
        return Ok(value.clone());
    }
    if let Some(default) = default {
        return Ok(default.to_string());
    }
    bail!(
        "server '{server}': environment variable '{name}' is not set (referenced in {field}); \
         set it or add a :-default"
    )
}

fn read_env_file(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = read_text(path)?;
    let mut result = BTreeMap::new();
    for raw_line in text.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("export ") {
            line = rest.trim_start();
        }
        let Some((key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_dotenv_value(raw_value.trim());
        result.insert(key.to_string(), value);
    }
    Ok(result)
}

fn parse_dotenv_value(value: &str) -> String {
    if let Some(inner) = value.strip_prefix('"') {
        if let Some(inner) = inner.strip_suffix('"') {
            let mut output = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' {
                    match chars.next() {
                        Some('n') => output.push('\n'),
                        Some('r') => output.push('\r'),
                        Some('t') => output.push('\t'),
                        Some('"') => output.push('"'),
                        Some('\\') => output.push('\\'),
                        Some(other) => {
                            output.push('\\');
                            output.push(other);
                        }
                        None => output.push('\\'),
                    }
                } else {
                    output.push(ch);
                }
            }
            return output;
        }
    }
    if let Some(inner) = value.strip_prefix('\'') {
        if let Some(inner) = inner.strip_suffix('\'') {
            return inner.to_string();
        }
    }
    let mut output = value.to_string();
    if let Some(index) = output.find(" #") {
        output.truncate(index);
    }
    output.trim().to_string()
}

fn fingerprint_resolved(
    name: &str,
    transport: &ResolvedTransport,
    timeout_ms: Option<u64>,
    opts: &ResolveOptions,
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, FINGERPRINT_VERSION.as_bytes());
    hash_field(&mut hasher, name.as_bytes());
    match transport {
        ResolvedTransport::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            hash_field(&mut hasher, b"stdio");
            hash_field(&mut hasher, command.as_bytes());
            hash_field(&mut hasher, &(args.len() as u64).to_be_bytes());
            for arg in args {
                hash_field(&mut hasher, arg.as_bytes());
            }
            hash_field(&mut hasher, &(env.len() as u64).to_be_bytes());
            for (key, value) in env {
                hash_field(&mut hasher, key.as_bytes());
                hash_field(&mut hasher, value.as_bytes());
            }
            match cwd {
                Some(cwd) => {
                    hash_field(&mut hasher, b"cwd");
                    hash_field(&mut hasher, cwd.to_string_lossy().as_bytes());
                }
                None => hash_field(&mut hasher, b"no-cwd"),
            }
        }
        ResolvedTransport::Http { url, headers } => {
            hash_field(&mut hasher, b"http");
            hash_field(&mut hasher, url.as_bytes());
            hash_field(&mut hasher, &(headers.len() as u64).to_be_bytes());
            for (key, value) in headers {
                hash_field(&mut hasher, key.to_ascii_lowercase().as_bytes());
                hash_field(&mut hasher, value.as_bytes());
            }
        }
    }
    hash_field(
        &mut hasher,
        &timeout_ms.unwrap_or_default().to_be_bytes(),
    );
    hash_field(&mut hasher, b"env");
    hash_field(&mut hasher, &(opts.env.len() as u64).to_be_bytes());
    for (key, value) in &opts.env {
        hash_field(&mut hasher, key.as_bytes());
        hash_field(&mut hasher, value.as_bytes());
    }
    hash_raw_process_env(&mut hasher);
    let effective_cwd = match transport {
        ResolvedTransport::Stdio { cwd: Some(cwd), .. } => cwd.clone(),
        _ => opts.cwd.clone(),
    };
    hash_field(&mut hasher, b"cwd-effective");
    hash_field(
        &mut hasher,
        effective_cwd.to_string_lossy().as_bytes(),
    );
    hex::encode(hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn hash_raw_process_env(hasher: &mut Sha256) {
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = std::env::vars_os()
        .map(|(key, value)| (os_bytes(&key), os_bytes(&value)))
        .collect();
    entries.sort();
    hash_field(hasher, b"raw-process-env");
    hash_field(hasher, &(entries.len() as u64).to_be_bytes());
    for (key, value) in entries {
        hash_field(hasher, &key);
        hash_field(hasher, &value);
    }
}

fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(|c: char| matches!(c, '-' | '_' | '.'), "");
    [
        "token",
        "secret",
        "password",
        "passwd",
        "authorization",
        "apikey",
        "credential",
        "cookie",
        "privatekey",
        "accesskey",
        "bearer",
    ]
    .iter()
    .any(|needle| key.contains(needle))
}


fn safe_endpoint(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return REDACTED.to_string();
    };
    let authority = rest
        .split(|c: char| matches!(c, '/' | '?' | '#'))
        .next()
        .unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    format!("{scheme}://{authority}")
}

fn redact_url(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_string();
    };
    let (authority_and_path, fragment) = match rest.split_once('#') {
        Some((left, right)) => (left, Some(right)),
        None => (rest, None),
    };
    let (authority, path_and_query) = match authority_and_path.find(|c: char| c == '/' || c == '?') {
        Some(index) => (
            &authority_and_path[..index],
            &authority_and_path[index..],
        ),
        None => (authority_and_path, ""),
    };
    let authority = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority);
    let mut output = format!("{scheme}://{authority}");
    if let Some(query_index) = path_and_query.find('?') {
        output.push_str(&path_and_query[..query_index]);
        output.push('?');
        let query = &path_and_query[query_index + 1..];
        let mut first = true;
        for part in query.split('&') {
            if !first {
                output.push('&');
            }
            first = false;
            match part.split_once('=') {
                Some((key, _)) if secret_key(key) => {
                    output.push_str(key);
                    output.push('=');
                    output.push_str(REDACTED);
                }
                _ => output.push_str(part),
            }
        }
    } else {
        output.push_str(path_and_query);
    }
    if fragment.is_some() || fragment.is_some_and(|value| !value.is_empty()) {
        output.push('#');
        output.push_str(REDACTED);
    }
    output
}

enum StoredToken {
    Valid { token: String },
    Expired { client: &'static str, login: String },
}

fn lookup_stored_token(
    server_name: &str,
    url: &str,
    env: &BTreeMap<String, String>,
) -> Option<StoredToken> {
    let mut expired = None;
    for store in CredentialStore::all(env) {
        match store.lookup(server_name, url) {
            Some(valid @ StoredToken::Valid { .. }) => return Some(valid),
            Some(item @ StoredToken::Expired { .. }) => {
                if expired.is_none() {
                    expired = Some(item);
                }
            }
            None => {}
        }
    }
    expired
}

struct CredentialStore {
    kind: CredentialStoreKind,
    path: PathBuf,
}

#[derive(Clone, Copy)]
enum CredentialStoreKind {
    Claude,
    Codex,
    OpenCode,
}

impl CredentialStore {
    fn all(env: &BTreeMap<String, String>) -> Vec<Self> {
        let mut stores = Vec::new();
        if let Some(path) = claude_credentials_path(env) {
            stores.push(Self {
                kind: CredentialStoreKind::Claude,
                path,
            });
        }
        if let Some(path) = codex_credentials_path(env) {
            stores.push(Self {
                kind: CredentialStoreKind::Codex,
                path,
            });
        }
        if let Some(path) = opencode_credentials_path(env) {
            stores.push(Self {
                kind: CredentialStoreKind::OpenCode,
                path,
            });
        }
        stores
    }

    fn lookup(&self, server_name: &str, url: &str) -> Option<StoredToken> {
        let text = read_text(&self.path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        match self.kind {
            CredentialStoreKind::Claude => {
                let entries = value.get("mcpOAuth")?.as_object()?;
                lookup_entries(self.kind, entries.iter(), server_name, url)
            }
            CredentialStoreKind::Codex | CredentialStoreKind::OpenCode => {
                let entries = value.as_object()?;
                lookup_entries(self.kind, entries.iter(), server_name, url)
            }
        }
    }
}

fn lookup_entries<'a>(
    kind: CredentialStoreKind,
    entries: impl Iterator<Item = (&'a String, &'a Value)>,
    server_name: &str,
    url: &str,
) -> Option<StoredToken> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let mut expired = None;
    for (_key, value) in entries {
        let Some(entry) = value.as_object() else {
            continue;
        };
        let Some(entry_url) = string_field(entry, &["serverUrl", "server_url", "url"]) else {
            continue;
        };
        if !urls_match(entry_url, url) {
            continue;
        }
        let token = match kind {
            CredentialStoreKind::OpenCode => entry
                .get("tokens")
                .and_then(Value::as_object)
                .and_then(|tokens| string_field(tokens, &["accessToken", "access_token"])),
            CredentialStoreKind::Claude => string_field(entry, &["accessToken", "access_token"]),
            CredentialStoreKind::Codex => entry
                .get("token_response")
                .and_then(Value::as_object)
                .and_then(|tokens| string_field(tokens, &["access_token"]))
                .or_else(|| string_field(entry, &["accessToken", "access_token"])),
        };
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            continue;
        };
        let expires = number_field(entry, &["expiresAt", "expires_at"])
            .or_else(|| {
                entry
                    .get("tokens")
                    .and_then(Value::as_object)
                    .and_then(|tokens| number_field(tokens, &["expiresAt", "expires_at"]))
            })
            .map(normalize_timestamp);
        if let Some(expires) = expires
            && expires <= now.saturating_add(30)
        {
            let (client, login) = match kind {
                CredentialStoreKind::Claude => {
                    ("Claude Code", format!("claude mcp login {server_name}"))
                }
                CredentialStoreKind::Codex => ("Codex", format!("codex mcp login {server_name}")),
                CredentialStoreKind::OpenCode => {
            continue;
                    ("OpenCode", format!("opencode mcp auth {server_name}"))
                }
            };
            if expired.is_none() {
                expired = Some(StoredToken::Expired { client, login });
            }
        }
        return Some(StoredToken::Valid {
            token: token.to_string(),
        });
    }
    expired
}

fn string_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    names: &[&str],
) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn number_field(object: &serde_json::Map<String, Value>, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_u64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn normalize_timestamp(value: u64) -> u64 {
    if value > 10_000_000_000 {
        value / 1000
    } else {
        value
    }
}

fn urls_match(left: &str, right: &str) -> bool {
    match (reqwest::Url::parse(left), reqwest::Url::parse(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn claude_credentials_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = env.get("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join(".credentials.json"));
    }
    home_from_env(env).map(|home| home.join(".claude").join(".credentials.json"))
}

fn codex_credentials_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = env.get("CODEX_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join(".credentials.json"));
    }
    home_from_env(env).map(|home| home.join(".codex").join(".credentials.json"))
}

fn opencode_credentials_path(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(dir) = env.get("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir).join("opencode").join("mcp-auth.json"));
    }
    home_from_env(env).map(|home| {
        home.join(".local")
            .join("share")
            .join("opencode")
            .join("mcp-auth.json")
    })
}
