# codemode-cli

`cm` reads MCP server configurations from existing clients and imports them into a per-user global configuration. It never creates a project-local `cm` config or edits the source client's files.

## Install

Requires Rust 1.88 or newer.

```bash
cargo install --path .
cm servers list
cm import cursor
cm import --all
```

Use `cm import --help` for source selection and overwrite options. `--path FILE` imports an explicit source file. Conflicting definitions fail without modifying the global config; `--force` explicitly replaces them.

## Configuration

The global file uses FastMCP's [`mcpServers` JSON format](https://gofastmcp.com/integrations/mcp-json-configuration). Its location is `$XDG_CONFIG_HOME/codemode/mcp.json`, falling back to `~/.config/codemode/mcp.json`. `CM_CONFIG` overrides that file for automation.

Supported sources are VS Code, GitHub Copilot, Cursor, OpenCode, Claude Code/Desktop, Codex, and Devin. Their user and applicable workspace files are discovered automatically. The global config takes precedence; different same-name definitions from multiple clients require an explicit global choice. Identical definitions are deduplicated. Disabled definitions remain disabled.

`CM_CLIENTS` limits discovery to comma-separated client names; `CM_CLIENTS=none` reads only the global config. Configuration files and imported credentials are written atomically with owner-only permissions. Only trusted configurations should be used: starting an MCP server executes its configured command.

Environment/header credentials and supported environment/file references are resolved on demand. Unexpired, endpoint-bound OAuth tokens can be reused from Claude Code, Codex, and OpenCode file stores. Name-only token records are not trusted. Interactive OAuth, encrypted application vaults, and executable header helpers are not used; configure static headers/environment credentials when a source cannot expose its token. Imports do not transfer client-specific tool approvals or permission policies.

Use `cm servers list` to inspect names, transports, and source paths without launching servers or printing credential values. Only stdio and modern Streamable HTTP definitions are accepted; deprecated SSE definitions are not silently converted.
