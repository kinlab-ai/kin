# Kin for Claude Code

Kin is a graph-native code repository for AI agents and the people who build with
them. Its MCP server lets your agent look up callers, references, history and what a
change might affect, from the same record you inspect with the kin CLI.

## Install

```
/plugin marketplace add kinlab-ai/kin
/plugin install kin@kin
```

## What it ships

The bundled MCP server runs `npx -y @kinlab/kin-mcp`, which downloads the matching Kin
release for your platform on first run, verifies its published SHA-256, and serves the
curated `agent-default` tool profile.

Three skills come with it. `kin-retrieval` teaches when to reach for `locate`,
`context`, and `refs` instead of grep and whole-file
reads. `blast-radius-review` builds a review workflow on `impact` and
`trace`, so a change is judged by what it can reach rather than by the lines it
moved. `kin-setup` walks a repository from nothing to a first graph-backed answer.

## Before the tools can answer

Kin answers from a graph, so a repository has to be admitted first. Run `kin init .` in the
repository, or set `KIN_MCP_AUTO_INIT=1` to let the server do it. Then run `kin embed` to
build the vector index that `locate` ranks against. The structural tools work as
soon as admission finishes. The `kin-setup` skill covers this end to end, and
[llms-install.md](https://github.com/kinlab-ai/kin/blob/main/llms-install.md) is the
unattended version.

Every response names the graph state that produced it, and an empty result says whether the
absence can be trusted. A graph gap is reported as a gap rather than filled in from raw file
search.

## Requirements

Node 20 or newer for `npx`, and network access on the first run to fetch the Kin release.
macOS and Linux are supported. On Windows, use WSL2: native Windows x64 support is early,
and review workflows are not yet tested there.

## Links

- Repository: https://github.com/kinlab-ai/kin
- Tool reference: https://github.com/kinlab-ai/kin/blob/main/docs/mcp-tools.md
- Home: https://kinlab.ai

Apache-2.0.
