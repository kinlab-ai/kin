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

# The reason a release built before populated admission gives when it refuses
# a populated directory with no Git history. Releases through v0.8.6 print it.
# The source no longer contains it, so no checked-in script can own it. The
# install proof still installs those releases, and this is the reviewed wording
# its one binding has to repeat.
RELEASED_NON_EMPTY_REFUSAL = "requires an empty directory"


def single_shell_binding(source: str, name: str, owner: str) -> str:
    """Return the one value a shell source assigns to `name`.

    Every assignment counts, not only the reviewed double-quoted form. A later
    `NAME=''` would replace the reviewed wording with a needle every line
    matches, and the text check that reads it would then pass on any output.
    """

    assignments = [
        line
        for line in active_lines(source)
        if re.match(rf"(?:(?:local|export|readonly|declare)\s+)?{name}=", line)
    ]
    if len(assignments) != 1:
        raise AssertionError(f"{owner} must bind {name} exactly once: {assignments}")
    binding = re.fullmatch(rf'{name}="([^"$`\\]+)"', assignments[0])
    if binding is None:
        raise AssertionError(
            f"{owner} must bind {name} to one double-quoted literal: "
            f"{assignments[0]}"
        )
    return binding[1]


def windows_init_contract_strings() -> dict[str, str]:
    """Return the wording the install proof's Windows admission step binds.

    The install proof installs a public release anonymously and has no
    checkout, so it cannot run the contract script and retypes its wording
    instead. `POPULATED_UNCOMMITTED` is shared with that script, which owns it:
    reading the script here is the only thing that keeps the two statements of
    one contract from drifting apart.

    `NON_EMPTY_REFUSAL` is the install proof's alone. It is what a release
    built before populated admission prints, and the script states only what a
    source build does, so `RELEASED_NON_EMPTY_REFUSAL` owns that wording.
    """

    source = WINDOWS_INIT_CONTRACT.read_text(encoding="utf-8")
    return {
        "POPULATED_UNCOMMITTED": single_shell_binding(
            source, "POPULATED_UNCOMMITTED", WINDOWS_INIT_CONTRACT_POLICY
        ),
        "NON_EMPTY_REFUSAL": RELEASED_NON_EMPTY_REFUSAL,
    }


def windows_public_support_notice(install_ps1: str) -> str:
    """Read the one public capability statement every Windows surface repeats.

    `scripts/assert-windows-init-contract.sh` used to bind this notice, back
    when the whole subject of that contract was that native Windows refused
    every repository. The contract now proves the opposite: every boundary it
    exercises is one it must `require_admitted`, a populated directory with no
    Git history included. It binds no notice at all any more, so the
    installer's single `$NativeWindowsSupportNotice` literal is the owner. That
    is the copy a Windows user reads before anything is downloaded, and the
    copy that went stale in public.
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
        'require_admitted "Windows native-populated bootstrap"',
        'fail "$label failed" "$log"',
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


# What the install proof requires a refusal to leave behind, as the step's
# active lines. Pinned whole: its stage count is spelled exactly as the
# admission's is, so only its place in this function says which answer it
# judges.
INSTALL_PROOF_REFUSAL_PUBLISHES_NOTHING = "\n".join(
    (
        "require_nothing_published() {",
        "local home",
        'home="$(fixture_home "$2")"',
        'if [ -e "$2/.kin" ]; then',
        'fail "$1 published authority at $2/.kin" "$3"',
        "fi",
        "if [ \"$(count_matching \"$home/stores\" '*/manifest.json')\" != \"0\" ]; then",
        'fail "$1 published a store in $home/stores" "$3"',
        "fi",
        'if [ -n "$(discovered_store "$2" central "$3")" ]; then',
        'fail "$1 left a store serving $2" "$3"',
        "fi",
        'require_no_stage_residue "$1" "$2" "$3"',
        "}",
    )
)

# The one place the install proof takes either of two answers, as the step's
# active lines. A populated directory with no Git history is admitted by a
# current binary and refused by a release built before that admission, and the
# step installs whichever release is public. Pinned whole, because position is
# the contract here. An assertion moved into the other branch still exists and
# no longer judges the answer the binary gave, and an outcome sentence moved
# across makes the closing line report the contract that was not observed.
INSTALL_PROOF_POPULATED_CONTRACT = "\n".join(
    (
        "require_populated_contract() {",
        'if fixture_kin "$2" central kin init > "$3" 2>&1; then',
        'populated_outcome="admitted it and reported its content as uncommitted work"',
        'echo "$1: kin init succeeded, so the admission contract applies."',
        'require_published "$1" "$2" "$3" central',
        'require_text "$1" "$POPULATED_UNCOMMITTED" "$3"',
        "else",
        'populated_outcome="refused it and published nothing, as a release built before populated admission does"',
        'echo "$1: kin init failed, so the refusal contract of a release built before populated admission applies."',
        'require_nothing_published "$1" "$2" "$3"',
        'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
        "fi",
        "}",
    )
)

# The last thing an admission's publication check does. The call is spelled as
# the refusal's is, so it is pinned by the function it closes.
INSTALL_PROOF_ADMISSION_COUNTS_STAGES = "\n".join(
    (
        'require_no_stage_residue "$1" "$2" "$3"',
        "}",
        "require_admitted() {",
    )
)


def assert_install_proof_windows_admission_contract(
    windows_admission: str, contract: dict[str, str]
) -> None:
    """Require released Windows bytes to assert the native admission contract.

    Exact Git in both layouts and an empty native directory must be admitted.
    A populated directory with no Git history is held to one whole contract,
    whichever the installed release exhibits: admitted as the empty directory
    is, with its content reported as uncommitted work, or refused with nothing
    published and the released refusal's own reason.
    """

    active = "\n".join(active_lines(windows_admission))
    for name, wording in contract.items():
        bound = single_shell_binding(windows_admission, name, "install proof")
        if bound != wording:
            raise AssertionError(
                "install proof must assert the shipped Windows admission contract "
                f"verbatim; {name} drifted from its reviewed wording: "
                f'expected {name}="{wording}"'
            )
    # A new store is central and the tree carries nothing, so publication is
    # what `kin store path` discovers against each fixture's own Kin home, and
    # the in-tree layout is admitted once more and must publish the tree's own
    # `.kin`. Stages are counted where each layout makes them.
    for policy in (
        'require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"',
        'require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree',
        'require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"',
        # Without this the directory is empty and the step proves the
        # native-empty boundary a second time.
        "printf 'untracked\\n' > \"$populated_boundary/stray.txt\"",
        'require_populated_contract "Windows native-populated boundary" "$populated_boundary" "$populated_log"',
        'fail "$1 failed" "$3"',
        'require_published "$1" "$2" "$3" "$layout"',
        'export KIN_HOME="$home"',
        'if ! answer="$(fixture_kin "$1" "$2" kin store path 2>> "$3")"; then',
        'fail "$1 returned success but no store serves $2" "$3"',
        'fail "$1 returned success but the store serving $2 holds no manifest: $store" "$3"',
        "if [ \"$(count_matching \"$home/stores\" '*/manifest.json')\" != \"1\" ]; then",
        'fail "$1 did not publish exactly one store in $home/stores" "$3"',
        'if [ ! -d "$2/.kin" ]; then',
        'fail "$1 wrote $2/.kin although the default layout keeps the working tree clean" "$3"',
        'parent="$(dirname "$2")"',
        "staged=\"$(count_matching \"$parent\" '.kin.init-*')\"",
        "staged=\"$(count_matching \"$stores\" '.kin.init-*')\"",
        'if [ "$staged" != "0" ]; then',
        INSTALL_PROOF_ADMISSION_COUNTS_STAGES,
        INSTALL_PROOF_REFUSAL_PUBLISHES_NOTHING,
        INSTALL_PROOF_POPULATED_CONTRACT,
        # The closing line reports the contract that was observed rather than
        # asserting one of them.
        'echo "Installed Windows release admitted exact Git and native-empty repositories into central stores and exact Git into its own .kin, without stage residue. Given a populated directory with no Git history, it $populated_outcome."',
    ):
        require(active, policy, "shipped Windows admission contract proof")


def assert_windows_admission_script_contract(contract_source: str) -> None:
    """Hold the source-build contract to admission on every boundary.

    The script runs against a binary built from the commit under review, so it
    states what that source does and nothing an earlier release did. A
    populated directory with no Git history must be admitted exactly as the
    empty one is and must report its content as uncommitted work.

    It may not take the refusal an earlier release gives. The install proof
    accepts that refusal from an installed release, which is sound only while
    every source build is required to admit.
    """

    active = "\n".join(active_lines(contract_source))
    wording = single_shell_binding(
        contract_source, "POPULATED_UNCOMMITTED", WINDOWS_INIT_CONTRACT_POLICY
    )
    for policy in (
        '"$kin_bin" init',
        '"$kin_bin" store path',
        f'POPULATED_UNCOMMITTED="{wording}"',
        'require_admitted "Windows exact-Git admission" "$git_boundary" "$git_log"',
        'require_admitted "Windows in-tree exact-Git admission" "$in_tree_boundary" "$in_tree_log" in-tree',
        'require_admitted "Windows native-unborn bootstrap" "$native_boundary" "$native_log"',
        # Without this the directory is empty and the script proves the
        # native-empty boundary a second time.
        "printf 'untracked\\n' > \"$populated_boundary/stray.txt\"",
        'require_admitted "Windows native-populated bootstrap" "$populated_boundary" "$populated_log"',
        'require_text "Windows native-populated bootstrap" "$POPULATED_UNCOMMITTED" "$populated_log"',
        'fail "$label failed" "$log"',
        'fail "$label returned success but no store serves $dir" "$log"',
        'fail "$label returned success but the store serving $dir holds no manifest: $store" "$log"',
        "count_matching \"$home/stores\" '*/manifest.json'",
        'fail "$label did not publish exactly one store in $home/stores" "$log"',
        'if [ ! -d "$dir/.kin" ]; then',
        'if [ -e "$dir/.kin" ]; then',
        'fail "$label wrote $dir/.kin although the default layout keeps the working tree clean" "$log"',
        'require_no_stage_residue "$label" "$dir" "$log"',
        'if [ "$staged" != "0" ]; then',
        "'.kin.init-*'",
    ):
        require(active, policy, "Windows admission contract assertions")
    for refusal in (
        "NON_EMPTY_REFUSAL",
        RELEASED_NON_EMPTY_REFUSAL,
        "unexpectedly succeeded",
    ):
        if refusal in active:
            raise AssertionError(
                "Windows admission contract assertions must require a source "
                "build to admit a populated directory, never accept its "
                f"refusal: found {refusal!r}"
            )



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
    # boundaries and leave no transaction residue. A populated directory with
    # no Git history is held to one whole contract: admitted with its content
    # reported as uncommitted work, or refused the way a release built before
    # that admission refuses it.
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
        "the executable contract reverts to refusing a populated directory",
        "tied to executable admission",
        lambda: assert_windows_public_support_contract(
            replace_exactly_once(
                windows_contract_source,
                'require_admitted "Windows native-populated bootstrap"',
                'require_non_empty_refused "Windows native-populated bootstrap"',
                "populated directory admission",
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
        "the contract script's uncommitted-work wording drifts from the install proof",
        "drifted from",
        lambda: assert_install_proof_windows_admission_contract(
            windows_admission,
            {**windows_contract, "POPULATED_UNCOMMITTED": "Pending work:"},
        ),
    )
    expect_assertion(
        "the install proof rewords the refusal a released binary gives",
        "drifted from",
        lambda: assert_install_proof_windows_admission_contract(
            replace_exactly_once(
                windows_admission,
                f'NON_EMPTY_REFUSAL="{RELEASED_NON_EMPTY_REFUSAL}"',
                'NON_EMPTY_REFUSAL="requires an empty folder"',
                "released refusal wording",
            ),
            windows_contract,
        ),
    )
    for name, wording in windows_contract.items():
        binding = f'{name}="{wording}"'
        expect_assertion(
            f"the install proof rebinds {name} after its reviewed binding",
            f"must bind {name} exactly once",
            lambda mutated=replace_exactly_once(
                windows_admission,
                binding,
                f"{binding}\n{name}=''",
                f"second {name} binding",
            ): assert_install_proof_windows_admission_contract(
                mutated, windows_contract
            ),
        )
        expect_assertion(
            f"the install proof binds {name} to a needle every line matches",
            f"must bind {name} to one double-quoted literal",
            lambda mutated=replace_exactly_once(
                windows_admission, binding, f"{name}=''", f"empty {name} binding"
            ): assert_install_proof_windows_admission_contract(
                mutated, windows_contract
            ),
        )
    for label, active_line in (
        (
            "a refused populated directory may publish a repository anyway",
            'fail "$1 published authority at $2/.kin" "$3"',
        ),
        (
            "a refused populated directory may publish a central store anyway",
            'fail "$1 published a store in $home/stores" "$3"',
        ),
        (
            "a refused populated directory may leave a store Kin can discover",
            'fail "$1 left a store serving $2" "$3"',
        ),
        (
            "a Windows admission may publish no store Kin can discover",
            'fail "$1 returned success but no store serves $2" "$3"',
        ),
        (
            "a Windows admission may publish a store that holds no manifest",
            'fail "$1 returned success but the store serving $2 holds no manifest: $store" "$3"',
        ),
        (
            "a central Windows admission may publish more or fewer stores than one",
            'fail "$1 did not publish exactly one store in $home/stores" "$3"',
        ),
        (
            "a central Windows admission may write a store into the working tree",
            'fail "$1 wrote $2/.kin although the default layout keeps the working tree clean" "$3"',
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
            "a Windows admission may fail and still pass",
            'fail "$1 failed" "$3"',
        ),
        (
            "a Windows admission stops judging what it published",
            'require_published "$1" "$2" "$3" "$layout"',
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
            "the populated directory is left empty",
            "printf 'untracked\\n' > \"$populated_boundary/stray.txt\"",
        ),
        (
            "the populated directory stops being held to either contract",
            'require_populated_contract "Windows native-populated boundary" "$populated_boundary" "$populated_log"',
        ),
        (
            "an admitted populated directory stops having to publish as an admission does",
            'require_published "$1" "$2" "$3" central',
        ),
        (
            "an admitted populated directory stops having to report its uncommitted work",
            'require_text "$1" "$POPULATED_UNCOMMITTED" "$3"',
        ),
        (
            "a refused populated directory stops having to publish nothing",
            'require_nothing_published "$1" "$2" "$3"',
        ),
        (
            "a refused populated directory stops having to give the released refusal's reason",
            'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
        ),
    ):
        expect_assertion(
            label,
            "shipped Windows admission contract proof",
            lambda mutated=replace_exactly_once(
                windows_admission, active_line, f"# {active_line}", label
            ): assert_install_proof_windows_admission_contract(
                mutated, windows_contract
            ),
        )
    # The stage count is spelled the same way after an admission and after a
    # refusal, so each call is removed by its position.
    stage_count = 'require_no_stage_residue "$1" "$2" "$3"'
    if windows_admission.count(stage_count) != 2:
        raise AssertionError(
            "install proof must count unpublished stages once after an "
            "admission and once after a refusal"
        )
    before_refusal, _, after_refusal = windows_admission.rpartition(stage_count)
    for label, mutated in (
        (
            "an admitted directory may leave its unpublished stage uncounted",
            windows_admission.replace(stage_count, f"# {stage_count}", 1),
        ),
        (
            "a refused populated directory may leave its unpublished stage uncounted",
            f"{before_refusal}# {stage_count}{after_refusal}",
        ),
    ):
        expect_assertion(
            label,
            "shipped Windows admission contract proof",
            lambda mutated=mutated: assert_install_proof_windows_admission_contract(
                mutated, windows_contract
            ),
        )
    # Each of these keeps every assertion in the step. It puts one where it
    # judges the wrong answer, or makes the step report a contract it did not
    # observe.
    for label, before, after in (
        (
            "the populated step takes a failed kin init for the admission",
            'if fixture_kin "$2" central kin init > "$3" 2>&1; then',
            'if ! fixture_kin "$2" central kin init > "$3" 2>&1; then',
        ),
        (
            "the populated step checks an admission for the refusal's wording",
            'require_text "$1" "$POPULATED_UNCOMMITTED" "$3"',
            'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
        ),
        (
            "the populated step checks a refusal for the admission's wording",
            'require_text "$1" "$NON_EMPTY_REFUSAL" "$3"',
            'require_text "$1" "$POPULATED_UNCOMMITTED" "$3"',
        ),
        (
            "the populated step judges a refusal as though it had published",
            'require_nothing_published "$1" "$2" "$3"',
            'require_published "$1" "$2" "$3" central',
        ),
        (
            "the populated step reports an admission as a refusal",
            'populated_outcome="admitted it and reported its content as uncommitted work"',
            'populated_outcome="refused it and published nothing"',
        ),
        (
            "the closing line claims one contract whichever was observed",
            'with no Git history, it $populated_outcome."',
            'with no Git history, it admitted it."',
        ),
    ):
        expect_assertion(
            label,
            "shipped Windows admission contract proof",
            lambda mutated=replace_exactly_once(
                windows_admission, before, after, label
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
    # Exact Git, native-empty and native-populated admission are supported. The
    # contract pins all three successes, zero transaction residue, and the
    # report that a populated directory's content is waiting as uncommitted
    # work. A new store is central and the tree carries nothing, so publication
    # is what `kin store path` discovers, and the in-tree layout is admitted
    # once more and must publish the tree's own `.kin`.
    windows_admission = (ROOT / "scripts" / "assert-windows-init-contract.sh").read_text(
        encoding="utf-8"
    )
    assert_windows_admission_script_contract(windows_admission)
    for label, active_line in (
        (
            "the source-build contract stops admitting a populated directory",
            'require_admitted "Windows native-populated bootstrap" "$populated_boundary" "$populated_log"',
        ),
        (
            "the source-build contract stops requiring the uncommitted-work report",
            'require_text "Windows native-populated bootstrap" "$POPULATED_UNCOMMITTED" "$populated_log"',
        ),
        (
            "the source-build contract lets a failed admission pass",
            'fail "$label failed" "$log"',
        ),
        (
            "the source-build contract leaves the populated directory empty",
            "printf 'untracked\\n' > \"$populated_boundary/stray.txt\"",
        ),
        (
            "the source-build contract may admit with no store Kin can discover",
            'fail "$label returned success but no store serves $dir" "$log"',
        ),
        (
            "the source-build contract may admit into a store that holds no manifest",
            'fail "$label returned success but the store serving $dir holds no manifest: $store" "$log"',
        ),
        (
            "the source-build contract may publish more or fewer central stores than one",
            'fail "$label did not publish exactly one store in $home/stores" "$log"',
        ),
        (
            "the source-build contract may write a store into the working tree",
            'fail "$label wrote $dir/.kin although the default layout keeps the working tree clean" "$log"',
        ),
        (
            "the source-build contract stops counting unpublished stages",
            'require_no_stage_residue "$label" "$dir" "$log"',
        ),
    ):
        expect_assertion(
            label,
            "Windows admission contract assertions is missing required policy",
            lambda mutated=replace_exactly_once(
                windows_admission, active_line, f"# {active_line}", label
            ): assert_windows_admission_script_contract(mutated),
        )
    expect_assertion(
        "the source-build contract accepts the refusal an earlier release gives",
        "never accept its refusal",
        lambda: assert_windows_admission_script_contract(
            f'{windows_admission}\nNON_EMPTY_REFUSAL="{RELEASED_NON_EMPTY_REFUSAL}"\n'
        ),
    )
    expect_assertion(
        "the source-build contract binds its uncommitted-work wording twice",
        "must bind POPULATED_UNCOMMITTED exactly once",
        lambda: assert_windows_admission_script_contract(
            f"{windows_admission}\nPOPULATED_UNCOMMITTED=''\n"
        ),
    )
