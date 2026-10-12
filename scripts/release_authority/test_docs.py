# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that pin the release documentation and the README release claims."""

from __future__ import annotations

import re

from .test_helpers import expect_assertion, require


def assert_readme_latest_release_policy(readme: str, readme_reference: str) -> None:
    """The README must follow the moving latest release, never a pinned tag.

    The badge may be written as Markdown or as the centered HTML header the
    README also uses; either way it must be the static release-latest-6E56CF
    image linking to /releases/latest, never a dynamic shields.io badge that
    tracks whatever tag GitHub currently calls latest. The rewrite moved the
    npm line and the download-prefix mention out of the README itself and
    onto the linked reference page, so those two may live on either page;
    the badge and the pinned-version refusal stay README-only, since that is
    the page whose visible surface they protect.
    """

    # A dotted quad is not a version. `\b\d+\.\d+\.\d+\b` matches "127.0.0" inside
    # "127.0.0.1", so documenting a loopback endpoint tripped this guard with a message
    # about pinning a release. Refuse a match that has a digit or dot on either side.
    pinned_readme_version = re.search(r"(?<![\d.])v?\d+\.\d+\.\d+(?![\d.])", readme)
    if pinned_readme_version:
        raise AssertionError(
            "README must follow the proven latest release instead of pinning "
            f"{pinned_readme_version.group(0)}"
        )
    markdown_badge = "[![Latest release](https://img.shields.io/badge/release-latest-6E56CF.svg)]"
    html_badge = (
        '<a href="https://github.com/kinlab-ai/kin/releases/latest">'
        '<img src="https://img.shields.io/badge/release-latest-6E56CF.svg" '
        'alt="Latest release" /></a>'
    )
    if markdown_badge not in readme and html_badge not in readme:
        raise AssertionError(
            "moving latest-release README reference is missing required policy: "
            "a release-latest-6E56CF.svg badge linking to /releases/latest, "
            "as Markdown or as the HTML <a><img> form"
        )
    require(readme, "https://github.com/kinlab-ai/kin/releases/latest", "moving latest-release README reference")
    for policy in (
        "https://github.com/kinlab-ai/kin/releases/latest/download/",
        "npm install -g @kinlab/kin@latest",
    ):
        if policy not in readme and policy not in readme_reference:
            raise AssertionError(
                "moving latest-release README reference is missing required "
                f"policy: {policy}"
            )
    if "img.shields.io/github/v/release" in readme:
        raise AssertionError(
            "README release badge must follow /releases/latest, not an unpromoted GitHub tag"
        )



def check_release_bot_and_update_trust_docs(
    release_bot_doc: str, update_trust: str
) -> None:
    for policy in (
        "Create the protected `release-tag` Environment",
        "allows **only `main`**",
        "forbids",
        "branch-selectable",
        "`workflow_dispatch`",
        "last commit on the default branch",
        "exists there",
        "only as `release-tag` Environment secrets",
        "any other eligible workflow",
        "repository could explicitly request a broadly scoped secret",
        "Remove or rotate away every",
        "organization-level copy visible",
        "gh api --method POST repos/firelock-ai/kin/dispatches --input -",
        '{event_type:"release_tag",client_payload:{tag:$tag,sha:$sha}}',
        # The abandonment operating rule has to live where an operator editing
        # the record will read it. JSON carries no comments, so the record file
        # itself cannot hold it, and a Python module docstring is not where the
        # person writing an entry is looking.
        "## Abandoning a release tag",
        "record the abandonment and leave the tag in place",
        "The only exit from that state is a hand-landed version bump",
        "a tag that has since moved refuses loudly",
        "fails the rail closed rather",
        # No CI job executes a line of release-recovery.yml: it triggers on
        # schedule and workflow_run only, so this prose is the whole of what an
        # operator has when a reconcile goes green while a release is visibly
        # broken. Each clause is pinned because each is separately droppable:
        # that the stand-down exists, that it is narrow, that recovery is a
        # consumer of the record at all, and that a record too weak for the rail
        # is too weak to quiet the alarm.
        "is reconciled instead of alerted",
        "still opens the issue and still fails the reconcile",
        "automatic recovery stands down for it",
        "cannot quiet the alarm either",
    ):
        require(
            release_bot_doc,
            policy,
            "default-branch-pinned release dispatch runbook",
        )
    if "Recommended hardening (optional)" in release_bot_doc:
        raise AssertionError(
            "release App Environment isolation must be mandatory, not optional"
        )
    if "gh workflow run release-tag.yml" in release_bot_doc:
        raise AssertionError(
            "release break glass must use typed repository_dispatch, not "
            "branch-selectable workflow_dispatch"
        )
    for policy in (
        "last commit on the default branch",
        "exists there",
        "forbids",
        "branch-selectable `workflow_dispatch`",
        "exact current-main payload",
        "must",
        "exist only as Environment secrets",
        "every broader copy must be removed or",
        "production-ready",
    ):
        require(
            update_trust,
            policy,
            "default-branch-pinned release App trust boundary",
        )



def check_readme_latest_release_policy(readme: str, readme_reference: str) -> None:
    assert_readme_latest_release_policy(readme, readme_reference)
    # The guard above has never been seen to fail. Falsify the four ways a
    # README could still slip a pinned release past it: the good badge simply
    # missing (proves the Markdown-or-HTML check itself can fail), the image
    # swapped for a tag-tracking shields.io badge, a pinned x.y.z string added
    # anywhere in the file, and the npm/download policy absent from both the
    # README and the reference page it may now live on instead.
    expect_assertion(
        "the README carries neither the Markdown nor the HTML latest-release badge",
        "is missing required policy",
        lambda: assert_readme_latest_release_policy(
            readme.replace(
                "https://img.shields.io/badge/release-latest-6E56CF.svg",
                "https://example.invalid/no-badge-here",
                1,
            ),
            readme_reference,
        ),
    )
    expect_assertion(
        "the README badge tracks a GitHub tag instead of the moving latest release",
        "must follow /releases/latest, not an unpromoted GitHub tag",
        lambda: assert_readme_latest_release_policy(
            readme + '\n<img src="https://img.shields.io/github/v/release/firelock-ai/kin.svg" />\n',
            readme_reference,
        ),
    )
    expect_assertion(
        "neither the README nor the reference page mentions the npm install line",
        "is missing required policy: npm install -g @kinlab/kin@latest",
        lambda: assert_readme_latest_release_policy(
            readme,
            readme_reference.replace("npm install -g @kinlab/kin@latest", "", 1),
        ),
    )
    expect_assertion(
        "the README pins a release version instead of following latest",
        "must follow the proven latest release instead of pinning",
        lambda: assert_readme_latest_release_policy(
            readme + "\n\nSee kin v0.7.10 for details.\n",
            readme_reference,
        ),
    )
