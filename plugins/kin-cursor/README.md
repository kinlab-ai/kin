# Kin for Cursor

Kin is a graph-native code repository for AI agents and the people who build with
them. Its MCP server lets your agent look up callers, references, history and what a
change might affect, from the same record you inspect with the kin CLI.

## Install

Add the MCP server by hand. See the [repository README](https://github.com/firelock-ai/kin#use-it-from-claude-code-codex-or-cursor)
for the surrounding context.

Add this to `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "kin": { "command": "npx", "args": ["-y", "@kinlab/kin", "mcp", "start"] }
  }
}
```

This is the entry `kin doctor` checks. `kin setup --intent agent` writes the same server
for you, pointing at your installed `kin` binary instead of `npx`.

## The MCP server and its tools

The plugin registers Kin's MCP server, which runs `npx -y @kinlab/kin-mcp`. On first run
that downloads the matching Kin release for your platform, verifies its published SHA-256,
and serves the curated `agent-default` tool profile. Until that first download finishes,
the server lists one tool, `kin_startup_status`, which reports its progress.

The tools worth knowing: `locate` finds parsed declarations by name, kind, and
language, or ranks code against a natural-language description using the vector index. `context` returns an entity with its callers and imports in one
call. `refs` and `neighbors` walk the reference graph. `trace`
returns the ordered chain a value travels. `impact` answers what a change can
reach. The full surface is documented in
[docs/mcp-tools.md](https://github.com/firelock-ai/kin/blob/main/docs/mcp-tools.md).

Every response names the graph state that produced it, and an empty result says whether the
absence can be trusted. A graph gap is reported as a gap rather than filled in from raw file
search.

## Before the tools can answer

Kin answers from a graph, so a repository has to be admitted first. Ask the agent to call
`init`, or run `npx -y @kinlab/kin init .` in the repository yourself: the plugin puts no
`kin` on your PATH, so `npx` runs it. Then run `npx -y @kinlab/kin embed` to build the vector
index that `locate` ranks against. The structural tools work as soon as admission
finishes.
[llms-install.md](https://github.com/firelock-ai/kin/blob/main/llms-install.md) is the
step-by-step version, written so an agent can follow it unattended.

## Requirements

Node 20 or newer for `npx`, and network access on the first run to fetch the Kin release.
macOS and Linux are supported. On Windows, use WSL2: native Windows x64 support is early,
and review workflows are not yet tested there.

Apache-2.0. Home: https://kinlab.ai
