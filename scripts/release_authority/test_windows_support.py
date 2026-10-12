# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""The Windows support claim.

The installer notice, the public documents that repeat it and the
admission contract that proves it.
"""

from __future__ import annotations

import re
from pathlib import Path

from .test_helpers import (
    INSTALL_PS1,
    NPM_CANONICAL_README,
    QUICKSTART_DOC,
    README,
    README_REFERENCE_DOC,
    ROOT,
    UPDATE_TRUST,
    active_lines,
    expect_assertion,
    install_proof_step,
    replace_exactly_once,
    require,
)


WINDOWS_INIT_CONTRACT = ROOT / "scripts" / "assert-windows-init-contract.sh"
WINDOWS_INIT_CONTRACT_POLICY = "scripts/assert-windows-init-contract.sh"
WINDOWS_WSL2_DOC = ROOT / "docs" / "windows-wsl2.md"

LLMS_DOC = ROOT / "llms.txt"

NPM_MCP_README = ROOT / "packages" / "kin-mcp" / "README.md"

INSTALL_PS1_POLICY = "scripts/install.ps1"
# The installer notice ends with autocrlf recovery advice for a shell that has
# already failed. Prose surfaces carry the capability claim before it, so this
# sentence opener is where the installer rendering and the doc rendering split.
WINDOWS_NOTICE_INSTALLER_TAIL = " If kin init reports"


def windows_init_contract_strings() -> dict[str, str]:
    """Return wording shared by both Windows admission proofs.

    The install proof installs a public release anonymously and has no
    checkout, so it cannot run the contract script and retypes these instead.
    Reading the script here is the only thing that keeps the two statements of
    one contract from drifting apart.
    """

    source = WINDOWS_INIT_CONTRACT.read_text(encoding="utf-8")
    strings: dict[str, str] = {}
    for name in ("NON_EMPTY_REFUSAL",):
        bindings = re.findall(rf'(?m)^{name}="([^"]+)"$', source)
        if len(bindings) != 1:
            raise AssertionError(
                f"{WINDOWS_INIT_CONTRACT_POLICY} must bind {name} exactly once: "
                f"{bindings}"
            )
        strings[name] = bindings[0]
    return strings


def windows_public_support_notice(install_ps1: str) -> str:
    """Read the one public capability statement every Windows surface repeats.

    `scripts/assert-windows-init-contract.sh` used to bind this notice, back
    when the whole subject of that contract was that native Windows refused
    every repository. The contract now proves the opposite: both admission
    boundaries `require_admitted`, and only a non-empty native directory is
    refused. It binds no notice at all any more, so the installer's single
    `$NativeWindowsSupportNotice` literal is the owner. That is the copy a
    Windows user reads before anything is downloaded, and the copy that went
    stale in public.
    """

    bindings = re.findall(
        r'(?m)^\$NativeWindowsSupportNotice = "([^"]+)"$', install_ps1
    )
    if len(bindings) != 1:
        raise AssertionError(
            f"{INSTALL_PS1_POLICY} must bind $NativeWindowsSupportNotice "
            f"exactly once: {bindings}"
        )
    notice = bindings[0]
    for truth in (
        "Native Windows x86_64 support is early",
        "Repository admission works",
        "kin init imports a Git repository and publishes graph authority",
        # The install proof's windows-latest row runs agent setup and the MCP
        # tool-call step, and `assert_install_proof_repo_steps_cover_windows`
        # keeps both on that row.
        "runs agent setup on native Windows",
        "gets graph-backed answers from the installed MCP server",
        "Transparent filesystem projection is not shipped on Windows",
        # No native Windows leg runs a review test, and the install proof runs
        # review on no row at all, so this is a statement about Windows tests
        # rather than about the install proof.
        "review workflows are not yet tested there",
        "WSL2 remains the recommended path",
    ):
        if truth not in notice:
            raise AssertionError(
                "Windows public support notice no longer states the executable "
                f"admission contract: missing {truth!r}"
            )
    return notice


def windows_public_support_doc_notice(notice: str) -> str:
    """Render the installer's notice the way prose surfaces carry it.

    Markdown and `llms.txt` copies code-format the command and drop the
    installer-only autocrlf recovery sentence, which is advice for a command
    that already failed rather than a capability claim. Deriving both
    renderings from the one binding is what keeps a single owner: editing
    either rendering alone still fails the exactly-once rule below.
    """

    doc = notice.split(WINDOWS_NOTICE_INSTALLER_TAIL)[0]
    if doc == notice:
        raise AssertionError(
            f"{INSTALL_PS1_POLICY} notice must keep its autocrlf recovery "
            "sentence, which marks where the prose rendering ends: "
            f"missing {WINDOWS_NOTICE_INSTALLER_TAIL!r}"
        )
    return doc.replace("kin init", "`kin init`")


def assert_windows_public_support_contract(
    contract_source: str,
    install_ps1: str,
    public_surfaces: dict[Path, str],
    compatibility_mcp_readme: str,
) -> None:
    """Keep every install surface inside the native-Windows admission boundary.

    The required Windows jobs prove what native Windows actually admits. This
    public notice is therefore owned by one binding rather than by independent
    prose on each surface. Each shipped installer/doc/package copy must be
    exact, and capability claims the contract disproves are forbidden even
    where the exact notice survives elsewhere in the same file.
    """

    notice = windows_public_support_notice(install_ps1)
    doc_notice = windows_public_support_doc_notice(notice)
    contract_active = "\n".join(active_lines(contract_source))
    for admission in (
        'require_admitted "Windows exact-Git admission"',
        'require_admitted "Windows native-unborn bootstrap"',
        "require_non_empty_refused",
        '"Windows non-empty native boundary"',
        'fail "$label unexpectedly succeeded" "$log"',
    ):
        require(
            contract_active,
            admission,
            "public Windows support notice tied to executable admission",
        )

    install_active = "\n".join(active_lines(install_ps1))
    installer_count = install_active.count(notice)
    if installer_count != 1:
        raise AssertionError(
            f"{INSTALL_PS1_POLICY} must carry the Windows support notice "
            f"exactly once; found {installer_count}"
        )
    for path, source in public_surfaces.items():
        count = "\n".join(active_lines(source)).count(doc_notice)
        if count != 1:
            raise AssertionError(
                f"{path.relative_to(ROOT)} must repeat the Windows support notice "
                f"from {INSTALL_PS1_POLICY} exactly once; found {count}"
            )

    all_surfaces = {INSTALL_PS1: install_ps1, **public_surfaces}
    normalized = " ".join(
        "\n".join([*all_surfaces.values(), compatibility_mcp_readme]).lower().split()
    )
    for stale_claim in (
        # Refusal-era claims. Native Windows admits repositories now, so each
        # of these understates the shipped product; the first three are the
        # v0.5.3 installer wording that stayed live on the public endpoint
        # after the notice itself was corrected.
        "repository admission is currently unavailable",
        "kin init fails closed",
        "graph, lexical, daemon, repository setup, mcp, and review workflows are unsupported",
        "use wsl2 for usable kin repositories",
        "native windows cannot admit a kin repository",
        "native windows cannot currently admit a repository",
        "while admission is unsupported",
        # Vector-free-era wording in the other direction. Each line comes from
        # copy that called native Windows a vector-free build, and the platform
        # table's line went on to claim review there. The archive carries
        # vector search and review workflows are not yet tested on Windows, so
        # none of these may return.
        "native windows is a supported vector-free subset",
        "native windows build is a supported vector-free runtime",
        "native windows supports graph + lexical workflows",
        "supported for graph, lexical retrieval, daemon, setup, mcp",
        "it ships the supported vector-free runtime",
        "the graph, lexical, daemon, setup, and mcp surfaces are release-tested",
        # Understatements of what the install proof now runs there. Its
        # windows-latest row runs agent setup and MCP tool calls, so no surface
        # may say it does not.
        "does not yet cover mcp",
        "mcp on native windows is not yet covered",
        "mcp and review workflows are not yet covered",
    ):
        if stale_claim in normalized:
            raise AssertionError(
                "public native-Windows surface contradicts the executable "
                f"admission contract: found stale claim {stale_claim!r}"
            )

    if re.search(r'(?m)^\s*"ARM64"\s*\{\s*return\b', install_ps1):
        raise AssertionError(
            "PowerShell installer must never resolve native ARM64 to a release "
            "archive; only windows-x86_64 is published"
        )
    for policy in (
        '$NativeWindowsSupportNotice = "' + notice + '"',
        'Write-Host "  ! $NativeWindowsSupportNotice"',
        "function Resolve-KinWindowsArchiveArchitecture",
        '"AMD64" { return "x86_64" }',
        '"ARM64" { throw "No native Windows ARM64 archive is published.',
        "Not running repository setup: WSL2 remains the recommended path on "
        "Windows.",
    ):
        require(install_active, policy, "truthful native-Windows installer")
    if "windows-aarch64" in install_ps1:
        raise AssertionError(
            "PowerShell installer fabricates the nonexistent windows-aarch64 archive"
        )
    if "& $KinExe setup" in install_active:
        raise AssertionError(
            "native-Windows installer must not run setup while WSL2 remains the "
            "recommended path there"
        )

    for policy in (
        "- macOS, Linux, or native Windows x64",
        "Use WSL2 when you need projection.",
    ):
        require(
            compatibility_mcp_readme,
            policy,
            "compatibility MCP package native-Windows boundary",
        )

    # The npm command forwards `setup` on every supported platform. Only the
    # PowerShell installer omits the wizard; WSL2's recommendation is not a
    # blanket native-Windows setup refusal. Ignore prose wrapping, not content.
    quickstart_active = " ".join(
        " ".join(active_lines(public_surfaces[QUICKSTART_DOC])).split()
    )
    for policy in (
        "on macOS and Linux, skip the `kin setup` wizard",
        "The PowerShell installer always installs without starting the wizard; "
        "run `kin setup` afterwards.",
        "It accepts `KIN_NO_SETUP` only for CI compatibility and selects the "
        "CI-oriented skip message.",
        "The recommended `npx -y @kinlab/kin setup` command launches it on "
        "macOS, Linux, WSL2, and native Windows x64.",
        "After using the PowerShell installer, run `kin setup` yourself.",
        "WSL2 remains the recommended Windows route for the full experience",
        "Native Windows ARM64 has no release archive",
    ):
        require(
            quickstart_active,
            policy,
            "quickstart platform-specific setup contract",
        )
    for stale_claim in (
        "Native Windows always skips repository setup",
        "Native Windows does not launch repository setup",
    ):
        if stale_claim in quickstart_active:
            raise AssertionError(
                "quickstart conflates the PowerShell installer with native "
                f"Windows setup: {stale_claim}"
            )


def assert_windows_contract_stage_check_is_reachable(contract_source: str) -> None:
    """Keep the shipped contract's stage-residue check able to fail at all.

    Both admission paths stage into the PARENT of the directory they admit:
    `crates/kin-core/src/git_init.rs` derives the stage from the source's
    parent and `crates/kin-core/src/init.rs` from the working directory's. A
    stage count taken inside the admitted directory therefore reports zero for
    every input and passes whether or not admission cleaned up after itself.
    This script is a required Windows check on every pull request, so a form
    that cannot fail is worse here than no check at all: it reads as proof.
    """

    active = "\n".join(active_lines(contract_source))
    for policy in (
        'parent="$(dirname "$dir")"',
        "staged=\"$(count_matching \"$parent\" '.kin.init-*')\"",
        'if [ "$staged" != "0" ]; then',
    ):
        require(active, policy, "reachable Windows stage-leak check")


def assert_install_proof_windows_admission_contract(
    windows_admission: str, contract: dict[str, str]
) -> None:
    """Require released Windows bytes to assert the native admission contract."""

    active = "\n".join(active_lines(windows_admission))
    for name, wording in contract.items():
        binding = f'{name}="{wording}"'
        if binding not in active:
            raise AssertionError(
                "install proof must assert the shipped Windows admission contract "
                f"verbatim; {name} drifted from {WINDOWS_INIT_CONTRACT_POLICY}: "
                f"expected {binding}"
            )
    # A new store is central and the tree carries nothing, so publication is
    # what `kin store path` discovers against each fixture's own Kin home, and
    # the in-tree layout is admitted once more and must publish the tree's own
    # `.kin`. Stages are counted where each layout makes them.
    for policy in (
        'require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"',
        'require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree',
        'require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"',
        'require_refused "Windows non-empty native boundary" "$populated_boundary" "$populated_log"',
        'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
        'fail "$1 failed" "$3"',
        'fail "$1 unexpectedly succeeded" "$3"',
        'export KIN_HOME="$home"',
        'if ! answer="$(fixture_kin "$1" "$2" kin store path 2>> "$3")"; then',
        'fail "$1 returned success but no store serves $2" "$3"',
        "if [ \"$(count_matching \"$home/stores\" '*/manifest.json')\" != \"1\" ]; then",
        'if [ ! -d "$2/.kin" ]; then',
        'fail "$1 published authority at $2/.kin" "$3"',
        'fail "$1 published a store in $home/stores" "$3"',
        'parent="$(dirname "$2")"',
        "staged=\"$(count_matching \"$parent\" '.kin.init-*')\"",
        "staged=\"$(count_matching \"$stores\" '.kin.init-*')\"",
        'if [ "$staged" != "0" ]; then',
    ):
        require(active, policy, "shipped Windows admission contract proof")



def check_windows_public_support_and_admission(
    install_proof: str,
    install_ps1: str,
    npm_canonical_readme: str,
    quickstart: str,
    readme: str,
    readme_reference: str,
    update_trust: str,
) -> None:
    # Native Windows release bytes must admit both supported repository
    # boundaries, leave no transaction residue, and retain the non-empty
    # boundary refusal.
    windows_contract = windows_init_contract_strings()
    windows_admission = install_proof_step(
        install_proof, "Windows admission contract proof"
    )
    assert_install_proof_windows_admission_contract(windows_admission, windows_contract)
    windows_contract_source = WINDOWS_INIT_CONTRACT.read_text(encoding="utf-8")
    assert_windows_contract_stage_check_is_reachable(windows_contract_source)

    # Every shipped copy of the one public native-Windows capability claim.
    windows_public_surfaces = {
        README: readme,
        QUICKSTART_DOC: quickstart,
        README_REFERENCE_DOC: readme_reference,
        WINDOWS_WSL2_DOC: WINDOWS_WSL2_DOC.read_text(encoding="utf-8"),
        UPDATE_TRUST: update_trust,
        NPM_CANONICAL_README: npm_canonical_readme,
        LLMS_DOC: LLMS_DOC.read_text(encoding="utf-8"),
    }
    compatibility_mcp_readme = NPM_MCP_README.read_text(encoding="utf-8")
    assert_windows_public_support_contract(
        windows_contract_source,
        install_ps1,
        windows_public_surfaces,
        compatibility_mcp_readme,
    )
    windows_notice = windows_public_support_notice(install_ps1)
    windows_doc_notice = windows_public_support_doc_notice(windows_notice)

    altered_surfaces = dict(windows_public_surfaces)
    altered_surfaces[README] = readme.replace(
        windows_doc_notice,
        "Native Windows supports the graph and daemon without vectors.",
        1,
    )
    expect_assertion(
        "a shipped surface restates the notice in its own words",
        "exactly once; found 0",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1,
            altered_surfaces,
            compatibility_mcp_readme,
        ),
    )
    duplicated_surfaces = dict(windows_public_surfaces)
    duplicated_surfaces[README] = readme.replace(
        windows_doc_notice,
        f"{windows_doc_notice}\n{windows_doc_notice}",
        1,
    )
    expect_assertion(
        "a shipped surface carries the notice twice rather than once",
        "exactly once; found 2",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1,
            duplicated_surfaces,
            compatibility_mcp_readme,
        ),
    )
    for stale_claim in (
        "repository admission is currently unavailable",
        "kin init fails closed",
        "Native Windows cannot admit a Kin repository",
        "it ships the supported vector-free runtime",
        "the end-to-end install proof does not yet cover MCP there",
    ):
        stale_surfaces = dict(windows_public_surfaces)
        stale_surfaces[LLMS_DOC] = (
            f"{windows_public_surfaces[LLMS_DOC]}\n{stale_claim}\n"
        )
        expect_assertion(
            f"a shipped surface restores the stale claim {stale_claim!r}",
            "found stale claim",
            lambda mutated=stale_surfaces: assert_windows_public_support_contract(
                windows_contract_source,
                install_ps1,
                mutated,
                compatibility_mcp_readme,
            ),
        )
    expect_assertion(
        "the compatibility MCP package restores a refusal-era Windows claim",
        "found stale claim",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1,
            windows_public_surfaces,
            f"{compatibility_mcp_readme}\nkin init fails closed on native Windows.\n",
        ),
    )
    expect_assertion(
        "the installer binds the public notice twice",
        "must bind $NativeWindowsSupportNotice exactly once",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1.replace(
                f'$NativeWindowsSupportNotice = "{windows_notice}"',
                f'$NativeWindowsSupportNotice = "{windows_notice}"\n'
                f'$NativeWindowsSupportNotice = "{windows_notice}"',
                1,
            ),
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    expect_assertion(
        "the installer repeats the notice outside its one binding",
        "must carry the Windows support notice exactly once",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1.replace(
                f'$NativeWindowsSupportNotice = "{windows_notice}"',
                f'$NativeWindowsSupportNotice = "{windows_notice}"\n'
                f'Write-Host "{windows_notice}"',
                1,
            ),
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    expect_assertion(
        "the public notice drops the admission the contract proves",
        "no longer states the executable admission contract",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1.replace(
                "Repository admission works:",
                "Repository admission is unavailable:",
                1,
            ),
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    expect_assertion(
        "the executable contract reverts to refusing repository admission",
        "tied to executable admission",
        lambda: assert_windows_public_support_contract(
            windows_contract_source.replace(
                'require_admitted "Windows exact-Git admission"',
                'require_refused "Windows exact-Git admission"',
                1,
            ),
            install_ps1,
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    expect_assertion(
        "the installer resolves native ARM64 to a release archive",
        "only windows-x86_64 is published",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1.replace(
                '"ARM64" { throw "No native Windows ARM64 archive is published.',
                '"ARM64" { return "aarch64" }\n        "ARM64_" { throw "x.',
                1,
            ),
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    expect_assertion(
        "the installer runs setup while WSL2 is the recommended Windows path",
        "must not run setup while WSL2 remains the recommended path",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1.replace(
                "# ── Cleanup ",
                "& $KinExe setup\n# ── Cleanup ",
                1,
            ),
            windows_public_surfaces,
            compatibility_mcp_readme,
        ),
    )
    quickstart_drift = dict(windows_public_surfaces)
    quickstart_drift[QUICKSTART_DOC] = replace_exactly_once(
        quickstart,
        "The PowerShell installer always installs without starting the wizard; run",
        "The PowerShell installer skips setup while admission is unsupported; run",
        "quickstart refusal-era setup reason",
    )
    expect_assertion(
        "the quickstart restores the refusal-era reason for skipping setup",
        "found stale claim",
        lambda: assert_windows_public_support_contract(
            windows_contract_source,
            install_ps1,
            quickstart_drift,
            compatibility_mcp_readme,
        ),
    )
    for label, before, after in (
        (
            "the npm setup command omits native Windows",
            "launches it on macOS, Linux, WSL2, and native Windows x64.",
            "launches it on macOS, Linux, and WSL2.",
        ),
        (
            "the PowerShell installer claims to launch the wizard",
            "always installs without starting the wizard",
            "always installs and starts the wizard",
        ),
        (
            "the PowerShell instructions drop the manual setup step",
            "installer, run `kin setup` yourself.",
            "installer, setup has already run.",
        ),
        (
            "KIN_NO_SETUP claims to control the PowerShell wizard",
            "It accepts `KIN_NO_SETUP` only for CI compatibility",
            "It accepts `KIN_NO_SETUP` to disable the wizard",
        ),
        (
            "the guided setup section drops the WSL2 recommendation",
            "WSL2 remains the recommended Windows route for",
            "Native Windows is the recommended Windows route for",
        ),
        (
            "the quickstart invents a native ARM64 archive",
            "Native Windows ARM64 has no release archive",
            "Native Windows ARM64 has a release archive",
        ),
    ):
        drifted = dict(windows_public_surfaces)
        drifted[QUICKSTART_DOC] = replace_exactly_once(
            quickstart, before, after, label
        )
        expect_assertion(
            label,
            "quickstart platform-specific setup contract",
            lambda mutated=drifted: assert_windows_public_support_contract(
                windows_contract_source,
                install_ps1,
                mutated,
                compatibility_mcp_readme,
            ),
        )
    for stale_claim in (
        "Native Windows always skips repository setup",
        "Native Windows does not launch repository setup",
    ):
        drifted = dict(windows_public_surfaces)
        drifted[QUICKSTART_DOC] = f"{quickstart}\n{stale_claim}.\n"
        expect_assertion(
            "the quickstart restores a blanket Windows setup refusal",
            "quickstart conflates the PowerShell installer",
            lambda mutated=drifted: assert_windows_public_support_contract(
                windows_contract_source,
                install_ps1,
                mutated,
                compatibility_mcp_readme,
            ),
        )

    expect_assertion(
        "the contract script counts stages where one can never appear",
        "reachable Windows stage-leak check",
        lambda: assert_windows_contract_stage_check_is_reachable(
            windows_contract_source.replace(
                'staged="$(count_matching "$parent"',
                'staged="$(count_matching "$dir"',
                1,
            )
        ),
    )
    expect_assertion(
        "the install proof counts stages where one can never appear",
        "shipped Windows admission contract proof",
        lambda: assert_install_proof_windows_admission_contract(
            windows_admission.replace(
                'staged="$(count_matching "$parent"',
                'staged="$(count_matching "$2"',
                1,
            ),
            windows_contract,
        ),
    )
    expect_assertion(
        "the contract script's non-empty refusal wording drifts from the install proof",
        "drifted from",
        lambda: assert_install_proof_windows_admission_contract(
            windows_admission,
            {**windows_contract, "NON_EMPTY_REFUSAL": "requires an empty folder"},
        ),
    )
    for label, active_line in (
        (
            "the non-empty Windows refusal may publish a repository anyway",
            'fail "$1 published authority at $2/.kin" "$3"',
        ),
        (
            "the non-empty Windows refusal may publish a central store anyway",
            'fail "$1 published a store in $home/stores" "$3"',
        ),
        (
            "a Windows admission may publish no store Kin can discover",
            'fail "$1 returned success but no store serves $2" "$3"',
        ),
        (
            "a central Windows admission may leave its unpublished stage behind",
            "staged=\"$(count_matching \"$stores\" '.kin.init-*')\"",
        ),
        (
            "the in-tree layout stops having to succeed",
            'require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree',
        ),
        (
            "a Windows admission may leave its unpublished stage behind",
            "staged=\"$(count_matching \"$parent\" '.kin.init-*')\"",
        ),
        (
            "exact Git stops having to succeed",
            'require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"',
        ),
        (
            "native-empty bootstrap stops having to succeed",
            'require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"',
        ),
        (
            "the non-empty boundary stops being refused",
            'require_refused "Windows non-empty native boundary" "$populated_boundary" "$populated_log"',
        ),
        (
            "the non-empty boundary stops naming its safety reason",
            'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
        ),
    ):
        expect_assertion(
            label,
            "shipped Windows admission contract proof",
            lambda mutated=windows_admission.replace(
                active_line, f"# {active_line}", 1
            ): assert_install_proof_windows_admission_contract(
                mutated, windows_contract
            ),
        )



def check_windows_admission_script() -> None:
    # New release and RC archive builders no longer have an external VFS source
    # input. The package and proof boundary is asserted above, while this later
    # release admission block stays focused on the active shipped runtime.

    # What Windows admission does is stated once and asserted from one script.
    # The installer leg proves it on the landing push, which is the commit
    # release proof reads. The authority job proves the same assertions against
    # the same command on every pull request and merge group, which is the only
    # place an admission change is still reviewable. Pin both invocations and
    # the assertions themselves: dropping the pull-request half restores a
    # behavior change no one can see until it fails a required check on a
    # release commit, and letting the two legs carry their own copies restores
    # the drift that made the copies disagree.
    #
    # Exact Git and native-empty admission are supported. The contract pins
    # both successes, zero transaction residue, and the retained non-empty
    # boundary refusal. A new store is central and the tree carries nothing, so
    # publication is what `kin store path` discovers, and the in-tree layout is
    # admitted once more and must publish the tree's own `.kin`.
    windows_admission = (ROOT / "scripts" / "assert-windows-init-contract.sh").read_text(
        encoding="utf-8"
    )
    for policy in (
        '"$kin_bin" init',
        '"$kin_bin" store path',
        'NON_EMPTY_REFUSAL="requires an empty directory"',
        'require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"',
        'require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree',
        'require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"',
        'require_non_empty_refused',
        'fail "$label returned success but no store serves $dir" "$log"',
        "count_matching \"$home/stores\" '*/manifest.json'",
        'if [ ! -d "$dir/.kin" ]; then',
        'fail "$label unexpectedly succeeded" "$log"',
        'if [ -e "$dir/.kin" ]; then',
        'if [ "$staged" != "0" ]; then',
        "'.kin.init-*'",
    ):
        require(windows_admission, policy, "Windows admission contract assertions")
