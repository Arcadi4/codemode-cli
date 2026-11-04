use std::{collections::HashMap, io, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use rmcp::{model::JsonRpcMessage, transport::async_rw::JsonRpcMessageCodec};
use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::config::ResolvedTransport;

// rmcp's generic envelope/codec preserves extension fields. Its typed ServerResult
// union is intentionally not used: overlapping result shapes can lose isError.
pub type Message = JsonRpcMessage<Value, Value, Value>;

type Input = FramedWrite<ChildStdin, JsonRpcMessageCodec<Message>>;
type Output = FramedRead<ChildStdout, JsonRpcMessageCodec<Message>>;

pub enum Connection {
    Stdio {
        child: Child,
        input: Option<Input>,
        output: Output,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: HeaderMap,
        max_bytes: usize,
    },
}

impl Connection {
    pub fn open(endpoint: ResolvedTransport, max_bytes: usize) -> Result<Self> {
        match endpoint {
            ResolvedTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                let mut process = tokio::process::Command::new(&command);
                process
                    .args(args)
                    .envs(env)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::inherit())
                    .kill_on_drop(true);
                if let Some(cwd) = cwd {
                    process.current_dir(cwd);
                }
                #[cfg(unix)]
                process.process_group(0);
                let mut child = process
                    .spawn()
                    .with_context(|| format!("cannot start MCP server executable {command:?}"))?;
                let input = child.stdin.take().context("cannot open MCP server stdin")?;
                let output = child
                    .stdout
                    .take()
                    .context("cannot open MCP server stdout")?;
                Ok(Self::Stdio {
                    child,
                    input: Some(FramedWrite::new(input, JsonRpcMessageCodec::new())),
                    output: FramedRead::new(
                        output,
                        JsonRpcMessageCodec::new_with_max_length(max_bytes),
                    ),
                })
            }
            ResolvedTransport::Http { url, headers } => {
                let parsed = reqwest::Url::parse(&url).context("invalid MCP endpoint URL")?;
                ensure!(
                    matches!(parsed.scheme(), "http" | "https"),
                    "MCP endpoint must use HTTP or HTTPS"
                );
                let mut configured_headers = HeaderMap::new();
                for (key, value) in headers {
                    let key = HeaderName::from_bytes(key.as_bytes())
                        .context("invalid configured HTTP header name")?;
                    ensure!(
                        key != "mcp-session-id" && key != "last-event-id",
                        "session and resume headers are not supported by MCP 2026-07-28"
                    );
                    let mut value = HeaderValue::from_str(&value)
                        .context("invalid configured HTTP header value")?;
                    value.set_sensitive(true);
                    configured_headers.insert(key, value);
                }
                let client = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .build()?;
                Ok(Self::Http {
                    client,
                    url,
                    headers: configured_headers,
                    max_bytes,
                })
            }
        }
    }

    pub async fn exchange(
        &mut self,
        message: Message,
        protocol_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<Message> {
        match self {
            Self::Stdio { input, output, .. } => {
                input
                    .as_mut()
                    .context("MCP server stdin is closed")?
                    .send(message)
                    .await
                    .context("cannot write to MCP server")?;
                while let Some(message) = output.next().await {
                    let message = message
                        .context("cannot decode MCP response (check --max-response-bytes)")?;
                    if !matches!(message, Message::Notification(_)) {
                        return Ok(message);
                    }
                }
                bail!("MCP server closed stdout before returning a response")
            }
            Self::Http {
                client,
                url,
                headers,
                max_bytes,
            } => {
                let mut headers = headers.clone();
                headers.extend(protocol_headers);
                headers.insert(
                    ACCEPT,
                    HeaderValue::from_static("application/json, text/event-stream"),
                );
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                let response = client
                    .post(url.as_str())
                    .headers(headers)
                    .json(&message)
                    .send()
                    .await
                    .map_err(without_url)?;
                let status = response.status();
                if status == reqwest::StatusCode::UNAUTHORIZED {
                    bail!(
                        "HTTP authorization required; log in with the source client or update its credentials"
                    );
                }
                if status == reqwest::StatusCode::FORBIDDEN {
                    bail!(
                        "HTTP authorization forbidden; check the source client's credentials and scopes"
                    );
                }
                let content_type = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_owned();
                if let Some(length) = response.content_length() {
                    ensure!(
                        length <= *max_bytes as u64,
                        "HTTP response exceeds --max-response-bytes"
                    );
                }
                let max_bytes = *max_bytes;
                let mut received = 0usize;
                let stream = response.bytes_stream().map(move |chunk| -> io::Result<_> {
                    let chunk = chunk.map_err(|error| io::Error::other(error.without_url()))?;
                    received = received.saturating_add(chunk.len());
                    if received > max_bytes {
                        return Err(io::Error::other(
                            "HTTP response exceeds --max-response-bytes",
                        ));
                    }
                    Ok(chunk)
                });
                if status.is_success() && content_type == "text/event-stream" {
                    let mut events = Box::pin(sse_stream::SseStream::from_bytes_stream(stream));
                    while let Some(event) = events.next().await {
                        let event = event.context("cannot decode MCP SSE response")?;
                        let Some(data) = event.data.filter(|data| !data.trim().is_empty()) else {
                            continue;
                        };
                        let message: Message = serde_json::from_str(&data)
                            .context("invalid JSON-RPC in MCP SSE response")?;
                        if !matches!(message, Message::Notification(_)) {
                            return Ok(message);
                        }
                    }
                    bail!("MCP SSE response ended without a final response")
                }
                if status.is_success() {
                    ensure!(
                        content_type == "application/json",
                        "MCP HTTP response must be application/json or text/event-stream"
                    );
                }
                let mut body = Vec::new();
                let mut stream = Box::pin(stream);
                while let Some(chunk) = stream.next().await {
                    body.extend_from_slice(&chunk?);
                }
                match serde_json::from_slice::<Message>(&body) {
                    Ok(message) if status.is_success() || matches!(message, Message::Error(_)) => {
                        Ok(message)
                    }
                    _ if !status.is_success() => bail!(
                        "MCP endpoint returned HTTP {status}; only modern Streamable HTTP is supported"
                    ),
                    Err(error) => Err(error).context("invalid JSON-RPC in MCP HTTP response"),
                    _ => bail!("invalid MCP HTTP response"),
                }
            }
        }
    }

    pub async fn cancel(&mut self, id: u64) {
        if let Self::Stdio {
            input: Some(input), ..
        } = self
        {
            if let Ok(message) = serde_json::from_value(
                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}}),
            ) {
                let _ = tokio::time::timeout(Duration::from_millis(200), input.send(message)).await;
            }
        }
        // On HTTP, dropping the request/response future closes the stream, which
        // is the protocol's cancellation signal. Never POST a cancellation.
    }

    pub async fn close(&mut self) {
        if let Self::Stdio { child, input, .. } = self {
            drop(input.take()); // actual pipe EOF, not merely AsyncWrite::shutdown
            if tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_ok()
            {
                return;
            }
            #[cfg(unix)]
            signal_group(child, nix::sys::signal::Signal::SIGTERM);
            if tokio::time::timeout(Duration::from_secs(1), child.wait())
                .await
                .is_ok()
            {
                return;
            }
            #[cfg(unix)]
            signal_group(child, nix::sys::signal::Signal::SIGKILL);
            if let Err(error) = child.kill().await {
                eprintln!("cm: cannot reap MCP server: {error}");
            }
        }
    }
}

#[cfg(unix)]
fn signal_group(child: &Child, signal: nix::sys::signal::Signal) {
    if let Some(id) = child.id() {
        let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(id as i32), signal);
    }
}

fn without_url(error: reqwest::Error) -> anyhow::Error {
    error.without_url().into()
}
