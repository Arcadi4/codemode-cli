# codemode-cli

You don't need a JS REPL for codemode. `cm` brings [codemode](https://blog.cloudflare.com/code-mode) to shell directly.

`cm` is also useful for agents to debug MCPs by not directly loading them in the harness. This allows hot-reload so agents see changes immediately and prevents cache invalidation.

Only [MCP 2026-07-28](https://modelcontextprotocol.io/specification/2026-07-28) or later is supported.

`cm` is stateless on the connection layer. It keeps no daemon or persistent session.

## Install and start

`cm` is not published to crates.io yet. Install it from the repository:

```bash
cargo install --git https://github.com/Arcadi4/codemode-cli --locked
```

`cm` reads the MCP servers you already have configured in other agents. It discovers the user and applicable workspace configuration files of:

- [VS Code](https://code.visualstudio.com/)
- [GitHub Copilot](https://github.com/features/copilot)
- [Cursor](https://cursor.com/)
- [OpenCode](https://opencode.ai/)
- [Claude Code](https://claude.com/product/claude-code)/[Desktop](https://claude.ai/download)
- [Codex](https://openai.com/codex/)
- [Devin](https://devin.ai/)

It also reads an optional per-user global file at `$XDG_CONFIG_HOME/codemode/mcp.json`, falling back to `~/.config/codemode/mcp.json`.

There is no project-local config file for `cm`. We would recommend using [`mcp.json`](https://gofastmcp.com/integrations/mcp-json-configuration).

Use `cm import <client>` only when you want to copy definitions into the global file:

```bash
cm import <client>
```

`CM_CONFIG` overrides the global file. `CM_CLIENTS=none` restricts discovery to it.
