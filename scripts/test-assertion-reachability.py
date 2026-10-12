#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Fail closed when a gate's assertion is defined but nothing ever runs it.

A guard that cannot run is indistinguishable from a guard that passes. The
release authority suite lost `assert_windows_public_support_contract` to a
stale-base merge: one pull request added the helper plus its call site, a
second branched before it and merged after, and the squash removed the call
site while leaving the definition behind. No conflict, no red, nothing to
review, and a Windows public-support claim went unpoliced.

This gate makes that condition visible. For every covered suite it answers two
questions the suites cannot answer about themselves:

1. Is every `assert_*` / `test_*` helper reachable from code that actually
   executes? Reachability, not mere mention: a cluster of helpers that only
   call each other is still dead.
2. Is the suite itself invoked from a pull-request-triggered CI job? A suite
   that runs only after merge polices nothing at review time.

The second question is what keeps this file honest. `ci.yml` runs this gate,
and the release authority suite asserts that wiring exists, so neither gate can
be removed without the other going red.
"""

from __future__ import annotations

import ast
import sys
import textwrap
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = ROOT / ".github" / "workflows"
CI = WORKFLOWS / "ci.yml"

# Prefixes that name a check rather than a plumbing helper. An orphaned
# `assert_*` or `test_*` is a policy that silently stopped being enforced; an
# orphaned formatting helper is merely dead code and is not this gate's job.
HELPER_PREFIXES = ("assert_", "test_")

COVERED_SUITES = (
    "scripts/test-release-workflow-authority.py",
    "scripts/test-homebrew-release-gate.py",
    "scripts/test-assertion-reachability.py",
)

# A suite that keeps its checks in a package beside it runs as one program: the
# runner imports every module, so a check defined in one module is reached
# from the runner or from a sibling. Its modules are analyzed together with the
# runner, or every check that moved out of the runner would go unpoliced.
SUITE_PACKAGES = {
    "scripts/test-release-workflow-authority.py": "scripts/release_authority",
}


def strip_docstrings(tree: ast.AST) -> None:
    """Drop docstrings so a prose mention cannot pass for a call site.

    Comments never reach the AST, so a `# see assert_foo` note cannot keep a
    dead helper alive. Docstrings are string constants and would, which would
    let a helper documented but never invoked read as reachable.
    """

    for node in ast.walk(tree):
        body = getattr(node, "body", None)
        if not isinstance(body, list) or not body:
            continue
        first = body[0]
        if (
            isinstance(first, ast.Expr)
            and isinstance(first.value, ast.Constant)
            and isinstance(first.value.value, str)
        ):
            del body[0]


def referenced_names(nodes: list[ast.AST]) -> set[str]:
    """Every name the given code could resolve a function through.

    Deliberately wider than "calls". A helper is counted as used when it is
    named at all: called directly, passed bare into a dispatch table, wrapped
    in a `lambda`, applied as a decorator, or reached through `getattr` with a
    literal string. Narrowing this to `ast.Call` would flag the 49 table-
    registered `test_*` helpers in the Homebrew gate, and a gate that cries
    wolf gets switched off, which recreates the bug it was built to prevent.

    A name used only through a computed string (`getattr(mod, "assert_" + x)`)
    is invisible here by construction. Naming it in a module-level tuple of
    strings is enough to record the use, because an exact string constant
    counts as a reference.
    """

    names: set[str] = set()
    for root in nodes:
        for node in ast.walk(root):
            if isinstance(node, ast.Name):
                names.add(node.id)
            elif isinstance(node, ast.Attribute):
                names.add(node.attr)
            elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                names.add(node.value)
    return names


def orphaned_helpers(source: str, label: str) -> list[str]:
    """Return the covered helpers unreachable from code that executes.

    Roots are the module's own top-level statements, which is what the
    interpreter runs on import. Everything else is reached, or is not.
    """

    return orphaned_helpers_across([(source, label)])


def orphaned_helpers_across(sources: list[tuple[str, str]]) -> list[str]:
    """Return the covered helpers unreachable across modules that run together.

    Roots are every module's top-level statements, because the runner executes
    its own and importing a module executes that module's. A check is resolved
    by name across all of them, so two modules defining the same check would
    let one keep the other alive; that is refused rather than guessed at.
    """

    functions: dict[str, ast.AST] = {}
    owners: dict[str, str] = {}
    module_statements: list[ast.AST] = []
    for source, label in sources:
        tree = ast.parse(source, filename=label)
        strip_docstrings(tree)
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                if node.name in owners and node.name.startswith(HELPER_PREFIXES):
                    raise AssertionError(
                        f"{label} and {owners[node.name]} both define {node.name}; "
                        "a check resolved by name cannot say which one runs"
                    )
                functions[node.name] = node
                owners[node.name] = label
            else:
                module_statements.append(node)

    edges = {name: referenced_names([node]) for name, node in functions.items()}

    reachable: set[str] = set()
    pending = [name for name in referenced_names(module_statements) if name in functions]
    while pending:
        name = pending.pop()
        if name in reachable:
            continue
        reachable.add(name)
        pending.extend(
            ref for ref in edges[name] if ref in functions and ref not in reachable
        )

    return sorted(
        name
        for name in functions
        if name.startswith(HELPER_PREFIXES) and name not in reachable
    )


def invokes(workflow: str, suite: str) -> bool:
    """Whether a workflow line actually runs the suite.

    Exact match on the whole line, not a search for the path. A workflow can
    name a script in a comment, in a step name, or as `run: # python3 <path>`,
    and every one of those leaves the path present while running nothing. The
    suite is invoked either as its own line inside a `run: |` block or as the
    scalar of a one-line `run:`, so those are the two forms that count.
    """

    command = f"python3 {suite}"
    accepted = {command, f"run: {command}", f"python3 ./{suite}"}
    return any(line.strip() in accepted for line in workflow.splitlines())


def suite_sources(suite: str) -> list[tuple[str, str]]:
    """The runner's source, plus every module of its package if it has one."""

    path = ROOT / suite
    if not path.is_file():
        raise AssertionError(f"covered suite is missing: {suite}")
    sources = [(path.read_text(encoding="utf-8"), suite)]
    package = SUITE_PACKAGES.get(suite)
    if package is not None:
        modules = sorted((ROOT / package).glob("*.py"))
        if not modules:
            raise AssertionError(
                f"{suite} keeps its checks in {package}, which holds no modules"
            )
        sources.extend(
            (module.read_text(encoding="utf-8"), module.relative_to(ROOT).as_posix())
            for module in modules
        )
    return sources


def assert_no_orphaned_helpers(suite: str) -> None:
    orphans = orphaned_helpers_across(suite_sources(suite))
    if orphans:
        raise AssertionError(
            f"{suite} defines checks nothing runs: {', '.join(orphans)}. "
            "Restore the call site, delete the helper, or - if it is reached "
            "through a computed name - record that use in a module-level tuple "
            "of strings."
        )


def assert_suite_runs_on_pull_requests(suite: str, ci_source: str) -> None:
    """A suite that no pull request runs cannot stop anything from landing."""

    header = ci_source.split("\njobs:", maxsplit=1)[0]
    if "pull_request:" not in header:
        raise AssertionError("ci.yml no longer runs on pull requests")
    if not invokes(ci_source, suite):
        raise AssertionError(
            f"ci.yml has no step running {suite}; a gate that runs nowhere "
            "reports green for a policy it never checked"
        )


ORPHAN_FIXTURE = textwrap.dedent(
    '''
    """A suite whose helper lost its call site to a stale-base merge."""

    def assert_reached(value):
        raise AssertionError(value)


    def assert_stranded(value):
        raise AssertionError(value)


    def main():
        assert_reached("checked")


    main()
    '''
)

INDIRECTION_FIXTURE = textwrap.dedent(
    '''
    """Every reference style this gate must treat as a live call site."""

    def assert_called_directly(value):
        raise AssertionError(value)


    def assert_registered_in_a_table(value):
        raise AssertionError(value)


    def assert_wrapped_in_a_lambda(value):
        raise AssertionError(value)


    def assert_named_for_getattr(value):
        raise AssertionError(value)


    def assert_reached_only_through_a_helper(value):
        raise AssertionError(value)


    def run_indirectly(value):
        assert_reached_only_through_a_helper(value)


    DYNAMIC_USE = ("assert_named_for_getattr",)

    CHECKS = (assert_registered_in_a_table,)


    def main():
        assert_called_directly("direct")
        for check in CHECKS:
            check("table")
        deferred = lambda: assert_wrapped_in_a_lambda("lambda")
        deferred()
        for name in DYNAMIC_USE:
            globals()[name]("getattr")
        run_indirectly("helper")


    main()
    '''
)

MUTUAL_ORPHAN_FIXTURE = textwrap.dedent(
    '''
    """Two helpers that call only each other are still dead."""

    def assert_ping(value):
        assert_pong(value)


    def assert_pong(value):
        assert_ping(value)


    def main():
        return None


    main()
    '''
)

PACKAGE_RUNNER_FIXTURE = textwrap.dedent(
    '''
    """A runner whose checks live in a module of its package."""

    from checks import test_release


    def main():
        test_release.check_release("checked")


    main()
    '''
)

PACKAGE_MODULE_FIXTURE = textwrap.dedent(
    '''
    """A package module: one check the runner reaches, one nothing reaches."""

    def assert_reached_from_the_runner(value):
        raise AssertionError(value)


    def assert_stranded_in_the_module(value):
        raise AssertionError(value)


    def check_release(value):
        assert_reached_from_the_runner(value)
    '''
)

DUPLICATE_MODULE_FIXTURE = textwrap.dedent(
    '''
    """A second module defining a check its sibling already defines."""

    def assert_reached_from_the_runner(value):
        raise AssertionError(value)
    '''
)


def assert_detector_reports_a_stranded_helper() -> None:
    orphans = orphaned_helpers(ORPHAN_FIXTURE, "orphan-fixture")
    if orphans != ["assert_stranded"]:
        raise AssertionError(
            f"detector must name the stranded helper alone, reported: {orphans}"
        )


def assert_detector_ignores_indirect_call_sites() -> None:
    orphans = orphaned_helpers(INDIRECTION_FIXTURE, "indirection-fixture")
    if orphans:
        raise AssertionError(
            f"detector cried wolf on live indirect call sites: {orphans}"
        )


def assert_wiring_check_rejects_a_disabled_step() -> None:
    """A named-but-not-run script must not read as wired."""

    suite = "scripts/example-gate.py"
    wired = (
        "      - name: Validate example gate\n"
        f"        run: python3 {suite}\n"
    )
    if not invokes(wired, suite):
        raise AssertionError("wiring check missed a real one-line run: step")
    if not invokes(f"        run: |\n          python3 {suite}\n", suite):
        raise AssertionError("wiring check missed a real run-block invocation")

    disabled = {
        "commented-out step": f"        # run: python3 {suite}\n",
        "commented-out command": f"        run: # python3 {suite}\n",
        "mention in a step name": f"      - name: replaces {suite}\n",
        "mention in a comment": f"      # {suite} used to run here\n",
    }
    for label, workflow in disabled.items():
        if invokes(workflow, suite):
            raise AssertionError(f"wiring check accepted a {label}")


def assert_detector_reports_a_mutually_recursive_cluster() -> None:
    orphans = orphaned_helpers(MUTUAL_ORPHAN_FIXTURE, "mutual-orphan-fixture")
    if orphans != ["assert_ping", "assert_pong"]:
        raise AssertionError(
            f"detector must report a closed unreachable cluster, reported: {orphans}"
        )


def assert_detector_follows_checks_into_a_package() -> None:
    """A check that moved into a module is still policed, from the runner."""

    module_alone = orphaned_helpers(PACKAGE_MODULE_FIXTURE, "module-fixture")
    if module_alone != [
        "assert_reached_from_the_runner",
        "assert_stranded_in_the_module",
    ]:
        raise AssertionError(
            "a package module read alone must report every check its runner "
            f"reaches, reported: {module_alone}"
        )
    orphans = orphaned_helpers_across(
        [
            (PACKAGE_RUNNER_FIXTURE, "runner-fixture"),
            (PACKAGE_MODULE_FIXTURE, "module-fixture"),
        ]
    )
    if orphans != ["assert_stranded_in_the_module"]:
        raise AssertionError(
            "detector must follow the runner into its package and name the "
            f"stranded check alone, reported: {orphans}"
        )
    try:
        orphaned_helpers_across(
            [
                (PACKAGE_RUNNER_FIXTURE, "runner-fixture"),
                (PACKAGE_MODULE_FIXTURE, "module-fixture"),
                (DUPLICATE_MODULE_FIXTURE, "duplicate-fixture"),
            ]
        )
    except AssertionError as error:
        if "both define assert_reached_from_the_runner" not in str(error):
            raise
    else:
        raise AssertionError(
            "detector accepted two modules defining the same check, so one "
            "could keep the other alive"
        )


def main() -> None:
    # Prove the detector still discriminates before trusting it on real files.
    # A gate that reports nothing and a gate that reports everything are both
    # useless, and only a falsification arm tells the two apart.
    assert_detector_reports_a_stranded_helper()
    assert_detector_ignores_indirect_call_sites()
    assert_detector_reports_a_mutually_recursive_cluster()
    assert_detector_follows_checks_into_a_package()
    assert_wiring_check_rejects_a_disabled_step()

    ci_source = CI.read_text(encoding="utf-8")
    for suite in COVERED_SUITES:
        assert_no_orphaned_helpers(suite)
        assert_suite_runs_on_pull_requests(suite, ci_source)

    print(
        f"{len(COVERED_SUITES)} gate suites run on pull requests and define no "
        "check that nothing calls"
    )


if __name__ == "__main__":
    try:
        main()
    except AssertionError as error:
        print(f"assertion reachability gate failed: {error}", file=sys.stderr)
        sys.exit(1)
