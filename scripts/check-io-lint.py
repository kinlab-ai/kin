#!/usr/bin/env python3
"""Filesystem boundary lint.

Kin answers semantic questions from its graph. Raw filesystem and process
access belongs at declared boundaries: ingestion, import, reconcile, migration,
projection, configuration, the store's own records and the processes Kin
supervises. This check finds every raw call site by name resolution rather
than by reading text, and holds the set of sites to an explicit list.

It runs clippy with `scripts/io-lint/clippy.toml`, whose disallowed-methods
list names every raw entry point, and compares each reported site with
`scripts/io-lint/allowed-sites.txt`. Clippy resolves a call to the function it
names, so an alias, a glob import, a `PathBuf` method, a function pointer and a
macro expansion are all reported, and a graph lease's own `.metadata()` or the
process's own id is not.

A site is keyed by its source path, the innermost named function around it,
and the method it calls. There are no counts and no line numbers, so an edit
that moves code or removes a call cannot break the check. A raw call in a
function that did not already make that call is reported.

Two more things are graded, because either would silence the lint:

- a lint suppression that covers the disallowed-methods lint (`allow` or
  `expect` of `clippy::disallowed_methods`, `clippy::disallowed_types`,
  `clippy::style`, `clippy::all` or `warnings`) outside test code and outside
  `scripts/io-lint/allowed-suppressions.txt`;
- a crate-local `clippy.toml`, which would replace this configuration for that
  crate. The workspace keeps one, at its root.

Before the workspace is linted, the probe crate in `scripts/io-lint/probe`
proves the configuration still reports every shape it claims to.

Usage:
  check-io-lint.py                      lint the whole workspace
  check-io-lint.py -p kin-mcp -p ...    lint these packages only
  check-io-lint.py --update             add this run's sites to allowed-sites.txt
  check-io-lint.py --update --prune     ... and drop listed sites this run did not see
  check-io-lint.py --cargo-arg=--target --cargo-arg=x86_64-pc-windows-gnu   lint another platform
  check-io-lint.py --diagnostics FILE   read cargo's JSON messages from FILE
  check-io-lint.py --cargo-arg=--profile --cargo-arg=iteration ...

With RUST_TEST_PACKAGES set (a JSON list, as private CI sets it) and no `-p`,
the run takes that scope, and RUN_ALL_RUST_TESTS=true widens it to the
workspace.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

KIN_ROOT = Path(__file__).resolve().parent.parent
CONFIG_DIR = KIN_ROOT / "scripts" / "io-lint"
ALLOWED_SITES = CONFIG_DIR / "allowed-sites.txt"
ALLOWED_SUPPRESSIONS = CONFIG_DIR / "allowed-suppressions.txt"
PROBE_MANIFEST = CONFIG_DIR / "probe" / "Cargo.toml"
LINT_CODES = ("clippy::disallowed_methods", "clippy::disallowed_types")
DISALLOWED = re.compile(r"disallowed (?:method|type) `([^`]+)`")
CONFIG_PATH = re.compile(r'path\s*=\s*"([^"]+)"')
CLIPPY_ARGS = [
    "--",
    "-A", "clippy::all",
    "-W", "clippy::disallowed_methods",
    "-W", "clippy::disallowed_types",
    # A workflow that sets RUSTFLAGS=-D warnings would otherwise turn the
    # first reported site into a failed crate, and every crate that depends on
    # it would go unlinted.
    "--cap-lints", "warn",
]
MODULE_SCOPE = "<module>"


@dataclass(frozen=True, order=True)
class Site:
    """One raw call site, as the allowed list keys it."""

    file: str
    function: str
    method: str

    def line(self) -> str:
        return f"{self.file}\t{self.function}\t{self.method}"


@dataclass(frozen=True)
class Reported:
    site: Site
    line: int


# ---------------------------------------------------------------------------
# Which function a line sits in.
#
# The workspace is held to `cargo fmt --check`, so a function's closing brace
# sits on its own line at the indentation of its `fn` line. That makes the
# body's extent readable without a Rust parser.

FN_DECL = re.compile(
    r"^(?P<indent>\s*)(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?"
    r"(?:async\s+)?(?:unsafe\s+)?(?:extern\s+(?:\"[^\"]*\"\s+)?)?fn\s+(?P<name>[A-Za-z0-9_]+)\b"
)


def function_ranges(lines: list[str]) -> list[tuple[int, int, str]]:
    """Every function body as (first line, last line, name), 1-based."""
    ranges = []
    for index, text in enumerate(lines):
        match = FN_DECL.match(text)
        if not match:
            continue
        indent = match.group("indent")
        end = None
        for cursor in range(index, len(lines)):
            stripped = lines[cursor].rstrip()
            code = stripped.split("//", 1)[0].rstrip()
            if code.endswith(";") and "{" not in code:
                # A declaration without a body, such as a trait method.
                break
            if "{" in code:
                if code.endswith("}") and code.count("{") == code.count("}"):
                    end = cursor
                    break
                for closing in range(cursor + 1, len(lines)):
                    if lines[closing].rstrip() == indent + "}":
                        end = closing
                        break
                break
        if end is not None:
            ranges.append((index + 1, end + 1, match.group("name")))
    return ranges


class FunctionIndex:
    """Caches each source's function ranges."""

    def __init__(self, root: Path):
        self.root = root
        self.cache: dict[str, list[tuple[int, int, str]]] = {}

    def enclosing(self, relative: str, line: int) -> str:
        ranges = self.cache.get(relative)
        if ranges is None:
            path = self.root / relative
            try:
                text = path.read_text(encoding="utf-8")
            except OSError:
                text = ""
            ranges = function_ranges(text.split("\n"))
            self.cache[relative] = ranges
        best = None
        for first, last, name in ranges:
            if first <= line <= last and (best is None or first > best[0]):
                best = (first, name)
        return best[1] if best else MODULE_SCOPE


# ---------------------------------------------------------------------------
# Reading cargo's messages.


def configured_methods(config: Path) -> set[str]:
    return set(CONFIG_PATH.findall(config.read_text(encoding="utf-8")))


def call_site(span: dict) -> dict:
    """The outermost call site of a span that came from a macro expansion.

    A site written inside a local `macro_rules!` is reported at the macro's
    definition. The function that matters is the one that invoked it.
    """
    while span.get("expansion") and span["expansion"].get("span"):
        span = span["expansion"]["span"]
    return span


def relative_source(file_name: str, root: Path) -> str | None:
    path = Path(file_name)
    if not path.is_absolute():
        path = (root / path).resolve()
    try:
        return path.resolve().relative_to(root.resolve()).as_posix()
    except ValueError:
        return None


def parse_messages(lines, root: Path, methods: set[str], index: FunctionIndex) -> tuple[list[Reported], list[str]]:
    """Reported sites, and any compiler errors that mean the run did not finish."""
    reported: dict[tuple[Site, int], Reported] = {}
    errors: list[str] = []
    for raw in lines:
        raw = raw.strip()
        if not raw.startswith("{"):
            continue
        try:
            record = json.loads(raw)
        except json.JSONDecodeError:
            continue
        if record.get("reason") != "compiler-message":
            continue
        message = record.get("message") or {}
        if "custom-build" in (record.get("target") or {}).get("kind", []):
            continue
        code = (message.get("code") or {}).get("code")
        if message.get("level") == "error" and code not in LINT_CODES:
            errors.append(message.get("rendered") or message.get("message", ""))
            continue
        if code not in LINT_CODES:
            continue
        found = DISALLOWED.search(message.get("message", ""))
        if not found or found.group(1) not in methods:
            continue
        primary = next((span for span in message.get("spans", []) if span.get("is_primary")), None)
        if primary is None:
            continue
        span = call_site(primary)
        relative = relative_source(span["file_name"], root)
        if relative is None:
            continue
        line = int(span["line_start"])
        site = Site(relative, index.enclosing(relative, line), found.group(1))
        reported[(site, line)] = Reported(site, line)
    return sorted(reported.values(), key=lambda r: (r.site, r.line)), errors


# ---------------------------------------------------------------------------
# The allowed list.

ALLOWED_HEADER = """\
# Raw filesystem and process call sites the filesystem boundary lint accepts.
#
# One site per line: source path, innermost named function (<module> outside
# any function), and the method called, separated by tabs. Generated by
# `python3 scripts/check-io-lint.py --update`; sorted, so concurrent changes
# merge line by line.
#
# Every site here is either a declared boundary (ingestion, import, reconcile,
# migration, projection, configuration, the store's own records, a process Kin
# supervises) or work still to move behind a kin-io capability. A site leaves
# this list when its crate moves to capabilities. A new site on an answer path
# does not belong here: answer from the graph instead.
"""


def load_allowed(path: Path) -> set[Site]:
    sites = set()
    if not path.exists():
        return sites
    for number, text in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not text.strip() or text.startswith("#"):
            continue
        parts = text.split("\t")
        if len(parts) != 3 or not all(parts):
            raise SystemExit(f"{path}:{number}: want three tab-separated fields, got {text!r}")
        sites.add(Site(*parts))
    return sites


def write_allowed(path: Path, sites: set[Site]) -> None:
    body = "".join(site.line() + "\n" for site in sorted(sites))
    path.write_text(ALLOWED_HEADER + "\n" + body, encoding="utf-8")


def in_scope(site: Site, directories: list[str] | None) -> bool:
    if directories is None:
        return True
    return any(site.file == d or site.file.startswith(d + "/") for d in directories)


def compare(
    found: list[Reported], allowed: set[Site], directories: list[str] | None
) -> tuple[list[Reported], list[Site]]:
    """Reported sites the list does not hold, and listed sites in scope that no
    longer occur."""
    current = {item.site for item in found}
    new = [item for item in found if item.site not in allowed]
    stale = sorted(site for site in allowed if in_scope(site, directories) and site not in current)
    return new, stale


# ---------------------------------------------------------------------------
# Suppressions and local configurations.

SUPPRESSED_LINTS = (
    "clippy::disallowed_methods",
    "clippy::disallowed_types",
    "clippy::style",
    "clippy::all",
    "warnings",
)
ATTRIBUTE = re.compile(r"#!?\[\s*(allow|expect|cfg_attr)\s*\((.*?)\)\s*\]", re.S)
TEST_GATE = re.compile(r"^\s*(test|all\s*\(\s*test\b)")


def is_test_source(relative: str) -> bool:
    parts = relative.split("/")
    name = parts[-1]
    return (
        "tests" in parts
        or "benches" in parts
        or "examples" in parts
        or "fuzz" in parts
        # Cargo's own layout: a build script and the modules it declares
        # beside it, never an ordinary `src/build.rs` module.
        or (len(parts) == 3 and parts[2] == "build.rs")
        or (len(parts) > 3 and parts[2] == "build")
        or name.endswith("_test.rs")
        or name.endswith("_tests.rs")
    )


def suppressions(text: str) -> list[tuple[int, str]]:
    """Attributes that would silence the boundary lint, as (line, attribute)."""
    found = []
    for match in ATTRIBUTE.finditer(text):
        kind, body = match.group(1), match.group(2)
        compact = re.sub(r"\s+", "", body)
        if kind == "cfg_attr":
            predicate = body.split(",", 1)[0]
            if TEST_GATE.match(predicate):
                continue
            if not re.search(r"\b(allow|expect)\(", compact):
                continue
        lints = re.findall(r"[A-Za-z_]+(?:::[A-Za-z_]+)?", compact)
        if any(lint in SUPPRESSED_LINTS for lint in lints):
            line = text.count("\n", 0, match.start()) + 1
            found.append((line, re.sub(r"\s+", " ", match.group(0))))
    return found


def audit_sources(root: Path) -> list[str]:
    allowed = set()
    if ALLOWED_SUPPRESSIONS.exists():
        allowed = {
            line.strip()
            for line in ALLOWED_SUPPRESSIONS.read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.startswith("#")
        }
    problems = []
    crates = root / "crates"
    for path in sorted(crates.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.name in ("clippy.toml", ".clippy.toml"):
            problems.append(
                f"{relative}: a crate-local clippy configuration replaces the workspace one for "
                "that crate, which turns the filesystem boundary lint off there. Keep one at the "
                "workspace root."
            )
            continue
        if path.suffix != ".rs" or "/target/" in f"/{relative}/" or is_test_source(relative):
            continue
        if relative in allowed:
            continue
        text = path.read_text(encoding="utf-8")
        for line, attribute in suppressions(text):
            problems.append(
                f"{relative}:{line}: `{attribute}` silences the filesystem boundary lint. "
                "Remove it, or declare this source in scripts/io-lint/allowed-suppressions.txt."
            )
    return problems


# ---------------------------------------------------------------------------
# Running clippy.


def scope_from_arguments(packages: list[str], environment: dict[str, str]) -> list[str] | None:
    """Package names to lint, or None for the whole workspace."""
    if packages:
        return packages
    if environment.get("RUN_ALL_RUST_TESTS") == "true":
        return None
    selection = environment.get("RUST_TEST_PACKAGES")
    if selection:
        chosen = json.loads(selection)
        if not isinstance(chosen, list) or not all(isinstance(p, str) for p in chosen):
            raise SystemExit("RUST_TEST_PACKAGES must be a JSON list of package names")
        return chosen
    return None


def package_directories(packages: list[str]) -> list[str]:
    completed = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"],
        cwd=KIN_ROOT, capture_output=True, text=True, check=True,
    )
    metadata = json.loads(completed.stdout)
    directories = {}
    for package in metadata["packages"]:
        directory = Path(package["manifest_path"]).parent.resolve()
        directories[package["name"]] = directory.relative_to(KIN_ROOT.resolve()).as_posix()
    missing = [p for p in packages if p not in directories]
    if missing:
        raise SystemExit(f"no workspace package named {', '.join(missing)}")
    return [directories[p] for p in packages]


# The packages a cross-platform pass enters through: the shipped binaries. Their
# workspace dependencies are linted with them, under that platform's cfgs.
CROSS_ENTRY_PACKAGES = ("kin-cli", "kin-daemon")


def run_clippy(
    manifest: Path,
    scope: list[str] | None,
    cargo_args: list[str],
    target: str | None = None,
) -> tuple[list[str], int]:
    command = ["cargo", "clippy", "--manifest-path", str(manifest), "--locked",
               "--lib", "--bins", "--message-format=json", *cargo_args]
    if target is not None:
        # Another platform's code, compiled the way CI's cross-check compiles
        # it: default features, entered through the shipped binaries.
        command += ["--target", target, *[a for p in CROSS_ENTRY_PACKAGES for a in ("-p", p)]]
    elif manifest == KIN_ROOT / "Cargo.toml":
        command += ["--all-features"]
        if scope is None:
            command += ["--workspace"]
        else:
            # Only the named packages: an unchanged workspace dependency has
            # nothing new to report.
            command += ["--no-deps", *[a for p in scope for a in ("-p", p)]]
    else:
        command += ["--all-features"]
    command += CLIPPY_ARGS
    environment = dict(os.environ, CLIPPY_CONF_DIR=str(CONFIG_DIR))
    print("+ " + " ".join(command), file=sys.stderr, flush=True)
    completed = subprocess.run(command, cwd=KIN_ROOT, env=environment, stdout=subprocess.PIPE, text=True)
    return completed.stdout.splitlines(), completed.returncode


# ---------------------------------------------------------------------------
# The probe.

PROBE_MARK = re.compile(r"//\s*io-lint:\s*(reported\s+(\S+)|clean)\s*$")


def check_probe(cargo_args: list[str], methods: set[str]) -> list[str]:
    root = PROBE_MANIFEST.parent
    source = root / "src" / "lib.rs"
    expected: dict[int, str | None] = {}
    for number, text in enumerate(source.read_text(encoding="utf-8").splitlines(), 1):
        mark = PROBE_MARK.search(text)
        if mark:
            expected[number] = mark.group(2)
    if not expected:
        return ["the probe crate marks no lines, so it proves nothing"]
    lines, status = run_clippy(PROBE_MANIFEST, None, cargo_args)
    found, errors = parse_messages(lines, root, methods, FunctionIndex(root))
    problems = [f"the probe crate did not build: {e}" for e in errors]
    if status != 0 and not errors:
        problems.append(f"clippy exited {status} on the probe crate")
    if problems:
        return problems
    seen: dict[int, set[str]] = {}
    for item in found:
        if item.site.file == "src/lib.rs":
            seen.setdefault(item.line, set()).add(item.site.method)
    for number, method in sorted(expected.items()):
        got = seen.get(number, set())
        if method is None and got:
            problems.append(f"probe line {number} should be clean but was reported for {', '.join(sorted(got))}")
        elif method is not None and method not in got:
            problems.append(f"probe line {number} should report {method} but reported {', '.join(sorted(got)) or 'nothing'}")
    unmarked = sorted(set(seen) - set(expected))
    for number in unmarked:
        problems.append(f"probe line {number} was reported for {', '.join(sorted(seen[number]))} but carries no mark")
    return problems


# ---------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("-p", "--package", action="append", default=[], help="lint this package only")
    parser.add_argument("--update", action="store_true", help="add this run's sites to allowed-sites.txt")
    parser.add_argument(
        "--prune",
        action="store_true",
        help="with --update, also drop listed sites in scope that this run did not see "
        "(only from a run that compiled every platform those sites need)",
    )
    parser.add_argument("--diagnostics", type=Path, help="read cargo JSON messages from this path")
    parser.add_argument("--cargo-arg", action="append", default=[], help="pass an argument to cargo")
    parser.add_argument("--no-probe", action="store_true", help="skip the probe crate")
    parser.add_argument("--show-stale", action="store_true", help="list each listed site this run did not see")
    parser.add_argument(
        "--target",
        help="lint another platform's code (for example x86_64-pc-windows-gnu), entered "
        "through kin-cli and kin-daemon",
    )
    arguments = parser.parse_args(argv)

    methods = configured_methods(CONFIG_DIR / "clippy.toml")
    failed = False

    problems = audit_sources(KIN_ROOT)
    for problem in problems:
        print(f"error: {problem}")
    failed |= bool(problems)

    if not arguments.no_probe and arguments.diagnostics is None:
        probe_problems = check_probe(arguments.cargo_arg, methods)
        for problem in probe_problems:
            print(f"error: {problem}")
        if probe_problems:
            print("The boundary lint's configuration no longer reports what its probe claims, so a "
                  "clean result below would prove nothing.")
            return 1
        print("Probe crate: every marked shape reported as declared.")

    scope = None if arguments.target else scope_from_arguments(arguments.package, os.environ)
    directories = None if scope is None else package_directories(scope)

    if arguments.diagnostics is not None:
        lines, status = arguments.diagnostics.read_text(encoding="utf-8").splitlines(), 0
    else:
        lines, status = run_clippy(KIN_ROOT / "Cargo.toml", scope, arguments.cargo_arg, arguments.target)
    found, errors = parse_messages(lines, KIN_ROOT, methods, FunctionIndex(KIN_ROOT))
    if errors or status != 0:
        for error in errors:
            print(error)
        print(f"error: clippy did not finish (exit {status}), so the sites below may be incomplete.")
        return 1

    current = {item.site for item in found}
    allowed = load_allowed(ALLOWED_SITES)

    if arguments.update:
        # Adding is always safe. Dropping is not: a site in code compiled only
        # on another platform is absent from this run and still real, so a
        # listed site goes only when --prune asks for it.
        kept = allowed
        if arguments.prune:
            kept = {site for site in allowed if not in_scope(site, directories) or site in current}
        write_allowed(ALLOWED_SITES, kept | current)
        added = len(current - allowed)
        print(
            f"Wrote {len(kept | current)} sites to {ALLOWED_SITES.relative_to(KIN_ROOT)} "
            f"({added} added, {len(allowed - kept)} pruned)."
        )
        return 1 if failed else 0

    new, stale = compare(found, allowed, directories)
    if arguments.target or arguments.diagnostics is not None:
        # One platform's view, or a saved run: a listed site it did not see may
        # belong to code it never compiled.
        stale = []
    new_sites = {item.site for item in new}
    for item in new:
        site = item.site
        where = "outside any function" if site.function == MODULE_SCOPE else f"in `{site.function}`"
        message = (
            f"`{site.method}` {where} is raw filesystem or process access that is not on the "
            "boundary list. Answer from the graph, or, at a declared boundary, take a kin-io "
            "capability or add the site to scripts/io-lint/allowed-sites.txt."
        )
        print(f"{site.file}:{item.line}: {message}")
        if os.environ.get("GITHUB_ACTIONS") == "true":
            print(f"::error file={site.file},line={item.line},title=Filesystem boundary::{message}")
    if arguments.show_stale:
        for site in stale:
            print(f"note: this run saw no `{site.method}` in `{site.function}` in {site.file}. If no "
                  "platform compiles it any more, its line in scripts/io-lint/allowed-sites.txt can go.")
    failed |= bool(new)

    scope_text = "the workspace" if scope is None else ", ".join(scope)
    if arguments.target:
        scope_text = f"{', '.join(CROSS_ENTRY_PACKAGES)} and their workspace dependencies for {arguments.target}"
    print(
        f"Filesystem boundary lint over {scope_text}: {len(current)} raw call sites, "
        f"{len(new_sites)} not on the list, {len(stale)} listed sites this run did not see "
        "(code for another platform, or calls that are gone; --show-stale lists them)."
    )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
