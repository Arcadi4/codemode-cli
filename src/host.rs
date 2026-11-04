use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    config::Config,
    mcp::{Client, RpcError},
    schema,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CatalogKind {
    Tools,
    Prompts,
    Resources,
    Templates,
}

impl CatalogKind {
    fn method(self) -> &'static str {
        match self {
            Self::Tools => "tools/list",
            Self::Prompts => "prompts/list",
            Self::Resources => "resources/list",
            Self::Templates => "resources/templates/list",
        }
    }
    fn field(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Prompts => "prompts",
            Self::Resources => "resources",
            Self::Templates => "resourceTemplates",
        }
    }
    fn capability(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Prompts => "prompts",
            Self::Resources | Self::Templates => "resources",
        }
    }
    fn identity(self) -> &'static str {
        match self {
            Self::Resources => "uri",
            Self::Templates => "uriTemplate",
            _ => "name",
        }
    }
}

pub struct CatalogEntry {
    pub name: String,
    pub server: String,
    pub definition: Value,
}

impl CatalogEntry {
    pub fn native_name(&self) -> &str {
        self.definition["name"]
            .as_str()
            .expect("catalog names are validated")
    }

    pub fn summary(&self) -> Value {
        let mut summary = json!({"name": self.name, "server": self.server});
        for key in ["title", "description", "uri", "uriTemplate", "mimeType"] {
            if let Some(value) = self.definition.get(key) {
                summary[key] = value.clone();
            }
        }
        summary
    }

    pub fn schema(&self, kind: CatalogKind) -> Value {
        let mut result = json!({"name": self.name});
        let fields: &[&str] = match kind {
            CatalogKind::Tools => &["inputSchema", "outputSchema"],
            CatalogKind::Prompts => &["arguments"],
            CatalogKind::Resources => &["uri", "mimeType"],
            CatalogKind::Templates => &["uriTemplate", "mimeType"],
        };
        for key in fields {
            if let Some(value) = self.definition.get(*key) {
                result[*key] = value.clone();
            }
        }
        result
    }
}

pub struct Host {
    pub config: Config,
    clients: BTreeMap<String, Client>,
    discoveries: BTreeMap<String, Value>,
    timeout: Duration,
    refresh: bool,
    max_bytes: usize,
    tasks: bool,
}

impl Host {
    pub fn new(
        config: Config,
        timeout: Duration,
        refresh: bool,
        max_bytes: usize,
        tasks: bool,
    ) -> Self {
        Self {
            config,
            clients: BTreeMap::new(),
            discoveries: BTreeMap::new(),
            timeout,
            refresh,
            max_bytes,
            tasks,
        }
    }

    fn client(&mut self, server: &str) -> Result<&mut Client> {
        if !self.clients.contains_key(server) {
            let resolved = self.config.resolve(server)?;
            let timeout = resolved
                .timeout_ms
                .map(Duration::from_millis)
                .map_or(self.timeout, |source| source.min(self.timeout));
            let client = Client::new(resolved, timeout, self.refresh, self.max_bytes, self.tasks)?;
            self.clients.insert(server.to_owned(), client);
        }
        Ok(self.clients.get_mut(server).expect("client inserted"))
    }

    pub async fn discover(&mut self, server: &str) -> Result<&Value> {
        if !self.discoveries.contains_key(server) {
            let discovery = self
                .client(server)?
                .discover()
                .await
                .with_context(|| format!("server {server:?}"))?;
            self.discoveries.insert(server.to_owned(), discovery);
        }
        Ok(&self.discoveries[server])
    }

    pub async fn require(&mut self, server: &str, capability: &str) -> Result<()> {
        ensure!(
            self.discover(server)
                .await?
                .get("capabilities")
                .and_then(|caps| caps.get(capability))
                .is_some(),
            "server {server:?} does not advertise {capability}"
        );
        Ok(())
    }

    pub async fn require_extension(&mut self, server: &str, identifier: &str) -> Result<()> {
        ensure!(
            self.discover(server)
                .await?
                .get("capabilities")
                .and_then(|caps| caps.get("extensions"))
                .and_then(|extensions| extensions.get(identifier))
                .is_some_and(Value::is_object),
            "server {server:?} does not advertise extension {identifier:?}"
        );
        Ok(())
    }

    pub async fn request(&mut self, server: &str, method: &str, params: Value) -> Result<Value> {
        self.client(server)?
            .request(method, params, None)
            .await
            .with_context(|| format!("server {server:?}"))
    }

    pub async fn catalog(
        &mut self,
        kind: CatalogKind,
        server: Option<&str>,
    ) -> Result<Vec<CatalogEntry>> {
        let servers = match server {
            Some(server) => {
                ensure!(
                    self.config.get(server).is_some(),
                    "unknown server {server:?}"
                );
                vec![server.to_owned()]
            }
            None => self.config.servers.keys().cloned().collect(),
        };
        let mut entries = Vec::new();
        for server in servers {
            entries.extend(self.catalog_for(kind, &server).await?);
        }
        entries.sort_by(|left, right| {
            left.name.cmp(&right.name).then_with(|| {
                left.definition[kind.identity()]
                    .as_str()
                    .cmp(&right.definition[kind.identity()].as_str())
            })
        });
        if matches!(kind, CatalogKind::Tools | CatalogKind::Prompts) {
            let mut names = HashSet::new();
            for entry in &entries {
                ensure!(
                    names.insert(&entry.name),
                    "namespace collision for {:?}; use --server to select its server",
                    entry.name
                );
            }
        }
        Ok(entries)
    }

    async fn catalog_for(&mut self, kind: CatalogKind, server: &str) -> Result<Vec<CatalogEntry>> {
        if self
            .discover(server)
            .await?
            .get("capabilities")
            .and_then(|caps| caps.get(kind.capability()))
            .is_none()
        {
            return Ok(Vec::new());
        }
        let client = self.client(server)?;
        let mut params = json!({});
        let mut pages = Vec::new();
        let mut seen_cursors = HashSet::new();
        let mut seen_names = HashSet::new();
        let mut entries = Vec::new();
        let mut restarted = false;
        loop {
            let result = match client.request(kind.method(), params.clone(), None).await {
                Ok(result) => result,
                Err(error)
                    if !restarted
                        && params.get("cursor").is_some()
                        && error
                            .downcast_ref::<RpcError>()
                            .is_some_and(|error| error.code == -32602) =>
                {
                    for page in &pages {
                        client.invalidate(kind.method(), page);
                    }
                    client.invalidate(kind.method(), &params);
                    params = json!({});
                    pages.clear();
                    seen_cursors.clear();
                    seen_names.clear();
                    entries.clear();
                    restarted = true;
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("server {server:?}: {}", kind.method()));
                }
            };
            pages.push(params.clone());
            let mut result = result;
            let definitions = result
                .get_mut(kind.field())
                .and_then(Value::as_array_mut)
                .context("MCP list response is missing its item array")?;
            for definition in definitions.drain(..) {
                let native_name = definition
                    .get("name")
                    .and_then(Value::as_str)
                    .context("MCP catalog item is missing a name")?;
                let identity = definition
                    .get(kind.identity())
                    .and_then(Value::as_str)
                    .context("MCP catalog item is missing its identifier")?;
                ensure!(
                    seen_names.insert(identity.to_owned()),
                    "server {server:?} returned duplicate identifier {identity:?}"
                );
                if kind == CatalogKind::Tools {
                    let validation = (|| -> Result<()> {
                        let input = definition
                            .get("inputSchema")
                            .context("tool is missing inputSchema")?;
                        schema::validate_schema(input)?;
                        if let Some(output) = definition.get("outputSchema") {
                            schema::validate_schema(output)?;
                        }
                        if client.is_http() {
                            schema::validate_annotations(input)?;
                        }
                        Ok(())
                    })();
                    if let Err(error) = validation {
                        eprintln!("cm: excluding invalid tool {server}_{native_name}: {error:#}");
                        continue;
                    }
                }
                entries.push(CatalogEntry {
                    name: format!("{server}_{native_name}"),
                    server: server.to_owned(),
                    definition,
                });
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => break,
                Some(Value::String(cursor)) => {
                    ensure!(
                        seen_cursors.insert(cursor.clone()),
                        "server {server:?} repeated a pagination cursor"
                    );
                    params = json!({"cursor": cursor});
                }
                _ => bail!("server {server:?} returned a non-string pagination cursor"),
            }
        }
        Ok(entries)
    }

    pub async fn locate(
        &mut self,
        kind: CatalogKind,
        name: &str,
        selected: Option<&str>,
    ) -> Result<CatalogEntry> {
        let candidates: Vec<_> = self
            .config
            .servers
            .keys()
            .filter(|server| {
                selected.is_none_or(|selected| selected == server.as_str())
                    && name
                        .strip_prefix(server.as_str())
                        .is_some_and(|suffix| suffix.starts_with('_'))
            })
            .cloned()
            .collect();
        ensure!(
            !candidates.is_empty(),
            "unknown namespaced name {name:?}; use <server>_<name> from the list command"
        );
        let mut found = None;
        for server in candidates {
            if let Some(entry) = self
                .catalog_for(kind, &server)
                .await?
                .into_iter()
                .find(|entry| entry.name == name)
            {
                ensure!(
                    found.is_none(),
                    "ambiguous name {name:?}; use --server to select its server"
                );
                found = Some(entry);
            }
        }
        found.with_context(|| {
            format!(
                "no {} entry {name:?}; list available names or use --refresh",
                kind.field()
            )
        })
    }

    pub async fn locate_resource(
        &mut self,
        server: &str,
        uri: &str,
        template: bool,
    ) -> Result<CatalogEntry> {
        let kind = if template {
            CatalogKind::Templates
        } else {
            CatalogKind::Resources
        };
        let entries = self.catalog(kind, Some(server)).await?;
        entries
            .into_iter()
            .find(|entry| entry.definition[kind.identity()].as_str() == Some(uri))
            .with_context(|| format!("server {server:?} does not list resource URI {uri:?}"))
    }

    pub async fn full_specification(&mut self, entry: CatalogEntry) -> Result<Value> {
        let instructions = self
            .discover(&entry.server)
            .await?
            .get("instructions")
            .cloned();
        Ok(
            json!({"name": entry.name, "server": entry.server, "definition": entry.definition, "instructions": instructions}),
        )
    }

    pub async fn call_tool(&mut self, entry: CatalogEntry, arguments: Value) -> Result<Value> {
        let input_schema = &entry.definition["inputSchema"];
        schema::validate_instance(input_schema, &arguments, "tool arguments")?;
        let result = self
            .client(&entry.server)?
            .request(
                "tools/call",
                json!({"name":entry.native_name(),"arguments":arguments}),
                Some(input_schema),
            )
            .await?;
        if result.get("resultType").and_then(Value::as_str) == Some("task") {
            self.require_extension(&entry.server, crate::tasks::EXTENSION)
                .await?;
        }
        if result.get("resultType").and_then(Value::as_str) != Some("task")
            && result.get("isError").and_then(Value::as_bool) != Some(true)
            && let Some(output_schema) = entry.definition.get("outputSchema")
        {
            let output = result
                .get("structuredContent")
                .context("tool declared an output schema but returned no structuredContent")?;
            schema::validate_instance(output_schema, output, "tool output")?;
        }
        Ok(result)
    }

    pub async fn get_prompt(&mut self, entry: CatalogEntry, arguments: Value) -> Result<Value> {
        if let Some(parameters) = entry.definition.get("arguments").and_then(Value::as_array) {
            for parameter in parameters {
                if parameter.get("required").and_then(Value::as_bool) == Some(true) {
                    let name = parameter
                        .get("name")
                        .and_then(Value::as_str)
                        .context("prompt argument is missing its name")?;
                    ensure!(
                        arguments.get(name).is_some(),
                        "missing required prompt argument {name:?}"
                    );
                }
            }
        }
        self.client(&entry.server)?
            .request(
                "prompts/get",
                json!({"name":entry.native_name(),"arguments":arguments}),
                None,
            )
            .await
    }

    pub async fn read_resource(&mut self, server: &str, uri: String) -> Result<Value> {
        self.require(server, "resources").await?;
        self.client(server)?
            .request("resources/read", json!({"uri": uri}), None)
            .await
    }

    pub async fn complete(
        &mut self,
        server: &str,
        reference: Value,
        argument: String,
        value: String,
        context: Option<Value>,
    ) -> Result<Value> {
        self.require(server, "completions").await?;
        let mut params = json!({"ref":reference,"argument":{"name":argument,"value":value}});
        if let Some(arguments) = context {
            params["context"] = json!({"arguments":arguments});
        }
        self.client(server)?
            .request("completion/complete", params, None)
            .await
    }

    pub async fn close(&mut self) {
        for client in self.clients.values_mut() {
            client.close().await;
        }
    }
}
