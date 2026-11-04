use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;
use tiny_http::{Header, Request, Response, Server};

const PROTOCOL: &str = "2026-07-28";
const EXTENSION: &str = "io.modelcontextprotocol/tasks";
const CREATED_AT: &str = "2026-11-04T10:00:00Z";
const UPDATED_AT: &str = "2026-11-04T10:01:00Z";
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfixture task image";

struct Workspace {
    root: TempDir,
}

impl Workspace {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("create private task workspace"),
        }
    }

    fn configure(&self, endpoint: &str) {
        std::fs::write(
            self.root.path().join("config.json"),
            serde_json::to_vec_pretty(&json!({
                "mcpServers": {
                    "fixture": {"url": endpoint}
                }
            }))
            .expect("serialize task config"),
        )
        .expect("write task config");
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
            .env("CM_CONFIG", self.root.path().join("config.json"))
            .env("CM_CACHE_DIR", self.root.path().join("cache"))
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

    fn run_json(&self, arguments: &[&str]) -> Value {
        let run = self.run(arguments);
        assert_eq!(
            run.code,
            Some(0),
            "cm {arguments:?} failed\nstdout:\n{}\nstderr:\n{}",
            run.stdout,
            run.stderr
        );
        serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
            panic!(
                "cm {arguments:?} did not emit JSON: {error}\n{}",
                run.stdout
            )
        })
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

struct State {
    records: Mutex<Vec<Record>>,
    media_polls: AtomicUsize,
}

struct Fixture {
    endpoint: String,
    state: Arc<State>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind task fixture");
        let port = server
            .server_addr()
            .to_ip()
            .expect("task fixture has an IP address")
            .port();
        let endpoint = format!("http://127.0.0.1:{port}/mcp");
        let state = Arc::new(State {
            records: Mutex::new(Vec::new()),
            media_polls: AtomicUsize::new(0),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(request)) => handle(request, &thread_state),
                    Ok(None) => {}
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn records(&self) -> Vec<Record> {
        self.state
            .records
            .lock()
            .expect("fixture record lock")
            .clone()
    }

    fn count(&self, method: &str) -> usize {
        self.records()
            .iter()
            .filter(|record| record.method == method)
            .count()
    }

    /// Check every recorded request. Records before `opt_in_from` must have no
    /// tasks capability; records from there on must carry the extension.
    fn assert_modern_protocol(&self, opt_in_from: usize) {
        for (index, record) in self.records().into_iter().enumerate() {
            let method = record.method.as_str();
            assert_ne!(method, "initialize");
            assert_ne!(method, "notifications/initialized");
            assert!(!method.starts_with("notifications/"));
            assert!(!method.starts_with("tasks/list"));
            assert!(!method.contains("tasks/result"));
            assert!(!method.contains("elicitation") && !method.contains("sampling"));
            assert!(
                !record.headers.contains_key("mcp-session-id"),
                "{method:?} used a session"
            );
            assert_eq!(
                record
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some(PROTOCOL),
                "{method:?} is missing the protocol header"
            );
            assert_eq!(
                record.headers.get("mcp-method").map(String::as_str),
                Some(method),
                "{method:?} sent the wrong mcp-method header"
            );
            let meta = record
                .params
                .get("_meta")
                .unwrap_or_else(|| panic!("{method:?} is missing request _meta"));
            assert_eq!(
                meta.get("io.modelcontextprotocol/protocolVersion")
                    .and_then(Value::as_str),
                Some(PROTOCOL)
            );
            let capabilities = meta
                .get("io.modelcontextprotocol/clientCapabilities")
                .expect("request client capabilities");
            if index >= opt_in_from {
                assert_eq!(
                    capabilities
                        .get("extensions")
                        .and_then(|extensions| extensions.get(EXTENSION)),
                    Some(&json!({})),
                    "{method:?} did not opt into the tasks extension"
                );
            } else {
                assert_eq!(
                    capabilities,
                    &json!({}),
                    "{method:?} advertised tasks without an explicit opt-in"
                );
            }
            if matches!(method, "tasks/get" | "tasks/cancel") {
                let task_id = record.params["taskId"].as_str().expect("task request ID");
                assert_eq!(
                    record.headers.get("mcp-name").map(String::as_str),
                    Some(task_id),
                    "{method:?} must route by task ID"
                );
            }
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

fn handle(mut request: Request, state: &State) {
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
    state
        .records
        .lock()
        .expect("fixture record lock")
        .push(Record {
            method: method.clone(),
            params: params.clone(),
            headers,
        });
    if request.method().as_str() != "POST" {
        reply(
            request,
            405,
            "application/json",
            error_body(id, -32600, "POST required"),
        );
        return;
    }

    match method.as_str() {
        "server/discover" => {
            let result = json!({
                "supportedVersions": [PROTOCOL],
                "serverInfo": {"name": "task-fixture", "version": "1.0.0"},
                "capabilities": {
                    "tools": {},
                    "extensions": {EXTENSION: {}}
                }
            });
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tools/list" => {
            let result = json!({
                "tools": [
                    tool("media", "Finish with an image", Some(json!({
                        "type": "object",
                        "properties": {"answer": {"type": "integer"}},
                        "required": ["answer"],
                        "additionalProperties": false
                    }))),
                    tool("tool_error", "Finish with a tool error", None),
                    tool("input_task", "Wait for client input", None),
                    tool("failed_task", "Fail at the protocol level", None),
                    tool("cancel_task", "Wait to be cancelled", None)
                ]
            });
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let task_id = match name {
                "media" => "task-media",
                "tool_error" => "task-error",
                "input_task" => "task-input",
                "failed_task" => "task-failed",
                "cancel_task" => "task-cancel",
                _ => {
                    reply(
                        request,
                        200,
                        "application/json",
                        error_body(id, -32602, "unknown tool"),
                    );
                    return;
                }
            };
            let result = json!({
                "resultType": "task",
                "taskId": task_id,
                "status": "working",
                "statusMessage": "queued",
                "createdAt": CREATED_AT,
                "lastUpdatedAt": CREATED_AT,
                "ttlMs": 600_000,
                "pollIntervalMs": 250
            });
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tasks/get" => {
            let task_id = params.get("taskId").and_then(Value::as_str).unwrap_or("");
            let result = match task_id {
                "task-media" => {
                    let poll = state.media_polls.fetch_add(1, Ordering::SeqCst);
                    if poll == 0 {
                        task(task_id, "working", json!({}))
                    } else {
                        task(
                            task_id,
                            "completed",
                            json!({
                                "statusMessage": "ready",
                                "result": {
                                    "content": [{
                                        "type": "image",
                                        "data": STANDARD.encode(PNG_BYTES),
                                        "mimeType": "image/png"
                                    }]
                                }
                            }),
                        )
                    }
                }
                "task-error" => task(
                    task_id,
                    "completed",
                    json!({
                        "result": {
                            "isError": true,
                            "content": [{"type": "text", "text": "task tool failed"}]
                        }
                    }),
                ),
                "task-input" => task(
                    task_id,
                    "input_required",
                    json!({
                        "inputRequests": {
                            "approval": {
                                "method": "elicitation/create",
                                "params": {
                                    "message": "approve",
                                    "requestedSchema": {
                                        "type": "object",
                                        "properties": {},
                                        "additionalProperties": false
                                    }
                                }
                            }
                        }
                    }),
                ),
                "task-failed" => task(
                    task_id,
                    "failed",
                    json!({
                        "statusMessage": "execution failed",
                        "error": {"code": -32603, "message": "fixture execution failure"}
                    }),
                ),
                _ => {
                    reply(
                        request,
                        200,
                        "application/json",
                        error_body(id, -32602, "task not found"),
                    );
                    return;
                }
            };
            reply(request, 200, "application/json", result_body(id, result));
        }
        "tasks/cancel" => {
            reply(
                request,
                200,
                "application/json",
                result_body(id, json!({"resultType": "complete"})),
            );
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

fn tool(name: &str, description: &str, output_schema: Option<Value>) -> Value {
    let mut value = json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
    });
    if let Some(schema) = output_schema {
        value["outputSchema"] = schema;
    }
    value
}

fn task(task_id: &str, status: &str, extra: Value) -> Value {
    let mut result = json!({
        "resultType": "complete",
        "taskId": task_id,
        "status": status,
        "createdAt": CREATED_AT,
        "lastUpdatedAt": UPDATED_AT,
        "ttlMs": 600_000,
        "pollIntervalMs": 250
    });
    if let Some(fields) = extra.as_object() {
        for (key, value) in fields {
            result[key] = value.clone();
        }
    }
    result
}

fn result_body(id: Value, result: Value) -> String {
    serde_json::to_string(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
        .expect("serialize task result")
}

fn error_body(id: Value, code: i64, message: &str) -> String {
    serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    }))
    .expect("serialize task error")
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
fn tasks_are_durable_across_processes_and_completed_media_is_materialized() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.configure(&fixture.endpoint);

    let receipt =
        workspace.run_json(&["--json", "--tasks", "tools", "call", "fixture_media", "{}"]);
    assert_eq!(receipt["resultType"], "task");
    assert_eq!(receipt["taskId"], "task-media");
    assert_eq!(receipt["status"], "working");
    assert_eq!(receipt["createdAt"], CREATED_AT);
    assert_eq!(receipt["lastUpdatedAt"], CREATED_AT);
    assert_eq!(receipt["ttlMs"], 600_000);
    assert_eq!(receipt["pollIntervalMs"], 250);
    assert_eq!(
        fixture.count("tasks/get"),
        0,
        "a task receipt must not start an implicit polling loop"
    );

    let working = workspace.run_json(&["--json", "tasks", "get", "fixture", "task-media"]);
    assert_eq!(working["resultType"], "complete");
    assert_eq!(working["taskId"], "task-media");
    assert_eq!(working["status"], "working");
    assert_eq!(working["createdAt"], CREATED_AT);
    assert_eq!(working["pollIntervalMs"], 250);
    assert!(working.get("result").is_none());
    assert_eq!(
        fixture.count("tasks/get"),
        1,
        "tasks/get must reach the server even with a warm metadata cache"
    );

    let completed = workspace.run_json(&["--json", "tasks", "get", "fixture", "task-media"]);
    assert_eq!(completed["resultType"], "complete");
    assert_eq!(completed["taskId"], "task-media");
    assert_eq!(completed["status"], "completed");
    assert_eq!(completed["statusMessage"], "ready");
    assert_eq!(completed["createdAt"], CREATED_AT);
    assert_eq!(completed["lastUpdatedAt"], UPDATED_AT);
    assert_eq!(completed["ttlMs"], 600_000);
    assert_eq!(completed["pollIntervalMs"], 250);
    assert_eq!(
        fixture.count("tasks/get"),
        2,
        "task state must not be cached between invocations"
    );

    let block = &completed["result"]["content"][0];
    assert_eq!(block["type"], "image");
    assert_eq!(block["mimeType"], "image/png");
    assert!(
        block["data"].is_object(),
        "completed media data must be a path descriptor, not base64: {block}"
    );
    let path = PathBuf::from(
        block["data"]["path"]
            .as_str()
            .expect("completed image path"),
    );
    assert!(path.is_absolute(), "image path {path:?} must be absolute");
    assert_eq!(
        std::fs::read(&path).expect("completed media survives the cm process"),
        PNG_BYTES
    );
    std::fs::remove_file(&path).expect("remove task image artifact");

    let cancel_receipt = workspace.run_json(&[
        "--json",
        "--tasks",
        "tools",
        "call",
        "fixture_cancel_task",
        "{}",
    ]);
    assert_eq!(cancel_receipt["taskId"], "task-cancel");
    let cancel = workspace.run(&["--json", "tasks", "cancel", "fixture", "task-cancel"]);
    assert_eq!(
        cancel.code,
        Some(0),
        "tasks cancel failed\nstdout:\n{}\nstderr:\n{}",
        cancel.stdout,
        cancel.stderr
    );
    assert_eq!(
        fixture.count("tasks/get"),
        2,
        "cancellation must not poll or retry"
    );
    let cancel_request = fixture
        .records()
        .into_iter()
        .find(|record| record.method == "tasks/cancel")
        .expect("tasks/cancel reached the fixture");
    assert_eq!(cancel_request.params["taskId"], "task-cancel");

    fixture.assert_modern_protocol(0);
}

#[test]
fn tasks_are_opt_in_and_failures_are_meaningful() {
    let fixture = Fixture::start();
    let workspace = Workspace::new();
    workspace.configure(&fixture.endpoint);

    let unexpected = workspace.run(&["--json", "tools", "call", "fixture_media", "{}"]);
    unexpected.assert_failed();
    unexpected.assert_stdout_empty();
    assert_eq!(
        fixture.count("tasks/get"),
        0,
        "without --tasks the client must not retain or poll a task"
    );

    let receipt = workspace.run_json(&[
        "--json",
        "--tasks",
        "tools",
        "call",
        "fixture_tool_error",
        "{}",
    ]);
    assert_eq!(receipt["resultType"], "task");
    assert_eq!(receipt["taskId"], "task-error");

    let tool_error = workspace.run(&["--json", "tasks", "get", "fixture", "task-error"]);
    tool_error.assert_failed();
    let envelope: Value = serde_json::from_str(&tool_error.stdout)
        .expect("a nonzero completed task still emits its full envelope");
    assert_eq!(envelope["resultType"], "complete");
    assert_eq!(envelope["taskId"], "task-error");
    assert_eq!(envelope["status"], "completed");
    assert_eq!(envelope["result"]["isError"], true);

    let failed = workspace.run(&["--json", "tasks", "get", "fixture", "task-failed"]);
    failed.assert_failed();
    let envelope: Value =
        serde_json::from_str(&failed.stdout).expect("a failed task emits its full envelope");
    assert_eq!(envelope["status"], "failed");
    assert_eq!(envelope["error"]["code"], -32603);

    let requested = workspace.run(&["tasks", "get", "fixture", "task-input"]);
    requested.assert_failed();
    requested.assert_stdout_empty();
    assert_eq!(
        fixture.count("tasks/update"),
        0,
        "unsupported input must never be answered or synthesized"
    );

    let opt_in_from = fixture
        .records()
        .iter()
        .position(|record| {
            record
                .params
                .get("_meta")
                .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
                .and_then(|capabilities| capabilities.get("extensions"))
                .is_some()
        })
        .expect("the first opted-in request was recorded");
    fixture.assert_modern_protocol(opt_in_from);
}
