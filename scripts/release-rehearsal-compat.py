#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Exercise candidate proof tooling against reviewed stable setup behaviour.

Prepare genuine v0.8.6 bytes from the independently verified retained release
archive, then use only that release's setup, status, doctor and provenance commands. Its
receipt is compatibility-baseline evidence, never candidate-binary qualification.
Git identity, shell and client configuration changes belong to a disposable
GitHub-hosted Windows account; persistent accounts are refused.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import zipfile

SCHEMA = "kin.release-rehearsal-compat.v1"
BASELINE = {
    "tag": "v0.8.6",
    "version": "0.8.6",
    "commit": "522ede0d0dfbdd33ff8ba3a5798bb3bed6902c08",
    "lock_sha256": "9db830e29224527847b5a122e0883a2c9f968d806e34b4bf4d5052c4ceb60495",
}
BASELINE_ARCHIVE = {
    "name": "kin-windows-x86_64.zip",
    "sha256": "b40bb7d72b2313dceaf806b76c58dd8c7b06cbf62a2a57cdd21d7a31f228e392",
    "components": {
        "kin.exe": {"sha256": "5e9729490b7a7bcf7a803d5333fab6476849dc1765d2f5c57deea71ca86249ab", "size_bytes": 105574912},
        "kin-daemon.exe": {"sha256": "923fbf6d669d6d32fab65a68b3338173af15ef4ea99d43f6d5b589d0882d7696", "size_bytes": 120715776},
    },
}
REQUIRED_HEALTHY = (
    "kin_binary", "kin_daemon_binary", "shell_path", "setup_ledger", "commit_author",
)
CLIENT_NAMES = ("claude", "cursor", "gemini", "windsurf")
STATUSES = {"healthy", "unsupported", "missing", "misconfigured", "stale", "pending", "degraded"}


def write_json(path: Path, data: dict) -> None:
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(block)
    return hasher.hexdigest()


def validate_host(system: str, machine: str, env: dict) -> None:
    if system != "win32" or machine.lower() not in {"amd64", "x86_64"}:
        raise ValueError("compatibility baseline requires native Windows x86_64")
    if env.get("GITHUB_ACTIONS") != "true" or env.get("RUNNER_ENVIRONMENT") != "github-hosted":
        raise ValueError("compatibility setup requires a disposable GitHub-hosted account")


def validate_version(text: str, name: str) -> None:
    pattern = rf"{re.escape(name)} {re.escape(BASELINE['version'])}(?: \([^\r\n]*\))?"
    if not re.fullmatch(pattern, text.strip()):
        raise ValueError(f"installed {name} did not report exactly {BASELINE['version']}")


def validate_meta(meta: dict) -> None:
    if not isinstance(meta, dict):
        raise ValueError("bench-meta must be an object")
    expected = {
        "kin_version": BASELINE["version"],
        "kin_commit": BASELINE["commit"],
        "kin_dirty": False,
        "kin_source_known": True,
        "dependency_provenance": BASELINE["lock_sha256"],
    }
    for key, value in expected.items():
        if type(meta.get(key)) is not type(value) or meta.get(key) != value:
            raise ValueError(f"installed baseline bench-meta has unexpected {key}")


def validate_health(report: dict, label: str) -> dict:
    if not isinstance(report, dict) or not isinstance(report.get("checks"), list):
        raise ValueError(f"{label}: checks is missing or malformed")
    checks = {}
    for row in report["checks"]:
        if not isinstance(row, dict) or not isinstance(row.get("id"), str) or not row["id"]:
            raise ValueError(f"{label}: health-check id is missing or malformed")
        if row["id"] in checks:
            raise ValueError(f"{label}: duplicate health-check id {row['id']}")
        if not isinstance(row.get("status"), str) or row["status"] not in STATUSES:
            raise ValueError(f"{label}: malformed status for {row['id']}")
        checks[row["id"]] = row["status"]
    for name in REQUIRED_HEALTHY:
        if checks.get(name) != "healthy":
            raise ValueError(f"{label}: {name} must be healthy, got {checks.get(name, 'absent')}")
    blocks = any(status in {"missing", "misconfigured"}
                 or (name == "semantic_query_readiness" and status == "stale")
                 for name, status in checks.items())
    attention = any(status not in {"healthy", "unsupported"} for status in checks.values())
    verdict = "failing" if blocks else "needs_attention" if attention else "ready"
    if report.get("verdict") != verdict or report.get("healthy") is not (verdict == "ready"):
        raise ValueError(f"{label}: aggregate health disagrees with its component checks")
    if blocks:
        raise ValueError(f"{label}: baseline setup has missing or misconfigured components")
    return checks


def validate_ledger(ledger: dict) -> int:
    if not isinstance(ledger, dict) or not isinstance(ledger.get("entries"), list):
        raise ValueError("baseline setup ledger entries are missing or malformed")
    if not ledger["entries"] or any(not isinstance(row, dict) for row in ledger["entries"]):
        raise ValueError("baseline setup ledger has no valid recorded artifacts")
    return len(ledger["entries"])


def binary_info(path: Path) -> dict:
    if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
        raise ValueError(f"installed binary is not a nonempty regular file: {path}")
    return {"path": str(path), "sha256": digest(path), "size_bytes": path.stat().st_size}


def read_baseline_archive(archive: Path) -> tuple[dict, dict[str, bytes]]:
    """Recheck the reviewed release bytes without trusting the mirror receipt."""
    checksum = Path(str(archive) + ".sha256")
    for path in (archive, checksum):
        if path.is_symlink() or not path.is_file():
            raise ValueError("baseline archive and sidecar must be regular files")
    have = digest(archive)
    if archive.name != BASELINE_ARCHIVE["name"] or have != BASELINE_ARCHIVE["sha256"]:
        raise ValueError("baseline archive differs from the independently reviewed release")
    checksum_text = checksum.read_text(encoding="utf-8-sig")
    if not re.fullmatch(re.escape(have) + r"[ \t]+\*?" + re.escape(archive.name) + r"\r?\n?", checksum_text):
        raise ValueError("baseline sidecar does not bind the exact reviewed archive hash and name")
    components = {}
    with zipfile.ZipFile(archive) as bundle:
        entries = bundle.infolist()
        for name, expected in BASELINE_ARCHIVE["components"].items():
            matches = [entry for entry in entries if entry.filename == name]
            if len(matches) != 1 or matches[0].is_dir() or ((matches[0].external_attr >> 16) & 0o170000) == 0o120000:
                raise ValueError(f"baseline archive has no unique regular {name}")
            body = bundle.read(matches[0])
            if len(body) != expected["size_bytes"] or hashlib.sha256(body).hexdigest() != expected["sha256"]:
                raise ValueError(f"baseline archive {name} differs from its reviewed component")
            components[name] = body
    return {"name": archive.name, "sha256": have, "checksum_sha256": digest(checksum),
            "components": dict(BASELINE_ARCHIVE["components"])}, components


def prepare_baseline(archive: Path, home: Path) -> dict:
    """Materialize only a fresh disposable account's known legacy fixture pair."""
    source, components = read_baseline_archive(archive)
    root = home / ".kin"
    bin_dir = root / "bin"
    if root.is_symlink() or bin_dir.exists() or bin_dir.is_symlink():
        raise ValueError("legacy baseline preparation refuses an existing or linked account bin directory")
    bin_dir.mkdir(parents=True, exist_ok=False)
    installed = {}
    for name, body in components.items():
        target = bin_dir / name
        with target.open("xb") as destination:
            destination.write(body)
        info = binary_info(target)
        if {key: info[key] for key in ("sha256", "size_bytes")} != source["components"][name]:
            raise ValueError(f"prepared {name} differs from the independently verified release archive")
        installed[name] = info
    return {"schema": "kin.legacy-baseline-preparation.v1", "status": "PASS",
            "coverage": "verified-legacy-archive-materialization", "source_archive": source,
            "installed": installed, "candidate_installer_invoked": False,
            "modern_managed_admission": False}


def run_command(command: list[str], label: str, output: Path, cwd: Path,
                env: dict, timeout: int = 120) -> str:
    """Preserve command output and exit evidence before raising on failure."""
    record = {"command": command, "exit_code": None}
    stdout, stderr = b"", b""
    try:
        completed = subprocess.run(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout)
        stdout, stderr = completed.stdout, completed.stderr
        record["exit_code"] = completed.returncode
    except subprocess.TimeoutExpired as error:
        stdout, stderr = error.stdout or b"", error.stderr or b""
        record["error"] = "command timed out"
    except OSError as error:
        record["error"] = str(error)
    (output / f"{label}.stdout.txt").write_bytes(stdout)
    (output / f"{label}.stderr.txt").write_bytes(stderr)
    write_json(output / f"{label}.exit.json", record)
    if record["exit_code"] != 0:
        raise ValueError(f"{label} failed; see captured output and exit evidence")
    return stdout.decode("utf-8-sig").strip()


def rehearse(output: Path, baseline_archive: Path) -> dict:
    validate_host(sys.platform, platform.machine(), os.environ)
    # This directory belongs to one invocation. Never overwrite earlier proof.
    output.mkdir(parents=True, exist_ok=False)
    result = {
        "schema": SCHEMA, "status": "FAIL", "coverage": "compatibility-baseline",
        "baseline": dict(BASELINE), "platform": sys.platform, "architecture": platform.machine(),
        "host_scope": "disposable GitHub-hosted Windows account",
        "scope": "independently verified legacy archive, stable CLI/daemon identity and repository-free agent setup; "
                 "candidate binaries require separate RC Build and preflight qualification",
    }
    try:
        home = Path.home()
        result["preparation"] = prepare_baseline(baseline_archive, home)
        bin_dir = home / ".kin" / "bin"
        kin = bin_dir / "kin.exe"
        daemon = bin_dir / "kin-daemon.exe"
        work = output / "repo-free"
        work.mkdir()
        # Inherited binary/registry overrides cannot substitute for the real
        # account fixture or global Git identity. No new Kin environment keys.
        env = {key: value for key, value in os.environ.items()
               if not re.match(r"^(?:_?KIN_|GIT_|LD_|DYLD_)", key, re.IGNORECASE)}
        env["PATH"] = str(bin_dir) + os.pathsep + env.get("PATH", "")
        git = shutil.which("git", path=env["PATH"])
        if not git:
            raise ValueError("Git for Windows is required for author configuration")
        # Do not inherit repository-local author state from a checkout above
        # the requested output directory.
        probe = subprocess.run([git, "rev-parse", "--is-inside-work-tree"], cwd=work, env=env,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, timeout=30)
        (output / "repo-free.stdout.txt").write_bytes(probe.stdout)
        (output / "repo-free.stderr.txt").write_bytes(probe.stderr)
        write_json(output / "repo-free.exit.json", {"exit_code": probe.returncode})
        if probe.returncode == 0:
            raise ValueError("compatibility setup output must be outside a Git repository")

        installed = {}
        for name, path in (("kin", kin), ("kin-daemon", daemon)):
            info = binary_info(path)
            info["version_text"] = run_command([str(path), "--version"], f"{name}-version",
                                               output, work, env)
            validate_version(info["version_text"], name)
            installed[name] = info
        result["installed"] = installed
        meta = json.loads(run_command([str(kin), "bench-meta", "--json"], "bench-meta",
                                      output, work, env))
        write_json(output / "bench-meta.json", meta)
        validate_meta(meta)
        result["bench_meta"] = meta

        # Only client detection is staged. Every actual Kin command above and
        # below still executes the explicit installed kin.exe or kin-daemon.exe.
        # Copies match the established Windows setup proof's PATHEXT fixtures.
        clients = output / "client-detection"
        clients.mkdir()
        fixtures = {}
        for name in CLIENT_NAMES:
            path = clients / f"{name}.exe"
            shutil.copyfile(kin, path)
            fixtures[name] = binary_info(path)
        result["client_detection_fixtures"] = {
            "purpose": "PATH detection for global client configuration writers; no client execution proof",
            "copies_of": str(kin), "executables": fixtures,
        }
        env["PATH"] = str(clients) + os.pathsep + env["PATH"]

        # Configure the actual host scope before setup reads its author. This
        # retains the missing-Git-identity regression from the Windows proof.
        for key, value in (("user.name", "ci"), ("user.email", "ci@firelock.ai")):
            run_command([git, "config", "--global", key, value], f"git-set-{key}", output, work, env)
            actual = run_command([git, "config", "--global", "--get", key], f"git-get-{key}",
                                 output, work, env)
            if actual != value:
                raise ValueError(f"global Git {key} was not configured")
        result["git_author"] = {"name": "ci", "email": "ci@firelock.ai", "scope": "global"}
        run_command([str(kin), "setup", "--no-interactive", "--intent", "agent", "--shell", "powershell"],
                    "setup", output, work, env)
        result["health_checks"] = {}
        for label, command in (("setup-status", ["setup", "status", "--json"]),
                               ("doctor", ["doctor", "--json"])):
            report = json.loads(run_command([str(kin), *command], label, output, work, env))
            write_json(output / f"{label}.json", report)
            result["health_checks"][label] = validate_health(report, label)

        ledger = home / ".kin" / "config" / "setup-ledger.json"
        if ledger.is_symlink() or not ledger.is_file():
            raise ValueError("baseline setup did not persist a regular setup ledger")
        data = ledger.read_bytes()
        ledger_json = json.loads(data.decode("utf-8-sig"))
        entry_count = validate_ledger(ledger_json)
        (output / "setup-ledger.json").write_bytes(data)
        result["setup_ledger"] = {"path": str(ledger), "sha256": digest(ledger),
                                  "entry_count": entry_count}
        for name, path in (("kin", kin), ("kin-daemon", daemon)):
            if binary_info(path) != {key: value for key, value in installed[name].items()
                                     if key != "version_text"}:
                raise ValueError(f"installed {name} bytes changed during setup")
        result["status"] = "PASS"
    except Exception as error:
        result["error"] = str(error)
        raise
    finally:
        write_json(output / "result.json", result)
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path,
                        help="new capture directory outside any Git repository")
    parser.add_argument("--baseline-archive", required=True, type=Path,
                        help="retained genuine reviewed ZIP with its public .sha256 sidecar")
    args = parser.parse_args()
    try:
        result = rehearse(args.output.resolve(), args.baseline_archive.resolve())
    except Exception as error:
        print(f"Compatibility baseline failed: {error}", file=sys.stderr)
        return 1
    print(f"PASS: {result['coverage']} for {BASELINE['tag']}; captures: {args.output.resolve()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
