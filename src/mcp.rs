use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    cache::Cache,
    config::{ResolvedServer, ResolvedTransport},
    schema,
    transport::{Connection, Message},
};

pub const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    method: String,
    message: String,
}
impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "MCP {} error {}: {}",
            self.method, self.code, self.message
        )
    }
}
impl std::error::Error for RpcError {}

pub struct Client {
    endpoint: Option<ResolvedTransport>,
    connection: Option<Connection>,
    http: bool,
    cache: Cache,
    timeout: Duration,
    max_bytes: usize,
    next_id: u64,
    in_flight: Option<u64>,
}

impl Client {
    pub fn new(
        server: ResolvedServer,
        timeout: Duration,
        refresh: bool,
        max_bytes: usize,
    ) -> Result<Self> {
        let context = format!(
            "cm:{PROTOCOL_VERSION}:{}:{}",
            env!("CARGO_PKG_VERSION"),
            server.fingerprint()
        );
        let http = matches!(server.transport, ResolvedTransport::Http { .. });
        Ok(Self {
            endpoint: Some(server.transport),
            connection: None,
            http,
            cache: Cache::new(context.as_bytes(), refresh)?,
            timeout,
            max_bytes,
            next_id: 1,
            in_flight: None,
        })
    }

    pub fn is_http(&self) -> bool {
        self.http
    }

    pub async fn discover(&mut self) -> Result<Value> {
        let result = self.request("server/discover", json!({}), None).await
            .context("server discovery failed; cm requires MCP 2026-07-28 without an initialize handshake")?;
        verify_version(&result)?;
        Ok(result)
    }

    fn connect(&mut self) -> Result<()> {
        if self.connection.is_none() {
            self.connection = Some(Connection::open(
                self.endpoint
                    .take()
                    .context("MCP endpoint already consumed")?,
                self.max_bytes,
            )?);
        }
        Ok(())
    }

    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
        tool_schema: Option<&Value>,
    ) -> Result<Value> {
        match self.cache.get(method, &params) {
            Ok(Some(result)) => return Ok(result),
            Ok(None) => {}
            Err(error) => eprintln!("cm: {error:#}; fetching from the server"),
        }
        if !self.http && self.connection.is_none() {
            self.connect()?;
            // A fresh stdio process is probed before any era-ambiguous request.
            // Never fall back to initialize, even if a legacy server requests it.
            if method != "server/discover" {
                let discovery = self
                    .rpc("server/discover", json!({}), None)
                    .await
                    .context("server must support MCP 2026-07-28 server/discover")?;
                verify_version(&discovery)?;
                self.save("server/discover", &json!({}), &discovery);
            }
        }
        let cache_params = crate::cache::cacheable(method, &params).then(|| params.clone());
        let result = self.rpc(method, params, tool_schema).await?;
        if method == "server/discover" {
            verify_version(&result)?;
        }
        if let Some(params) = cache_params {
            self.save(method, &params, &result);
        }
        Ok(result)
    }

    fn save(&self, method: &str, params: &Value, result: &Value) {
        if let Err(error) = self.cache.put(method, params, result) {
            eprintln!("cm: {error:#}; response was not cached");
        }
    }

    pub fn invalidate(&self, method: &str, params: &Value) {
        if let Err(error) = self.cache.invalidate(method, params) {
            eprintln!("cm: cannot invalidate cache: {error:#}");
        }
    }

    async fn rpc(
        &mut self,
        method: &str,
        mut params: Value,
        tool_schema: Option<&Value>,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let headers = if self.http {
            schema::request_headers(method, &params, tool_schema)?
        } else {
            Default::default()
        };
        let request_params = params
            .as_object_mut()
            .context("request parameters must be an object")?;
        request_params.insert("_meta".into(), json!({
            "io.modelcontextprotocol/protocolVersion": PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientInfo": {"name":"codemode-cli", "version":env!("CARGO_PKG_VERSION")},
            "io.modelcontextprotocol/clientCapabilities": {}
        }));
        let request: Message = serde_json::from_value(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        self.connect()?;
        self.in_flight = Some(id);
        let response = tokio::time::timeout(
            self.timeout,
            self.connection.as_mut().unwrap().exchange(request, headers),
        )
        .await;
        self.in_flight = None;
        let response = match response {
            Ok(result) => result?,
            Err(_) => {
                self.connection.as_mut().unwrap().cancel(id).await;
                bail!(
                    "{method} exceeded the {} second deadline; the operation may have executed, so it was not retried",
                    self.timeout.as_secs_f64()
                );
            }
        };
        match response {
            Message::Response(response) => {
                ensure!(
                    serde_json::to_value(&response.id)? == json!(id),
                    "server returned an unrelated response ID"
                );
                let result = response.result;
                ensure!(result.is_object(), "MCP result must be an object");
                match result.get("resultType") {
                    None => {} // Required backwards-compatible interpretation from the spec.
                    Some(Value::String(kind)) if kind == "complete" => {}
                    Some(Value::String(kind)) if kind == "input_required" => bail!(
                        "server requested client input; cm does not support elicitation, sampling, or roots"
                    ),
                    Some(kind) => bail!("server returned unsupported resultType {kind}"),
                }
                Ok(result)
            }
            Message::Error(response) => {
                if let Some(response_id) = &response.id {
                    ensure!(
                        serde_json::to_value(response_id)? == json!(id),
                        "server returned an unrelated error ID"
                    );
                }
                let error = serde_json::to_value(response.error)?;
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown MCP error");
                match code {
                    -32022 => bail!(
                        "server rejects MCP {PROTOCOL_VERSION}; older protocol versions are not supported ({message})"
                    ),
                    -32021 => bail!(
                        "server requires unsupported client capabilities; cm does not offer elicitation or deprecated capabilities ({message})"
                    ),
                    _ => Err(RpcError {
                        code,
                        method: method.to_owned(),
                        message: message.to_owned(),
                    }
                    .into()),
                }
            }
            Message::Request(_) => bail!(
                "server-initiated JSON-RPC requests are not supported by MCP {PROTOCOL_VERSION}"
            ),
            _ => bail!("server did not return a JSON-RPC response"),
        }
    }

    pub async fn close(&mut self) {
        if let Some(connection) = &mut self.connection {
            if let Some(id) = self.in_flight.take() {
                connection.cancel(id).await;
            }
            connection.close().await;
        }
    }
}

fn verify_version(discovery: &Value) -> Result<()> {
    ensure!(
        discovery
            .get("supportedVersions")
            .and_then(Value::as_array)
            .is_some_and(|versions| versions.iter().any(|version| version == PROTOCOL_VERSION)),
        "server does not support MCP {PROTOCOL_VERSION}; older versions are not supported"
    );
    ensure!(
        discovery.get("capabilities").is_some_and(Value::is_object),
        "server discovery is missing its capabilities"
    );
    Ok(())
}
