"""Select an isolated acceptance runtime and bind direct native HTTP requests.

Clean release measurements use the verified canonical pair in the fixture's
KIN_HOME. Debug or dirty pairs remain explicit Development measurements.
This helper is opt-in, so legacy-entrypoint refusal controls stay unmodified.
"""

import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import urllib.parse
import urllib.request


IDENTITY_HEADER = "x-kin-runtime-identity"
REPOSITORY_IDENTITY_HEADER = "x-kin-repository-identity"
IDENTITY_FIELDS = (
    "version", "commit_sha", "source_dirty", "source_known", "target", "build_profile",
)
BUILD_FIELDS = (
    "kin_version", "kin_commit", "kin_dirty", "kin_source_known", "kin_target",
    "kin_build_profile",
)


def _identity(value):
    if not isinstance(value, dict):
        raise ValueError("runtime identity is not an object")
    result = {key: value.get(key) for key in IDENTITY_FIELDS}
    for key in ("version", "commit_sha", "target", "build_profile"):
        if not isinstance(result[key], str) or not result[key]:
            raise ValueError("runtime identity lacks " + key)
    for key in ("source_dirty", "source_known"):
        if type(result[key]) is not bool:
            raise ValueError("runtime identity lacks boolean " + key)
    return result


def _probe(command, env, cwd=None):
    result = subprocess.run(command, env=env, cwd=cwd, capture_output=True,
                            text=True, timeout=30)
    if result.returncode:
        raise RuntimeError("runtime metadata probe failed: %s" %
                           (result.stderr.strip() or result.stdout.strip()))
    return json.loads(result.stdout)


def selected_cli_identity(kin, env, cwd=None):
    metadata = _probe([str(kin), "bench-meta", "--json", "--runtime-identity-only"],
                      env, cwd)
    if metadata.get("schema") != "kin.runtime-build-identity.v1":
        raise ValueError("unsupported selected CLI runtime metadata")
    return _identity(dict(zip(IDENTITY_FIELDS,
                              (metadata.get(key) for key in BUILD_FIELDS))))


def runtime_fingerprint(identity, mode, namespace):
    identity = _identity(identity)
    if mode not in ("managed", "development"):
        raise ValueError("unsupported runtime mode")
    if not isinstance(namespace, str) or not namespace:
        raise ValueError("runtime service has no namespace token")
    # Rust serializes struct fields in declaration order, not JSON map order.
    body = json.dumps(identity, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(body + b"\0" + mode.encode() + b"\0" +
                          namespace.encode()).hexdigest()


def runtime_namespace(kin_home):
    """Capture the Unix directory identity used by the native HTTP suites."""
    if os.name != "posix":
        raise RuntimeError("native HTTP acceptance namespace capture requires Unix")
    home = Path(kin_home)
    canonical = home.resolve(strict=True)
    descriptor = os.open(home, os.O_RDONLY | os.O_DIRECTORY)
    try:
        observed = os.fstat(descriptor)
        current = home.stat()
        if (home.resolve(strict=True) != canonical
                or (current.st_dev, current.st_ino) != (observed.st_dev, observed.st_ino)):
            raise ValueError("selected fixture home changed during namespace capture")
        directory = observed.st_dev.to_bytes(8, "little") + observed.st_ino.to_bytes(8, "little")
        return hashlib.sha256(os.fsencode(canonical) + b"\0" + directory).hexdigest()
    finally:
        os.close(descriptor)


def repository_fingerprint(repository):
    """Match the native repository claim's independently pinned wire digest."""
    if not isinstance(repository, dict):
        raise ValueError("local runtime has no repository claim")
    fields = [repository.get(key) for key in ("repository_id", "store_namespace")]
    if any(not isinstance(value, str) or not value.strip() for value in fields):
        raise ValueError("local runtime repository claim lacks a nonempty identity")
    wire = json.dumps(["kin.runtime-repository.v1", *fields], ensure_ascii=False,
                      separators=(",", ":")).encode("utf-8")
    return hashlib.sha256(wire).hexdigest()


def _selected_repository_identity(kin, env, cwd):
    """Read only the selected fixture's store config, never adopt a peer's root."""
    selected_cwd = Path(cwd or os.getcwd()).resolve(strict=True)
    layout = _probe([str(kin), "store", "path", "--json"], env, str(selected_cwd))
    store = layout.get("store") if isinstance(layout, dict) else None
    if (not isinstance(layout, dict) or layout.get("schema") != "kin.store-path.v1"
            or not isinstance(store, str) or not store
            or not isinstance(layout.get("working_dir"), str)
            or Path(layout["working_dir"]).resolve(strict=True) != selected_cwd):
        raise ValueError("selected CLI did not bind the intended fixture repository store")
    store = Path(store)
    descriptor = os.open(store, os.O_RDONLY | os.O_DIRECTORY)
    try:
        retained = os.fstat(descriptor)
        namespace = runtime_namespace(store)
        observed = store.stat()
        if (observed.st_dev, observed.st_ino) != (retained.st_dev, retained.st_ino):
            raise ValueError("selected fixture store changed before manifest capture")
        manifest_descriptor = os.open("manifest.json", os.O_RDONLY, dir_fd=descriptor)
        with os.fdopen(manifest_descriptor, "r", encoding="utf-8") as stream:
            manifest = json.load(stream)
        repository = {"repository_id": manifest.get("repo_id") if isinstance(manifest, dict) else None,
                      "store_namespace": namespace}
        repository_fingerprint(repository)
        current = store.stat()
        if (runtime_namespace(store) != namespace
                or (current.st_dev, current.st_ino) != (retained.st_dev, retained.st_ino)):
            raise ValueError("selected fixture store changed during manifest capture")
        return repository
    finally:
        os.close(descriptor)


def _sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def prepare_runtime_fixture(kin, daemon, env, kin_home):
    """Return selected paths, leaving the operator's home and actors untouched."""
    kin = Path(kin).resolve(strict=True)
    daemon = Path(daemon or kin.with_name("kin-daemon" + kin.suffix)).resolve(strict=True)
    home = Path(kin_home).resolve()
    home.mkdir(parents=True, exist_ok=True)
    env["KIN_HOME"] = str(home)
    cli_identity = selected_cli_identity(kin, env)
    daemon_metadata = _probe([str(daemon), "--compat-json"], env)
    daemon_identity = _identity(daemon_metadata.get("runtime_identity"))
    if daemon_identity != cli_identity:
        raise ValueError("acceptance CLI and daemon have different build identities")
    managed = bool(cli_identity["source_known"] and not cli_identity["source_dirty"]
               and cli_identity["build_profile"] == "release"
               and re.fullmatch(r"[0-9a-f]{40}", cli_identity["commit_sha"]))
    if cli_identity["build_profile"] == "release" and not cli_identity["source_dirty"] and not managed:
        raise ValueError("clean release qualification needs a known full source identity")
    mode = "managed" if managed else "development"
    env["KIN_RUNTIME_MODE"] = mode
    hashes = {"cli": _sha256(kin), "daemon": _sha256(daemon)}
    if managed:
        binary_dir = home / "bin"
        binary_dir.mkdir(exist_ok=True)
        selected = []
        for source, name, digest in ((kin, "kin" + kin.suffix, hashes["cli"]),
                                     (daemon, "kin-daemon" + daemon.suffix, hashes["daemon"])):
            target = binary_dir / name
            if target.exists() or target.is_symlink():
                if target.is_symlink() or _sha256(target) != digest:
                    raise ValueError("fixture canonical runtime already differs: " + str(target))
            else:
                shutil.copy2(source, target)
            if _sha256(target) != digest:
                raise ValueError("fixture canonical copy changed runtime bytes")
            selected.append(target)
        kin, daemon = selected
        env["KIN_DAEMON_BIN"] = str(daemon)
        status = _probe([str(kin), "runtime", "status", "--json"], env)
        if (status.get("schema") != "kin.runtime-status.v1"
                or status.get("mode") != "managed" or status.get("ready") is not True
                or status.get("pair_matches") is not True):
            raise ValueError("fixture canonical runtime is not Managed ready")
        for key, path in (("cli", kin), ("daemon", daemon)):
            if (status.get(key, {}).get("sha256") != hashes[key]
                    or Path(status.get(key, {}).get("path", "")) != path
                    or _identity(status.get(key, {})) != cli_identity):
                raise ValueError("canonical status differs from the independently probed pair")
    else:
        env["KIN_DAEMON_BIN"] = str(daemon)
    receipt = {"schema": "kin.acceptance-runtime-fixture.v1", "mode": mode,
               "identity": cli_identity, "kin_home": str(home),
               "cli": str(kin), "daemon": str(daemon), "sha256": hashes,
               "release_kind": "unknown" if managed else "development"}
    (home / "acceptance-runtime.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return str(kin), str(daemon)


def native_request_headers(kin, env, endpoint, token=None, cwd=None, timeout=5):
    """Pin source, home and the intended repository before a local request."""
    address = urllib.parse.urlsplit(endpoint)
    if address.scheme != "http" or address.hostname not in ("127.0.0.1", "::1", "localhost"):
        raise ValueError("native runtime headers are only for controlled loopback services")
    expected = selected_cli_identity(kin, env, cwd)
    namespace = runtime_namespace(env["KIN_HOME"])
    path = address.path
    if path.startswith("/v2/"):
        path = path[3:]
    diagnostic = path in ("/health", "/ready", "/readiness", "/auth/rotation",
                          "/runtime/identity")
    repository = None if diagnostic else _selected_repository_identity(kin, env, cwd)
    headers = {"Authorization": "Bearer " + token} if token else {}
    identity_url = urllib.parse.urlunsplit((address.scheme, address.netloc,
                                          "/runtime/identity", "", ""))
    request = urllib.request.Request(identity_url, headers=headers)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        service = json.load(response)
    if service.get("schema") != "kin.runtime-service.v1" or service.get("component") != "daemon":
        raise ValueError("unsupported local daemon runtime metadata")
    if _identity(service.get("identity")) != expected:
        raise ValueError("native service differs from the independently selected CLI")
    mode = service.get("mode")
    if mode != env.get("KIN_RUNTIME_MODE"):
        raise ValueError("native service uses a different runtime mode")
    if service.get("namespace") != namespace or runtime_namespace(env["KIN_HOME"]) != namespace:
        raise ValueError("native service uses a different or replaced fixture namespace")
    headers[IDENTITY_HEADER] = runtime_fingerprint(expected, mode, namespace)
    if not diagnostic:
        claimed = service.get("repository")
        repository_fingerprint(claimed)
        if claimed != repository:
            raise ValueError("native service uses a different fixture repository or store")
        if _selected_repository_identity(kin, env, cwd) != repository:
            raise ValueError("intended fixture repository or store changed during metadata capture")
        headers[REPOSITORY_IDENTITY_HEADER] = repository_fingerprint(repository)
    return headers


def fixture_daemon_metadata(kin, env, cwd=None):
    """Identify an opt-in controlled HTTP peer used by consumer contract tests."""
    mode = env["KIN_RUNTIME_MODE"]
    if mode not in ("managed", "development"):
        raise ValueError("unsupported fixture runtime mode")
    return {"schema": "kin.runtime-service.v1", "component": "daemon",
            "identity": selected_cli_identity(kin, env, cwd), "mode": mode,
            "namespace": runtime_namespace(env["KIN_HOME"]), "pid": os.getpid(),
            "repository": _selected_repository_identity(kin, env, cwd) if cwd is not None else None}
