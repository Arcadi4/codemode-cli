# codemode-cli

`cm` is a Rust MCP application host for shell-access agents. Discover capabilities on demand, invoke them from bash, and use tools such as `jq` to process results without loading a server's entire tool catalog into model context.

Only [MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) is supported. Connections are created when needed and closed when the command finishes. There is no daemon, initialize handshake, session reuse, elicitation, or deprecated-feature support. Stdio and modern Streamable HTTP work; HTTP responses may be JSON or request-scoped SSE. Notification subscriptions are not opened.

## Install and start

Requires Rust 1.88 or newer.

```bash
cargo install --path .
cm skills read core
cm servers list
cm tools list --server docs
cm tools discover docs_search
cm tools call docs_search '{"query":"MCP caching"}' | jq '.results[:3]'
```

The last three commands assume a configured `docs` server exposing `search`; use the names and schemas returned by your server. The built-in `core` skill is the short agent guide. Retrieve exact syntax and options through `cm --help` and `<command> --help` rather than loading exhaustive instructions up front.

Tools and prompts use `<server>_<name>` identifiers. `--server` disambiguates names containing underscores. Resource names are labels, not identities: resource discovery, schema inspection, and reading take a server plus URI, so two resources may share a display name. Resource templates and prompt/resource argument completion have their own subcommands. `discover` includes the complete definition and server instructions; `schema` returns declared schemas/arguments or URI/MIME metadata where MCP does not define JSON Schemas.

## Configuration

The per-user global file uses FastMCP's [`mcpServers` JSON format](https://gofastmcp.com/integrations/mcp-json-configuration). Its location is `$XDG_CONFIG_HOME/codemode/mcp.json`, falling back to `~/.config/codemode/mcp.json`. `CM_CONFIG` overrides that file for automation. No project-local `cm` file is read or created.

Supported sources are VS Code, GitHub Copilot, Cursor, OpenCode, Claude Code/Desktop, Codex, and Devin. Their user and applicable workspace files are discovered automatically:

```bash
cm import cursor
cm import --all
cm import codex --path /path/to/config.toml
```

The global config takes precedence; different same-name definitions from multiple clients require an explicit global choice. Identical definitions are deduplicated. Disabled definitions remain disabled. Conflicting imports fail without changing the global file; `--force` explicitly permits replacement. Source client files are never edited. Global writes are atomic and owner-only.

Missing configuration files are skipped silently, including explicit import paths. Invalid or unreadable files produce stderr warnings without blocking valid sources. Imports with no usable source are no-ops; an invalid global destination is left unchanged, including with `--force`.

Named VS Code profiles are discovered alongside the default profile. Different definitions of the same name in independent profiles are not arbitrarily merged: use a global choice or import one profile explicitly with `--path`. An applicable workspace definition overrides profile-level definitions.

`CM_CLIENTS` limits automatic discovery to comma-separated client names; `CM_CLIENTS=none` reads only the global config. `cm servers list` inspects names, transports, and source paths without launching servers or printing credentials. Only trusted configurations should be used: starting a stdio server executes its configured command. Imports do not transfer client-specific tool approvals or permission policies.

Environment/header credentials and supported environment/file references are resolved on demand. Unexpired, endpoint-bound OAuth tokens can be reused from Claude Code, Codex, and OpenCode file stores. Name-only token records are not trusted. Interactive OAuth and executable header helpers are not used; configure static headers/environment credentials when a source cannot expose its token.

On macOS, `--native-credentials` also permits read-only access to endpoint-bound MCP OAuth grants in the Claude Code and Codex keychains and the VS Code and Cursor encrypted stores. This flag is off by default; the OS may request access approval. Explicit Authorization headers and bearer-token environment variables take precedence. Expired grants require signing in again with the source client. GitHub login sessions, Devin account tokens, and Copilot CLI's separate native vault are not reused.

## Shell contract

- Stdout contains data, never banners or logs. Catalogs, schemas, and discovery are JSON. Tool structured output is JSON; unstructured text is printed directly. Distinct structured/content output retains its full envelope rather than losing content.
- Tool/prompt arguments accept a JSON object, `@file.json`, or `-` for stdin. Prompt/completion argument values must be strings. Tool input and declared output schemas are validated. JSON Schema drafts 4, 6, 7, 2019-09, and 2020-12 are supported; the default is 2020-12. External schema references are not fetched.
- `--json` preserves complete result envelopes. Image/audio `data` and resource `blob` fields become `{ "path": "/absolute/file" }` descriptors. Other fields remain intact. Artifacts have owner-only permissions and survive command exit; remove them when no longer needed. Links are not downloaded, and returned HTML is never executed.
- Diagnostics and server stderr stay on stderr. Exit status is `0` for success, `1` for operational/tool errors, `2` for usage errors, `130` for SIGINT, and `143` for SIGTERM. Broken pipes terminate quietly. A timeout/interruption does not prove a side effect did not execute; interrupted calls are not automatically replayed.
- `--timeout` bounds each request, including startup/probing; a shorter source-client timeout still applies. `--max-response-bytes` defaults to 64 MiB of encoded response data and can be raised for large media. Stdio processes receive EOF and are reaped, with termination escalation if necessary.

## Asynchronous Tasks

For servers advertising the [Tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview), `--tasks` permits `tools call` to return a durable task handle instead of an inline result. The server chooses which form to return. Use `cm tasks get SERVER TASK_ID` to retrieve state and the eventual result, or `cm tasks cancel SERVER TASK_ID` to request cooperative cancellation. These commands explicitly opt into the extension; it is otherwise disabled.

Task handles survive separate CLI invocations. Keep the returned ID and respect `pollIntervalMs` in shell polling loops. Task state/results are not cached, no polling happens in the background, and cancellation acknowledgement does not prove work stopped. Completed tool errors still exit nonzero. Tasks requesting elicitation or deprecated client input are rejected rather than fulfilled. Only the 2026 Tasks methods are supported, not legacy task/session APIs.

## Cache

Only protocol-designated discovery/list/resource-read results are persisted. Server `ttlMs` and `cacheScope` hints govern freshness; missing, zero, or negative TTLs are not reused. Each pagination cursor has a separate cache entry. Entries are isolated by resolved configuration, authorization, environment, effective directory, protocol version, and declared client capabilities. Tool calls, prompt retrieval, intermediate input requests, and errors are never cached.

The private cache is `$XDG_CACHE_HOME/codemode`, falling back to `~/.cache/codemode`; `CM_CACHE_DIR` overrides it. `--refresh` bypasses fresh entries and replaces them with new server responses. Cache corruption is treated as a miss. Server-provided instructions are exposed through discovery; callers remain responsible for following them and obtaining authorization for side effects.
