# AGENTS.md

`codemode-cli` builds `cm`, a stateless MCP host for shell-native agent workflows. This file is the internal reference for building, testing, architecture, and behavioral contracts. The embedded agent guide is `skills/core/SKILL.md` and is served by `cm skills read core`.

## Build and test

Rust 1.88 or newer (edition 2024).

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

- `tests/cli.rs`, `tests/tasks.rs`, and `tests/config_loading.rs` are end-to-end tests. They drive `CARGO_BIN_EXE_cm` against an in-process `tiny_http` MCP fixture, clear the environment, and point `HOME`, `XDG_*`, `CM_CONFIG`, and `CM_CACHE_DIR` at a temp directory with `CM_CLIENTS=none`. Keep new tests equally hermetic.
- `tests/unit/{config,credentials,output}.rs` are unit tests compiled into the binary through `#[path]` declarations in `src/main.rs`; they can reach private items.
- There is no CI workflow; Dependabot updates Cargo dependencies weekly.

## Module map

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Process entry, exit codes, broken-pipe handling, test-module wiring |
| `src/cli.rs` | Clap command surface and global flags |
| `src/app.rs` | Command dispatch, signal exits, skill embedding |
| `src/config.rs` | Config loading/merging, server resolution, fingerprints, atomic global writes |
| `src/import.rs` | Client discovery, per-client parsers, draft-to-config conversion |
| `src/credentials.rs` | File-based and macOS native token stores |
| `src/host.rs` | Catalog aggregation, name resolution, capability/extension checks |
| `src/search.rs` | Ranked tool-metadata search and selected summary fields |
| `src/mcp.rs` | JSON-RPC client, discovery probe, pagination, cache integration |
| `src/transport.rs` | Stdio and Streamable HTTP transports, process lifecycle |
| `src/cache.rs` | TTL response cache |
| `src/schema.rs` | JSON Schema validation and complexity bounds |
| `src/output.rs` | Envelope rendering and binary artifacts |
| `src/tasks.rs` | Tasks extension validation and retrieval |

## Runtime and protocol

- Each invocation is one process with no daemon or persistent session. `Host` creates a `Client` lazily per server on first use and closes every stdio child before exit.
- Connections are opened only when needed. A stdio client probes with `server/discover` before any other request, verifies `2026-07-28`, and never falls back to the legacy `initialize` handshake.
- HTTP uses modern Streamable HTTP only: no session/resume headers, redirects and automatic retries disabled, and `application/json` or `text/event-stream` responses. Cancellation on HTTP is dropping the response stream, never a cancellation POST.
- Stdio servers inherit stderr, run in their own Unix process group, and are killed on drop. Closing drops stdin for a real EOF, waits one second, sends `SIGTERM` to the group, waits one second, then sends `SIGKILL`. Request cancellation is a best-effort `notifications/cancelled` with a 200 ms cap.
- Unscoped catalogs treat servers independently: a failed server logs to stderr, its client/discovery state is dropped, and partial pages from it are discarded. Targeted operations propagate the error. Duplicate identifiers within one server are errors; ambiguous cross-server names stay listed with a warning.
- If a server rejects a cached pagination cursor with `-32602`, catalog discovery invalidates the cached pages and restarts once from the first page.

## Configuration and imports

### Files and precedence

- The global file uses FastMCP's `mcpServers` JSON format at `$XDG_CONFIG_HOME/codemode/mcp.json`, falling back to `~/.config/codemode/mcp.json`. `CM_CONFIG` overrides it. No project-local `cm` file is read or created.
- Supported sources are VS Code, GitHub Copilot, Cursor, OpenCode, Claude Code/Desktop, Codex, and Devin. Their user and applicable workspace files are discovered automatically:
  ```bash
  cm import cursor
  cm import --all
  cm import codex --path /path/to/config.toml
  ```
- The global config takes precedence. Ambiguous same-name definitions are excluded from automatic discovery until a global choice is supplied. Identical definitions are deduplicated. Disabled definitions remain disabled.
- Imports skip ambiguous source servers, but conflicts with existing global definitions fail without changing the global file; `--force` explicitly permits replacement. Source client files are never edited. Global writes are owner-only.
- Missing configuration files are skipped silently, including explicit import paths. Invalid or unreadable files produce stderr warnings without blocking valid sources. Invalid server definitions are skipped without discarding healthy entries in the same file. Imports with no usable source are no-ops; an invalid global destination is left unchanged, including with `--force`.
- Named VS Code profiles are discovered alongside the default profile. Different definitions of the same name in independent profiles are not arbitrarily merged: use a global choice or import one profile explicitly with `--path`. An applicable workspace definition overrides profile-level definitions.
- `CM_CLIENTS` limits automatic discovery to comma-separated client names; `CM_CLIENTS=none` reads only the global config. `cm servers list` inspects names, transports, and source paths without launching servers or printing credentials.
- Only trusted configurations should be used: starting a stdio server executes its configured command. Imports do not transfer client-specific tool approvals or permission policies.

### Credentials

- Environment/header credentials and supported environment/file references are resolved on demand.
- Unexpired, endpoint-bound OAuth tokens can be reused from Claude Code, Codex, and OpenCode file stores. Name-only token records are not trusted.
- Interactive OAuth and executable header helpers are not used; configure static headers/environment credentials when a source cannot expose its token.
- On macOS, `--native-credentials` also permits read-only access to endpoint-bound MCP OAuth grants in the Claude Code and Codex keychains and the VS Code and Cursor encrypted stores. This flag is off by default; the OS may request access approval.
- Explicit `Authorization` headers and bearer-token environment variables take precedence. Expired grants require signing in again with the source client. GitHub login sessions, Devin account tokens, and Copilot CLI's separate native vault are not reused.

### Implementation

- The global file is parsed as JSON5 so JSONC is accepted, with a 64 MiB cap. Writes take an exclusive `flock` on `.<file>.lock`, secure the `codemode` directory to `0700`, write and fsync a `0600` temp file, rename it into place, then fsync the parent directory.
- Import enumeration lives in `import::Client` and `SourceSpec`; `Parser`/`Syntax` cover JSON/JSONC/TOML sources. Drafts convert to `ServerConfig` before merging.
- `ResolveOptions` snapshots environment and working directory. Environment/file references and credentials resolve on demand, and the cache fingerprint binds results to that resolution.

## Shell contract

- Stdout contains data, never banners or logs. Catalogs, schemas, and discovery are JSON. Tool structured output is JSON; unstructured text is printed directly. Distinct structured/content output retains its full envelope rather than losing content.
- Tool/prompt arguments accept a JSON object, `@file.json`, or `-` for stdin. Prompt/completion argument values must be strings. Tool input and declared output schemas are validated. JSON Schema drafts 4, 6, 7, 2019-09, and 2020-12 are supported; the default is 2020-12. External schema references are not fetched.
- `--json` preserves complete result envelopes. Image/audio `data` and resource `blob` fields become `{ "path": "/absolute/file" }` descriptors. Other fields remain intact. Artifacts have owner-only permissions and survive command exit; remove them when no longer needed. Links are not downloaded, and returned HTML is never executed.
- Diagnostics and server stderr stay on stderr. Exit status is `0` for success, `1` for operational/tool errors, `2` for usage errors, `130` for SIGINT, and `143` for SIGTERM. Broken pipes terminate quietly. A timeout/interruption does not prove a side effect did not execute; interrupted calls are not automatically replayed.
- Unscoped tool, prompt, resource, and template catalogs skip failed servers with stderr diagnostics and return usable results, or `[]`, with status `0`. `--server` and targeted operations still fail when their target fails. Ambiguous names remain visible in lists; use `--server` to select the source.
- `--server` is the selector for server-targeted commands. URI operations, server discovery, and task commands require it; missing selectors are usage errors before config or stdin work. Scoped tool/prompt operations accept native or qualified names and reject multiple matches. Unscoped names remain qualified. Server inventory can include a selected disabled definition without starting it.
- `cm tools search QUERY` matches all whitespace-separated terms case-insensitively across names, titles, and descriptions. Exact names rank before prefixes, substrings, and metadata-only matches; names and servers break ties. `--limit` defaults to 20 and must be positive. `--fields` selects `name,server,title,description`; defaults are `name,server,description`. Full schemas remain in `tools schema`/`discover`.
- `--timeout` bounds each request, including startup/probing; a shorter source-client timeout still applies. `--max-response-bytes` defaults to 64 MiB of encoded response data and can be raised for large media.

## Cache

- Only protocol-designated discovery/list/resource-read results are persisted: `server/discover`, `tools/list`, `prompts/list`, `resources/list`, `resources/templates/list`, and `resources/read`. Params carrying `inputResponses` or `requestState` are never cacheable.
- Server `ttlMs` and `cacheScope` hints govern freshness; missing, zero, or negative TTLs are not reused. Only `resultType: complete`, non-`isError` results with a positive TTL and `cacheScope` of `public` or `private` are stored.
- Tool calls, prompt retrieval, intermediate input requests, errors, and task results are never cached.
- Entry paths hash `cm-cache-v1`, the partition, the method, and the JSON params. Each pagination cursor is a separate entry. The partition hashes `cm:<protocol>:<crate version>:<resolved server fingerprint>:tasks=<bool>`.
- The resolved server fingerprint hashes the fingerprint version, server name, transport details (stdio command/args/env/cwd or HTTP URL/lowercased headers), timeout, explicitly resolved environment, the full process environment, and the effective working directory.
- The private cache is `$XDG_CACHE_HOME/codemode`, falling back to `~/.cache/codemode`; `CM_CACHE_DIR` overrides it. `--refresh` bypasses fresh entries and replaces them with new server responses.
- Cache files are written via `tempfile` persist in a directory secured to `0700` on Unix. Cache corruption is treated as a miss; read errors other than "not found" produce a warning and fall through to the server.
- Server-provided instructions are exposed through discovery; callers remain responsible for following them and obtaining authorization for side effects.

## Asynchronous tasks

- For servers advertising the [Tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview), `--tasks` permits `tools call` to return a durable task handle instead of an inline result. The server chooses which form to return.
- Use `cm tasks get TASK_ID --server SERVER` to retrieve state and the eventual result, or `cm tasks cancel TASK_ID --server SERVER` to request cooperative cancellation. These commands explicitly opt into the extension; it is otherwise disabled.
- Task handles survive separate CLI invocations. Keep the returned ID and respect `pollIntervalMs` in shell polling loops; `cm` does not poll in the background. Cancellation acknowledgement does not prove work stopped.
- Completed tool errors still exit nonzero. Tasks requesting elicitation or deprecated client input are rejected rather than fulfilled. Only the 2026 Tasks methods are supported, not legacy task/session APIs.
- Only `io.modelcontextprotocol/tasks` and its `tasks/get`/`tasks/cancel` methods are supported. Receipts must have `resultType: task`; `input_required` is rejected because `cm` never fulfills elicitation, sampling, or roots.

## Output, schemas, and validation

- JSON-RPC uses `rmcp`'s untyped `JsonRpcMessage<Value, Value, Value>` envelopes. Typed `ServerResult` shapes can overlap and lose `isError`.
- Rendering selects behavior from the originating operation (`OutputKind`), never from ambiguous extension fields. `--json` and task receipts print the full envelope. Base64 payloads stream into temp artifacts with MIME-derived extensions; text without a trailing newline gets one.
- Schema validation uses `jsonschema` with network and file retrieval disabled (default features off). The default draft is 2020-12, external references fail closed, and complexity is capped at depth 64 and 20,000 nodes. Validation errors cite paths but never echo argument values.
