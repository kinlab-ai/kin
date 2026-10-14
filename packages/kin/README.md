# @kinlab/kin

The canonical npm install surface for [Kin](https://github.com/kinlab-ai/kin), a
graph-native code repository for AI agents and the people who build with them.

Install it without root, without `sudo`, and without a writable global npm prefix. This
is the default path because it is the one that works everywhere, including the container
and locked-down developer box where the global install below is refused outright:

```sh
npx -y @kinlab/kin --version
export PATH="$HOME/.kin/bin:$PATH"
kin --version
```

The first line downloads the native `kin` and `kin-daemon` for your platform into
`~/.kin/bin`, verified against their published SHA-256. That install is persistent: `npx`
provisions the same managed binaries the global install does, so the second and third
lines are reading a real binary on disk, not re-downloading anything. `kin setup` writes
the `PATH` line into your shell profile, so you type the `export` once:

```sh
kin setup --intent agent
```

If your global npm prefix is writable, or you are root, `npm install -g` puts the
launcher on `PATH` for you and is the shorter route:

```sh
npm install -g @kinlab/kin
kin --version
kin setup --intent agent
```

Native Windows x86_64 support is early. Repository admission works: `kin init` imports a Git repository and publishes graph authority, and graph, lexical, and daemon-backed queries answer natively. The end-to-end install proof also runs agent setup on native Windows and gets graph-backed answers from the installed MCP server. Transparent filesystem projection is not shipped on Windows, and review workflows are not yet tested there, so WSL2 remains the recommended path for the full Kin experience.
No native Windows ARM64 archive is published. An x64 Node process under Windows
emulation can provision the x86_64 archive. On native Windows, use WSL2 for the agent
setup documented below.

## If the global install is refused

On a machine whose global npm prefix is root-owned, and where you are not root and have no
`sudo`, `npm install -g` fails before Kin runs. This is why the install at the top of this
page leads with `npx`, which never touches that prefix:

```
npm error code EACCES
npm error syscall mkdir
npm error path /usr/local/lib/node_modules/@kinlab
npm error Error: EACCES: permission denied, mkdir '/usr/local/lib/node_modules/@kinlab'
```

npm fails here before it unpacks anything, so no Kin code has run and nothing in this
package can catch it or print a fix. Containers whose default user is not root are the
common case. Two ways out. The `npx` path at the top of this page needs no writable prefix
and installs the same native binaries under `~/.kin/bin`, which is why it is the default
rather than a fallback. Or move the npm prefix somewhere you own, and put it on your
`PATH`:

```sh
npm config set prefix ~/.npm-global
export PATH="$HOME/.npm-global/bin:$PATH"   # add this to your shell profile too
npm install -g @kinlab/kin
```

A user prefix is read by your interactive shell and by nothing else. Scripts, CI steps,
`docker exec`, and agent clients do not inherit it, so register `kin` with those by its
absolute path (`$(npm prefix -g)/bin/kin`, or `~/.kin/bin/kin` after `kin setup`) rather
than by name. Installing as root does not fix that on an image where every user shares one
`HOME`: root's npm reads the same `.npmrc`, reinstalls into the user prefix, and reports
success. Pass `npm install -g --prefix /usr/local @kinlab/kin` when you want the system
prefix.

## What it does

`@kinlab/kin` ships two thin launchers, not a JavaScript reimplementation:

- **`kin`**: provisions the managed native `kin` + `kin-daemon` release for your
  platform on first run (downloaded from the matching GitHub release, verified against
  its published SHA-256), installs it under `~/.kin/bin`, then passes every invocation
  straight through to the real binary and mirrors its exit.
- **`kin-mcp`**: compatibility entrypoint that starts Kin's MCP server
  (`kin mcp start`). MCP is one included mode of Kin, not a separate product.

The launcher and the shell installer (`scripts/install.sh`) share the same install
contract (`$KIN_HOME`, default `~/.kin`): either lane satisfies the other, and neither
downgrades an install the other made.

On macOS, Linux, or WSL2, run `kin setup --intent agent` after provisioning. Setup writes the Kin MCP server into
detected AI clients with the `agent-default` tool profile, adds the managed bin directory
to your shell profile, installs the shell/session hook, and records the install ledger used
by `kin setup status`, `kin doctor --fix`, and `kin setup uninstall`. On native Windows,
provisioning installs the CLI, `kin init` and graph queries run there, and the install proof
runs agent setup and graph-backed MCP tool calls there too. WSL2 remains the recommended
path for the full Kin experience, so run agent setup inside WSL2. The PowerShell installer
makes the same call and does not run setup at all.

## One managed runtime

Ordinary npm, installer and compatibility MCP launches use the same native pair
at `$KIN_HOME/bin` (`$KIN_DIR` is the compatibility alias; the default is `~/.kin`).
Before serving, the launcher reads `kin runtime status --json` and requires a clean
CLI and daemon with matching source commit, target and build profile. Equal version
strings or an old launcher stamp do not certify a pair. A missing daemon or a
differing pair stops with a repair instruction instead of selecting another binary
from PATH or a cache.

The package version is the minimum supported runtime. An older managed install is
updated through its own `kin update --apply` transaction when it supports the
runtime contract. Legacy releases without that contract use a privately staged
latest official stable release that meets the package minimum. Its native bootstrap
independently verifies published provenance
and commits the existing installation through the same native transaction. The
wrapper binds the official tag's source commit, archive checksum and platform;
it reports success only after the installed pair certifies that exact identity.
An incomplete migration retains its private stage and reports recovery or restart
as pending. npm and the installer do not replace an existing CLI and daemon
separately. A newer certified pair is reused. `KIN_LAUNCHER_ADOPT` is refused
because it would bypass that shared update authority.

Explicit development selections stay separate: `KIN_MANAGED_BIN` points at a
chosen binary and disables provisioning. The owner of that selection is responsible
for its matching daemon. Development binaries are not silently registered as the
normal managed install. Development launches derive a private Kin home and remove
inherited normal-home values. To supply an isolated home yourself, set
`KIN_RUNTIME_MODE=development` together with that separate `KIN_HOME`.

## Environment

| Variable | Effect |
| --- | --- |
| `KIN_HOME` | Root of the managed install (default `~/.kin`). |
| `KIN_DIR` | Compatibility alias, used only when `KIN_HOME` is unset. |
| `KIN_MANAGED_BIN` | Explicit development binary; disables provisioning. |
| `KIN_NO_PROVISION=1` | Resolve and certify an existing pair without network access. |

## MCP setup

`kin setup --intent agent` is the preferred MCP setup path. It writes an absolute path to
the managed native `kin` binary, so agents do not depend on inheriting your shell `PATH`.

Any MCP client can also use the included server manually:

```json
{
  "command": "npx",
  "args": ["-y", "@kinlab/kin", "mcp", "start"],
  "env": { "KIN_MCP_TOOL_PROFILE": "agent-default" }
}
```

`kin setup status` and `kin doctor` recognize this exact canonical wrapper topology instead
of flagging it for repair. Do not shorten `command` to a bare `kin`: agent clients do not
reliably inherit your shell `PATH`. Codex and Antigravity bindings also require
`"--repo", "/absolute/path/to/repository"` at the end of the argument vector; an Antigravity
workspace entry also uses that path as `cwd`.

`@kinlab/kin-mcp` remains published for existing configurations; new setups should use
this package.
