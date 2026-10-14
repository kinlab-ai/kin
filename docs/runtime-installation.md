# One admitted runtime

Normal Kin commands use one managed CLI and daemon pair. Its root is `KIN_HOME`,
then `KIN_DIR`, then `~/.kin`; empty overrides are ignored. Both executables live
under that root's `bin` directory.

```sh
kin runtime status --json
```

This checks the installed executable pair without opening a repository or
starting a daemon. The `kin.runtime-status.v1` record reports the paths, versions,
exact source commits, dirty state, build targets and profiles, binary hashes,
recorded update channel, and pending restart acknowledgement. A refusal still
returns JSON and exits nonzero. `ready` requires known clean release identities
that agree, and no pending restart fence. Matching version numbers alone are
insufficient. A channel preference does not prove that a build was published.

An ordinary release executable found elsewhere on PATH delegates repository,
UI and MCP commands to the admitted managed CLI. It refuses when that pair is
missing or mismatched. `kin setup` writes clients against the admitted pair;
diagnostics and the native updater remain available to repair an installation.
The updater retains its existing transaction and restart acknowledgement fences.

## Development builds

Debug and dirty builds default to development mode. Development uses a separate
Kin home and service namespace, derived from the executable directory and build
identity. An explicit isolated `KIN_HOME` can select a test fixture; the ordinary
`~/.kin` home is refused. Development cannot perform global setup, doctor repairs,
or managed update mutations. Read-only checks and dry runs remain available.

```sh
kin runtime status --json --development --binary /path/to/build/kin
```

The selected CLI must have a matching sibling daemon. For an ordinary development
command, set `KIN_RUNTIME_MODE=development`; wrappers pass the admitted isolated
home to its children. Clean release builds default to managed mode, and explicitly
requesting managed mode never admits an unknown, dirty, or debug identity.
