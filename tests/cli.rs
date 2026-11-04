//! End-to-end tests for the `cm` binary.
//!
//! Every test drives the real executable against a tiny in-process streamable
//! HTTP MCP server. Configuration and cache always live in a private temporary
//! directory, and client discovery is disabled, so no user state is read.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;
use tiny_http::{Header, Request, Response, Server};

const PROTOCOL: &str = "2026-07-28";
const INSTRUCTIONS: &str = "Fixture server instructions.";
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfixture image bytes";

struct Workspace {
    root: TempDir,
}

impl Workspace {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("create private test workspace"),
        }
    }

    fn config_path(&self) -> PathBuf {
        self.root.path().join("config.json")
    }

    fn cache_path(&self) -> PathBuf {
        self.root.path().join("cache")
    }

    fn write_config(&self, servers: Value) {
        let document = json!({"mcpServers": servers});
        std::fs::write(
            self.config_path(),
            serde_json::to_vec_pretty(&document).expect("serialize test config"),
        )
        .expect("write test config");
    }

    fn configure(&self, endpoint: &str, token: &str) {
        self.write_config(json!({
            "fixture": {
                "url": endpoint,
                "headers": {"Authorization": format!("Bearer {token}")}
            }
        }));
    }

    fn command(&self) -> Command {
        let root = self.root.path().to_str().expect("UTF-8 temp path");
        let mut command = Command::new(env!("CARGO_BIN_EXE_cm"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", self.root.path().join("xdg-config"))
            .env("XDG_CACHE_HOME", self.root.path().join("xdg-cache"))
            .env("CM_CONFIG", self.config_path())
            .env("CM_CACHE_DIR", self.cache_path())
            .env("CM_CLIENTS", "none")
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env("RUST_BACKTRACE", "0")
            .current_dir(self.root.path());
        command
    }

    fn run(&self, arguments: &[&str]) -> Run {
        let output = self
            .command()
            .args(arguments)
            .output()
            .unwrap_or_else(|error| panic!("cannot run cm {arguments:?}: {error}"));
        Run {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).expect("cm stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("cm stderr is UTF-8"),
            arguments: arguments.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }

    fn run_ok(&self, arguments: &[&str]) -> String {
        let run = self.run(arguments);
        assert_eq!(
            run.code,
            Some(0),
            "cm {arguments:?} failed\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr
        );
        assert!(
            run.stderr.is_empty(),
            "successful cm {arguments:?} wrote diagnostics to stderr:\n{}",
            run.stderr
        );
        run.stdout
    }

    fn run_json(&self, arguments: &[&str]) -> Value {
        let stdout = self.run_ok(arguments);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("cm {arguments:?} did not emit JSON: {error}\n{stdout}"))
    }
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    arguments: Vec<String>,
}

impl Run {
    fn assert_failed(&self) {
        assert_eq!(
            self.code,
            Some(1),
            "cm {:?} should exit 1\nstdout:\n{}\nstderr:\n{}",
            self.arguments,
            self.stdout,
            self.stderr
        );
    }

    fn assert_stdout_empty(&self) {
        assert!(
            self.stdout.is_empty(),
            "cm {:?} wrote partial stdout on failure:\n{}",
            self.arguments,
            self.stdout
        );
    }
}

#[derive(Clone)]
struct Record {
    method: String,
    params: Value,
    headers: HashMap<String, String>,
}

struct Fixture {
    endpoint: String,
    records: Arc<Mutex<Vec<Record>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind fixture server");
        let port = server
            .server_addr()
            .to_ip()
            .expect("fixture server has an IP address")
            .port();
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let records = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_records = Arc::clone(&records);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(request)) => handle_request(request, &thread_records),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            records,
            stop,
            thread: Some(thread),
        }
    }

    fn records(&self) -> Vec<Record> {
        self.records.lock().expect("fixture record lock").clone()
    }

    fn request_count(&self) -> usize {
        self.records().len()
    }

    fn calls_to(&self, method: &str, name: &str) -> usize {
        self.records()
            .into_iter()
            .filter(|record| {
                record.method == method
                    && record.params.get("name").and_then(Value::as_str) == Some(name)
            })
            .count()
    }

    fn assert_modern_protocol_only(&self) {
        let records = self.records();
        assert!(!records.is_empty(), "fixture received no requests");
        for record in records {
            let method = record.method.as_str();
            assert_ne!(
                method, "initialize",
                "cm attempted a legacy initialize handshake"
            );
            assert_ne!(method, "notifications/initialized", "cm sent initialized");
            assert!(
                !method.starts_with("notifications/"),
                "cm sent unsupported notification {method:?}"
            );
            assert!(
                !method.contains("elicitation")
                    && !method.contains("sampling")
                    && !method.contains("roots"),
                "cm attempted unsupported interaction {method:?}"
            );
            assert_eq!(
                record
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some(PROTOCOL),
                "{method:?} is missing the modern protocol header"
            );
            assert_eq!(
                record.headers.get("mcp-method").map(String::as_str),
                Some(method),
                "{method:?} sent the wrong mcp-method header"
            );
            assert!(
                !record.headers.contains_key("mcp-session-id"),
                "{method:?} unexpectedly used a session"
            );
            let meta = record
                .params
                .get("_meta")
                .unwrap_or_else(|| panic!("{method:?} is missing request _meta"));
            assert_eq!(
                meta.get("io.modelcontextprotocol/protocolVersion")
                    .and_then(Value::as_str),
                Some(PROTOCOL),
                "{method:?} sent the wrong protocol version"
            );
            assert_eq!(
                meta.get("io.modelcontextprotocol/clientCapabilities")
                    .cloned(),
                Some(json!({})),
                "{method:?} advertised legacy or interactive capabilities"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_request(mut request: Request, records: &Mutex<Vec<Record>>) {
    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    let headers: HashMap<String, String> = request
        .headers()
        .iter()
        .map(|header| {
            (
                header.field.as_str().as_str().to_ascii_lowercase(),
                header.value.as_str().to_owned(),
            )
        })
        .collect();

    if request.method().as_str() != "POST" {
        records.lock().expect("fixture record lock").push(Record {
            method: format!("HTTP {}", request.method().as_str()),
            params: Value::Null,
            headers,
        });
        reply(
            request,
            405,
            "application/json",
            error_body(Value::Null, -32600, "POST required"),
        );
        return;
    }

    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        reply(
            request,
            400,
            "application/json",
            error_body(Value::Null, -32700, "parse error"),
        );
        return;
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    records.lock().expect("fixture record lock").push(Record {
        method: method.clone(),
        params: params.clone(),
        headers: headers.clone(),
    });

    if method.starts_with("notifications/") {
        reply(request, 202, "text/plain", String::new());
        return;
    }

    let authorization = headers.get("authorization").map_or("", String::as_str);
    if method == "server/discover" && authorization == "Bearer legacy" {
        reply(
            request,
            200,
            "application/json",
            error_body(id, -32022, "legacy server requires initialize"),
        );
        return;
    }

    match method.as_str() {
        "server/discover" => {
            let result = cacheable(json!({
                "supportedVersions": [PROTOCOL],
                "serverInfo": {"name": "fixture", "version": "1.0.0"},
                "capabilities": {
                    "tools": {},
                    "prompts": {},
                    "resources": {},
                    "completions": {}
                },
                "instructions": INSTRUCTIONS
            }));
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tools/list" => {
            let result = match params.get("cursor").and_then(Value::as_str) {
                None => cacheable(json!({
                    "tools": [
                        tool("echo", "Echo text", json!({
                            "type": "object",
                            "properties": {
                                "text": {"type": "string"},
                                "count": {"type": "integer"}
                            },
                            "required": ["text"],
                            "additionalProperties": false
                        }), None),
                        tool("structured", "Return structured data", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), Some(json!({
                            "type": "object",
                            "properties": {
                                "answer": {"type": "integer"},
                                "label": {"type": "string"}
                            },
                            "required": ["answer", "label"],
                            "additionalProperties": false
                        }))),
                        tool("image", "Return an image", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), None),
                        tool("fails", "Always reports a tool error", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), None),
                        tool("headers", "Report request headers", json!({
                            "type": "object",
                            "properties": {
                                "target": {"type": "string", "x-mcp-header": "target"},
                                "count": {"type": "integer", "x-mcp-header": "count"}
                            },
                            "required": ["target", "count"],
                            "additionalProperties": false
                        }), None),
                        tool("needs_input", "Ask for unsupported client input", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), None),
                        tool("sse", "Return the result over request-scoped SSE", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), None)
                    ],
                    "nextCursor": "page-2"
                })),
                Some("page-2") => cacheable(json!({
                    "tools": [
                        tool("loud", "Return loud text", json!({
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }), None)
                    ]
                })),
                Some(cursor) => {
                    reply(
                        request,
                        200,
                        "application/json",
                        error_body(id, -32602, &format!("unknown cursor {cursor:?}")),
                    );
                    return;
                }
            };
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if name == "sse" {
                let message = result_message(
                    id,
                    json!({
                        "content": [{"type": "text", "text": "from-sse"}]
                    }),
                );
                let body = format!(
                    "event: message\ndata: {}\n\n",
                    serde_json::to_string(&message).expect("serialize SSE message")
                );
                reply(request, 200, "text/event-stream", body);
                return;
            }
            let result = match name {
                "echo" => json!({
                    "content": [{
                        "type": "text",
                        "text": format!("echo:{}", arguments.get("text").and_then(Value::as_str).unwrap_or(""))
                    }]
                }),
                "structured" => json!({
                    "content": [{"type": "text", "text": "{\"answer\":42,\"label\":\"ok\"}"}],
                    "structuredContent": {"answer": 42, "label": "ok"}
                }),
                "image" => json!({
                    "content": [{
                        "type": "image",
                        "data": STANDARD.encode(PNG_BYTES),
                        "mimeType": "image/png"
                    }]
                }),
                "fails" => json!({
                    "isError": true,
                    "action": "accept",
                    "category": "fixture-error",
                    "content": [{"type": "text", "text": "boom"}]
                }),
                "headers" => {
                    let raw = headers.get("mcp-param-target").cloned().unwrap_or_default();
                    let (target, encoded) = decode_header(&raw);
                    let raw_count = headers.get("mcp-param-count").cloned().unwrap_or_default();
                    let (count, _) = decode_header(&raw_count);
                    json!({
                        "content": [{
                            "type": "text",
                            "text": format!(
                                "auth={authorization};target={target};encoded={encoded};count={count}"
                            )
                        }]
                    })
                }
                "needs_input" => json!({
                    "resultType": "input_required",
                    "inputRequests": [{
                        "type": "elicitation/create",
                        "message": "fixture requires approval"
                    }]
                }),
                _ => {
                    reply(
                        request,
                        200,
                        "application/json",
                        error_body(id, -32602, &format!("unknown tool {name:?}")),
                    );
                    return;
                }
            };
            reply(request, 200, "application/json", result_body(id, result));
        }
        "prompts/list" => {
            let result = cacheable(json!({
                "prompts": [{
                    "name": "greet",
                    "title": "Greeting",
                    "description": "Greets a person",
                    "arguments": [{
                        "name": "name",
                        "description": "Person to greet",
                        "required": true
                    }]
                }]
            }));
            reply(request, 200, "application/json", result_body(id, result));
        }
        "prompts/get" => {
            let name = params
                .get("arguments")
                .and_then(|args| args.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let result = json!({
                "description": "A greeting",
                "messages": [{
                    "role": "user",
                    "content": {"type": "text", "text": format!("Hello, {name}!")}
                }]
            });
            reply(request, 200, "application/json", result_body(id, result));
        }
        "resources/list" => {
            let result = cacheable(json!({
                "resources": [{
                    "name": "note",
                    "title": "A note",
                    "description": "A text note",
                    "uri": "fixture://note",
                    "mimeType": "text/plain"
                }, {
                    "name": "note",
                    "title": "Another note",
                    "description": "A second note with the same display name",
                    "uri": "fixture://other-note",
                    "mimeType": "text/plain"
                }]
            }));
            reply(request, 200, "application/json", result_body(id, result));
        }
        "resources/templates/list" => {
            let result = cacheable(json!({
                "resourceTemplates": [{
                    "name": "note_template",
                    "title": "A note template",
                    "description": "A templated note",
                    "uriTemplate": "fixture://note/{slug}",
                    "mimeType": "text/plain"
                }]
            }));
            reply(request, 200, "application/json", result_body(id, result));
        }
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            let text = if uri == "fixture://note" {
                "Note body\n".to_owned()
            } else {
                format!("Note {uri} body\n")
            };
            let result = cacheable(json!({
                "contents": [{"uri": uri, "mimeType": "text/plain", "text": text}]
            }));
            reply(request, 200, "application/json", result_body(id, result));
        }
        "completion/complete" => {
            let value = params
                .pointer("/argument/value")
                .and_then(Value::as_str)
                .unwrap_or("");
            let prompt = params.pointer("/ref/type").and_then(Value::as_str) == Some("ref/prompt");
            let values = if prompt {
                vec![format!("{value}da"), format!("{value}licia")]
            } else {
                vec![format!("{value}lpha"), format!("{value}mber")]
            };
            let result = json!({
                "completion": {"values": values, "hasMore": false, "total": 2}
            });
            reply(request, 200, "application/json", result_body(id, result));
        }
        _ => {
            reply(
                request,
                200,
                "application/json",
                error_body(id, -32601, &format!("unsupported method {method:?}")),
            );
        }
    }
}

fn tool(name: &str, description: &str, input_schema: Value, output_schema: Option<Value>) -> Value {
    let mut value = json!({
        "name": name,
        "title": format!("Fixture {name}"),
        "description": description,
        "inputSchema": input_schema
    });
    if let Some(schema) = output_schema {
        value["outputSchema"] = schema;
    }
    value
}

fn cacheable(mut result: Value) -> Value {
    result["resultType"] = json!("complete");
    result["ttlMs"] = json!(600_000);
    result["cacheScope"] = json!("private");
    result
}

fn decode_header(raw: &str) -> (String, bool) {
    if raw.starts_with("=?base64?") && raw.ends_with("?=") {
        let inner = &raw["=?base64?".len()..raw.len() - "?=".len()];
        if let Ok(bytes) = STANDARD.decode(inner)
            && let Ok(text) = String::from_utf8(bytes)
        {
            return (text, true);
        }
    }
    (raw.to_owned(), false)
}

fn result_message(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn result_body(id: Value, result: Value) -> String {
    serde_json::to_string(&result_message(id, result)).expect("serialize fixture result")
}

fn error_body(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }))
    .expect("serialize fixture error")
}

fn reply(request: Request, status: u16, content_type: &str, body: String) {
    let header = Header::from_bytes("Content-Type", content_type).expect("valid content type");
    let _ = request.respond(
        Response::from_string(body)
            .with_status_code(status)
            .with_header(header),
    );
}

#[test]
fn catalogs_are_paginated_namespaced_and_compact() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.write_config(json!({
        "alpha": {
            "url": fixture.endpoint.clone(),
            "headers": {"Authorization": "Bearer token-one"}
        },
        "beta": {
            "url": fixture.endpoint.clone(),
            "headers": {"Authorization": "Bearer token-two"}
        }
    }));

    let catalog = workspace.run_json(&["tools", "list"]);
    let entries = catalog.as_array().expect("tools list is a JSON array");
    let mut expected = Vec::new();
    for server in ["alpha", "beta"] {
        for tool in [
            "echo",
            "fails",
            "headers",
            "image",
            "loud",
            "needs_input",
            "sse",
            "structured",
        ] {
            expected.push(format!("{server}_{tool}"));
        }
    }
    expected.sort();
    let names: Vec<String> = entries
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .expect("catalog name")
                .to_owned()
        })
        .collect();
    assert_eq!(names, expected, "both paginated pages must be namespaced");
    for entry in entries {
        let object = entry
            .as_object()
            .expect("compact catalog entry is an object");
        assert!(object.contains_key("server"));
        let name = object["name"].as_str().unwrap();
        let server = object["server"].as_str().unwrap();
        assert!(
            name.starts_with(&format!("{server}_")),
            "{name:?} is not namespaced under {server:?}"
        );
        assert!(
            !object.contains_key("inputSchema"),
            "{name:?} leaked its input schema"
        );
        assert!(
            !object.contains_key("outputSchema"),
            "{name:?} leaked its output schema"
        );
        assert!(
            !object.contains_key("definition"),
            "{name:?} leaked its definition"
        );
    }

    let selected = workspace.run_json(&["--server", "alpha", "tools", "list"]);
    let selected_names: Vec<_> = selected
        .as_array()
        .expect("selected tools list is an array")
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(selected_names.len(), 8);
    assert!(selected_names.iter().all(|name| name.starts_with("alpha_")));

    let discovered = workspace.run_json(&["tools", "discover", "alpha_echo"]);
    assert_eq!(discovered["name"], "alpha_echo");
    assert_eq!(discovered["server"], "alpha");
    assert_eq!(discovered["instructions"], INSTRUCTIONS);
    assert_eq!(discovered["definition"]["name"], "echo");
    assert!(discovered["definition"]["inputSchema"].is_object());
    assert_eq!(
        discovered["definition"]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );

    fixture.assert_modern_protocol_only();
}

#[test]
fn discovery_is_cached_on_disk_and_scoped_to_credentials() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.configure(&fixture.endpoint, "token-one");

    let first = workspace.run_json(&["tools", "list"]);
    let first_requests = fixture.request_count();
    assert!(
        first_requests >= 3,
        "expected discovery plus two catalog pages, saw {first_requests} requests"
    );
    assert!(first.as_array().is_some_and(|entries| entries.len() == 8));

    let second = workspace.run_json(&["tools", "list"]);
    assert_eq!(
        fixture.request_count(),
        first_requests,
        "a repeated invocation must use the disk cache"
    );
    assert_eq!(second, first);

    let cache_dir = workspace.cache_path();
    let cached_files: Vec<_> = std::fs::read_dir(&cache_dir)
        .expect("cache directory exists")
        .map(|entry| entry.expect("cache entry").path())
        .collect();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&cache_dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "cache directory must be private"
        );
        for file in &cached_files {
            assert_eq!(
                std::fs::metadata(file).unwrap().permissions().mode() & 0o077,
                0,
                "cache entry {:?} is readable by other users",
                file
            );
        }
    }

    workspace.configure(&fixture.endpoint, "token-two");
    let other_credentials = workspace.run_json(&["tools", "list"]);
    assert!(
        fixture.request_count() > first_requests,
        "a changed token must not reuse the private cache"
    );
    assert_eq!(other_credentials, first);

    let refresh_requests = fixture.request_count();
    let refreshed = workspace.run_json(&["--refresh", "tools", "list"]);
    assert!(
        fixture.request_count() > refresh_requests,
        "--refresh must bypass fresh cached metadata"
    );
    assert_eq!(refreshed, first);

    fixture.assert_modern_protocol_only();
}

#[test]
fn tool_calls_render_text_structured_media_headers_and_errors() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.configure(&fixture.endpoint, "token-one");

    assert_eq!(
        workspace.run_ok(&["tools", "call", "fixture_echo", r#"{"text":"hello"}"#]),
        "echo:hello\n"
    );

    let structured = workspace.run_ok(&["tools", "call", "fixture_structured"]);
    assert_eq!(
        serde_json::from_str::<Value>(&structured).expect("structured auto output is JSON"),
        json!({"answer": 42, "label": "ok"})
    );

    assert_eq!(
        workspace.run_ok(&["tools", "call", "fixture_sse"]),
        "from-sse\n",
        "request-scoped SSE results must be decoded"
    );

    assert_eq!(
        workspace.run_ok(&[
            "tools",
            "call",
            "fixture_headers",
            r#"{"target":"café","count":42.0}"#,
        ]),
        "auth=Bearer token-one;target=café;encoded=true;count=42\n",
        "configured authorization, safely encoded x-mcp-header values, and integral numbers must reach the server"
    );

    let image = workspace.run_json(&["--json", "tools", "call", "fixture_image"]);
    let block = &image["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["mimeType"], "image/png");
    assert!(
        block["data"].is_object(),
        "--json must replace base64 with a file descriptor"
    );
    let path = PathBuf::from(block["data"]["path"].as_str().expect("image path"));
    assert!(path.is_absolute(), "image path {path:?} must be absolute");
    assert_eq!(
        std::fs::read(&path).expect("image artifact survives the cm process"),
        PNG_BYTES
    );
    std::fs::remove_file(&path).expect("remove test image artifact");

    let failed = workspace.run(&["tools", "call", "fixture_fails"]);
    failed.assert_failed();
    assert_eq!(
        failed.stdout, "boom\n",
        "tool error content is still the result"
    );

    let failed_json = workspace.run(&["--json", "tools", "call", "fixture_fails"]);
    failed_json.assert_failed();
    let envelope: Value = serde_json::from_str(&failed_json.stdout)
        .expect("--json tool error output must be a JSON envelope");
    assert_eq!(envelope["isError"], true);
    assert_eq!(envelope["action"], "accept");
    assert_eq!(envelope["category"], "fixture-error");
    assert_eq!(envelope["content"][0]["text"], "boom");

    fixture.assert_modern_protocol_only();
}

#[test]
fn invalid_arguments_legacy_protocols_and_requested_input_are_rejected() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.write_config(json!({
        "fixture": {
            "url": fixture.endpoint.clone(),
            "headers": {"Authorization": "Bearer token-one"}
        },
        "legacy": {
            "url": fixture.endpoint.clone(),
            "headers": {"Authorization": "Bearer legacy"}
        }
    }));

    let invalid = workspace.run(&["tools", "call", "fixture_echo", r#"{"text":42}"#]);
    invalid.assert_failed();
    invalid.assert_stdout_empty();
    assert_eq!(
        fixture.calls_to("tools/call", "echo"),
        0,
        "invalid arguments must be rejected before the tool executes"
    );

    let legacy = workspace.run(&["--server", "legacy", "tools", "list"]);
    legacy.assert_failed();
    legacy.assert_stdout_empty();

    let requested = workspace.run(&["tools", "call", "fixture_needs_input", "{}"]);
    requested.assert_failed();
    requested.assert_stdout_empty();

    fixture.assert_modern_protocol_only();
}

#[test]
fn prompts_resources_and_completion_are_usable_end_to_end() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.configure(&fixture.endpoint, "token-one");

    let prompts = workspace.run_json(&["prompts", "list"]);
    let prompt_names: Vec<_> = prompts
        .as_array()
        .expect("prompts list is an array")
        .iter()
        .map(|prompt| prompt["name"].as_str().unwrap())
        .collect();
    assert_eq!(prompt_names, vec!["fixture_greet"]);

    let schema = workspace.run_json(&["prompts", "schema", "fixture_greet"]);
    assert_eq!(schema["name"], "fixture_greet");
    assert_eq!(schema["arguments"][0]["name"], "name");
    assert_eq!(schema["arguments"][0]["required"].as_bool(), Some(true));

    assert_eq!(
        workspace.run_ok(&["prompts", "get", "fixture_greet", r#"{"name":"Ada"}"#]),
        "user:\nHello, Ada!\n"
    );

    let prompt_completion =
        workspace.run_json(&["prompts", "complete", "fixture_greet", "name", "A"]);
    assert_eq!(prompt_completion["values"], json!(["Ada", "Alicia"]));

    let resources_value = workspace.run_json(&["resources", "list"]);
    let resources = resources_value
        .as_array()
        .expect("resources list is an array");
    assert_eq!(
        resources.len(),
        2,
        "resources with the same display name but different URIs must both be listed"
    );
    let note = resources
        .iter()
        .find(|resource| resource["uri"] == "fixture://note")
        .expect("fixture://note is listed");
    assert_eq!(note["name"], "fixture_note");
    assert_eq!(note["mimeType"], "text/plain");
    let other = resources
        .iter()
        .find(|resource| resource["uri"] == "fixture://other-note")
        .expect("fixture://other-note is listed");
    assert_eq!(other["name"], "fixture_note");
    assert_eq!(other["uri"], "fixture://other-note");

    let templates = workspace.run_json(&["resources", "templates"]);
    assert_eq!(templates[0]["name"], "fixture_note_template");
    assert_eq!(templates[0]["uriTemplate"], "fixture://note/{slug}");

    assert_eq!(
        workspace.run_ok(&["resources", "read", "fixture", "fixture://note"]),
        "Note body\n"
    );

    let resource_completion = workspace.run_json(&[
        "resources",
        "complete",
        "fixture",
        "fixture://note/{slug}",
        "slug",
        "a",
    ]);
    assert_eq!(resource_completion["values"], json!(["alpha", "amber"]));

    fixture.assert_modern_protocol_only();
}
