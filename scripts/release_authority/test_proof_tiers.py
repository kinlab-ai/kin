# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""The two-tier proof contract across release-cut, release-tag and release.

A release needs the machine proof. A missing first-contact record does not
hold it back, but every place that labels the release has to say so.
"""

from __future__ import annotations

import re
from pathlib import Path

from .test_helpers import (
    RELEASE_GATE_FIXTURE_SHA,
    WORKFLOWS,
    active_lines,
    execute_tag_readback,
    expect_assertion,
    tag_readback_source,
    workflow_job_blocks,
)


# ── the two-tier proof contract ───────────────────────────────────────────
#
# Until 2026-09-02 both decision points required the same thing: a preflight
# record AND a complete stranger record, or no release at all. That made every
# release hostage to a driver, and on run rc0551 the account's weekly limit
# turned a proven candidate into a tag that could not be cut, with a hold alarm
# as the only signal.
#
# The contract now has two tiers, and these assertions are what stop the tiers
# collapsing back into one in either direction. Collapsing UP puts the outage
# back: a release the stranger cannot reach is held off Latest indefinitely
# while every machine signal is green.
#
# Collapsing DOWN is subtler than it was, because promoting without a stranger
# record is now the INTENDED behaviour. What must not collapse is the label. The
# 2026-08-20 failure was a release promoted to Latest with no PREFLIGHT record
# and no stranger run on its bytes, and nothing anywhere saying so; the machine
# proof is still required and the missing first-contact record still has to be
# stated on the release, in the mint's summary and in the promote summary. A
# release that is unmeasured and silent about it is the failure. A release that
# is unmeasured and says so is the design.
#
# Every assertion reads active lines, never raw text, because each of these
# files carries prose ABOUT the thing being asserted and a check satisfied by
# its own explanatory comment cannot fail.
TAG_TIER_MODE = "KIN_RELEASE_REQUIRE=preflight"
PROMOTION_STATE = "complete"


def assert_tag_tier_requires_only_the_machine_proof(release_tag: str) -> None:
    """The mint must ask for the preflight tier, so a missing stranger cannot hold a tag."""

    lines = active_lines(release_tag)
    if not any(TAG_TIER_MODE in line for line in lines):
        raise AssertionError(
            "release-tag.yml must run the proof gate with "
            f"{TAG_TIER_MODE}; without it a candidate whose stranger has not "
            "run yet cannot be tagged at all, which is the outage this tier "
            "exists to end"
        )
    # Reading the state is not optional. A mint that relaxed the requirement and
    # then said nothing would cut tags that silently claim nothing, and the
    # release would carry no record of what it is missing.
    if not any("stranger_state=" in line for line in lines):
        raise AssertionError(
            "release-tag.yml must record the stranger state it proceeded on"
        )


# ── the mint's candidate listing ──────────────────────────────────────────
#
# The tier above is what the proof GATE requires. This is what the mint ever
# OFFERS it, and until 2026-09-04 the two disagreed. The gate ran the preflight
# tier while the listing proposed only a sha carrying BOTH records, so a
# preflight-only candidate never reached the gate that would have admitted it,
# and the mint reported "no candidate" and exited 0. That path is a deliberate
# no-op rather than a decline, so it raises nothing: half the rail was relaxed
# and the half that picks the sha was not, silently. Nothing guarded the
# listing expression at all before this assertion.
CANDIDATE_LISTING_CAPTURE = 'capture("^evidence/'
MINT_FIRST_CONTACT_ROW = "| first contact |"


def assert_the_mint_lists_preflight_only_candidates(release_tag: str) -> None:
    """A missing stranger must label a release, never hide its candidate.

    Three things together, because any one of them alone ships inert: the
    listing has to propose a preflight-only sha, the mint has to read back the
    stranger state it computes, and it has to write that state where a person
    reading the mint run finds it.
    """

    lines = active_lines(release_tag)
    listing = [line for line in lines if CANDIDATE_LISTING_CAPTURE in line]
    if len(listing) != 1:
        raise AssertionError(
            "release-tag.yml must build its candidate list from exactly one "
            f"evidence path capture; found {len(listing)}"
        )
    if "preflight" not in listing[0]:
        raise AssertionError(
            "release-tag.yml's candidate listing no longer keys on preflight.json"
        )
    if "stranger" in listing[0]:
        raise AssertionError(
            "release-tag.yml's candidate listing requires a stranger record "
            "again, so a candidate whose stranger has not run is never offered "
            "to the gate that would admit it and the mint no-ops in silence. "
            "The rule is that a missing stranger labels a release; it does not "
            "hold one"
        )
    if not any("steps.proof.outputs.stranger_state" in line for line in lines):
        raise AssertionError(
            "release-tag.yml computes a stranger state and reads it back "
            "nowhere, so a release minted without first-contact proof is "
            "indistinguishable from one that has it"
        )


def assert_the_hosted_stranger_never_reddens_the_rail(release_cut: str) -> None:
    """A nice-to-have must not be able to fail a release run.

    Founder decision, 2026-09-04: "the damn smoke should never block release it
    is a nice to have ONLY ... it should be mainly run locally not in the CI."
    A driver out of limit, an offline runner or a container that would not start
    is not a fact about the candidate, and a red cut run for any of them is a
    rail that looks broken while behaving exactly as designed.

    The second half is the trap that makes the first half dangerous.
    `continue-on-error` masks `needs.<job>.result` to 'success' for every
    downstream job, including one whose upstream failed. The record publisher
    therefore has to key on the stranger job's own output, which is written only
    after a record exists on disk. Keyed on `.result` under this flag it would
    run over a failed arm and publish nothing while reporting success.
    """

    jobs = workflow_job_blocks(release_cut)
    for name in ("stranger", "publish-stranger"):
        if name not in jobs:
            raise AssertionError(f"release-cut.yml no longer declares a {name} job")
    block = jobs["stranger"]
    if "continue-on-error: true" not in block:
        raise AssertionError(
            "release-cut.yml's stranger job can redden the cut run; it is a "
            "nice-to-have and must carry continue-on-error: true"
        )

    publisher = "\n".join(active_lines(jobs["publish-stranger"].split("\n    steps:", 1)[0]))
    if "needs.stranger.result" in publisher:
        raise AssertionError(
            "publish-stranger keys on needs.stranger.result, which "
            "continue-on-error masks to 'success' even for a failed stranger, "
            "so it would publish over an arm that produced no record"
        )
    if "needs.stranger.outputs.recorded == 'true'" not in publisher:
        raise AssertionError(
            "publish-stranger must key on the stranger job's own recorded "
            f"output, which is written only after a record exists: {publisher}"
        )

    # Silence is the other way a nice-to-have becomes invisible. A job allowed
    # to fail has to say what it left behind, in the word this rail acts on.
    if "pending" not in "\n".join(active_lines(block)).lower():
        raise AssertionError(
            "release-cut.yml's stranger job never names the pending state, so a "
            "cycle that measured nothing leaves no record saying so"
        )


def assert_the_stranger_labels_a_release_and_never_holds_it(release: str) -> None:
    """First contact is a label on the release, not a gate in front of it.

    Until 2026-09-04 GitHub Latest required the stranger record, and the
    outage that bought was real: an offline runner or a spent weekly limit kept
    a finished, installable release off /releases/latest while every machine
    signal was green. The founder's ruling is to ship anyway and say what is
    missing. These assertions hold both halves at once, because either one
    alone is a different failure. Losing the hold and the label together
    promotes an unmeasured release silently, which is the 2026-08-20 failure.
    Keeping the hold puts the outage back.
    """

    lines = active_lines(release)
    if any("promoted=false" in line for line in lines):
        raise AssertionError(
            "release.yml holds a release back from GitHub Latest again; first "
            "contact is a label, not a gate, and a stranger that cannot run "
            "must not keep a finished release off the channel an installer "
            "resolves"
        )
    if not any("promoted=true" in line for line in lines):
        raise AssertionError(
            "release.yml must record promoted=true when it promotes, so a "
            "downstream job can tell a decision from a step that never ran"
        )
    labelled = f'"$STRANGER_STATE" != {PROMOTION_STATE}'
    if not any(labelled in line for line in lines):
        raise AssertionError(
            "release.yml must still branch on the stranger state to label the "
            "release it promotes; a promotion that stops reading the state "
            "cannot say which releases were measured, and unmeasured then "
            "looks exactly like measured"
        )
    # The pending notice is what carries the label onto the release itself, and
    # it is for STABLE releases only. A prerelease tag is a prerelease because
    # that is what it is, and it never becomes Latest whatever its record says,
    # so the notice would answer a question nobody installing an rc is asking.
    anchor = "      - name: Record the missing first-contact proof on the release itself\n"
    if release.count(anchor) != 1:
        raise AssertionError(
            "release.yml must declare exactly one step recording a missing "
            "first-contact proof on the release"
        )
    start = release.index(anchor)
    notice = release[start : release.index("\n      - name: ", start + len(anchor))]
    condition = "\n".join(active_lines(notice))
    for required in ("stranger_state == 'pending'", "!contains(github.ref_name, '-')"):
        if required not in condition:
            raise AssertionError(
                "the first-contact notice must be written only for a pending "
                f"STABLE release; its condition is missing {required}"
            )


def assert_latest_claims_gate_on_promotion(release: str) -> None:
    """Every job whose meaning is "this IS Latest" must gate on the promotion, not the job.

    `finalize_release` succeeding is not the same fact as this tag having been
    promoted. The promote step does not run at all on a prerelease tag, and a
    job that read the job's result instead of its output would move a mutable
    `latest` alias for a release that never became Latest.
    """

    needed = "needs.finalize_release.outputs.promoted == 'true'"
    for job in ("promote_ghcr_latest", "seal_release_completion"):
        start = release.index(f"\n  {job}:")
        end = release.index("\n    steps:", start)
        if needed not in "\n".join(active_lines(release[start:end])):
            raise AssertionError(
                f"{job} must gate on {needed}; it claims that this release IS "
                "Latest, which finalize_release succeeding on its own does not "
                "say"
            )


def assert_require_modes_match_the_gate(
    release_tag: str, release: str, proof_gate: str
) -> None:
    """The mode both workflows ask for must be one the gate module actually knows.

    This is the coupling that would otherwise fail on a release night. A mode
    renamed in the module and not in the workflows refuses at run time with
    "unknown require mode", which is correct behaviour and a stopped release.
    """

    declared = re.search(
        r"export const REQUIRE_MODES = Object\.freeze\(\[([^\]]*)\]\)",
        proof_gate,
    )
    if declared is None:
        raise AssertionError(
            "the proof gate must declare REQUIRE_MODES for the workflows to be "
            "checked against"
        )
    modes = set(re.findall(r"'([^']+)'", declared.group(1)))
    asked = set()
    for source in (release_tag, release):
        for line in active_lines(source):
            found = re.search(r"KIN_RELEASE_REQUIRE=([A-Za-z-]+)", line)
            if found:
                asked.add(found.group(1))
    if not asked:
        raise AssertionError(
            "no workflow asks the proof gate for a require mode, so this "
            "coupling checks nothing"
        )
    unknown = sorted(asked - modes)
    if unknown:
        raise AssertionError(
            f"the release workflows ask the proof gate for mode(s) {unknown} "
            f"that it does not declare (it knows {sorted(modes)}); move both "
            "sides together"
        )


def assert_stranger_driver_credential_is_either(release_cut: str) -> None:
    """The cut must accept a local model OR a key, and must still refuse with neither.

    The key is required only on the account path. Naming a local model points
    the driver at an endpoint on the runner and `driver_env_argv` then UNSETS
    ANTHROPIC_API_KEY for the driver child, so demanding a secret the run
    throws away is how a rail stays blocked on a founder step it does not need.

    Both halves are asserted, because the refusal is the load-bearing one: an
    absent credential sends every driver request out unauthenticated and the
    transcript records only a server error, hours in.
    """

    lines = active_lines(release_cut)
    joined = "\n".join(lines)
    if "vars.KIN_STRANGER_LOCAL_MODEL" not in joined:
        raise AssertionError(
            "release-cut.yml must let a local model drive the stranger; without "
            "it the cut demands an API key the local path immediately discards"
        )
    if '--local-model' not in joined:
        raise AssertionError(
            "release-cut.yml names a local model and never passes --local-model "
            "to the harness, so the run would drive the account anyway"
        )
    # The refusal must still exist, and it must still be reachable when neither
    # credential is configured.
    if 'if [ -n "${LOCAL_MODEL:-}" ]; then' not in joined:
        raise AssertionError(
            "release-cut.yml must branch on the local model before it demands a key"
        )
    if 'elif [ -z "${STRANGER_KEY:-}" ]; then' not in joined:
        raise AssertionError(
            "release-cut.yml must still refuse when NEITHER a local model nor a "
            "key is configured; a driver with no credential fails hours in with "
            "a bare server error"
        )


# The `release` environment's deployment branch policy admits tag: v*.*.* and
# nothing else, so a job that reaches it from a branch is refused by the server
# before a step runs. That is invisible in the workflow file and fatal on a
# schedule, where the refusal repeats every cycle forever.
#
# The policies themselves live in repository settings and cannot be read from
# the tree, so this asserts the rule the tree CAN carry: a workflow that fires
# on a schedule, a repository_dispatch or a push to a branch runs from a branch,
# and must not declare the tag-only environment. The push clause matters since
# release-cut.yml wakes on the landing push to arm an inherited candidate at
# once; a tag push, which is how release.yml runs, is not a branch.
TAG_ONLY_ENVIRONMENT = "release"
BRANCH_ENVIRONMENT = "release-tag"


def fires_from_a_branch(head: str) -> bool:
    """`head` is a workflow's text before `jobs:`."""

    if "schedule:" in head or "repository_dispatch:" in head:
        return True
    push = re.search(r"(?m)^  push:\n((?:(?:    .*|\s*#.*|\s*)\n)*)", head)
    return bool(push and re.search(r"(?m)^    branches(?:-ignore)?:", push.group(1)))


def assert_branch_triggered_workflows_avoid_the_tag_environment(
    workflows: dict[Path, str],
) -> None:
    for path, source in workflows.items():
        head = source.split("\njobs:", 1)[0]
        if not fires_from_a_branch(head):
            continue
        for line in active_lines(source):
            if line == f"environment: {TAG_ONLY_ENVIRONMENT}":
                raise AssertionError(
                    f"{path.name} can fire from a branch and declares "
                    f"environment: {TAG_ONLY_ENVIRONMENT}, whose deployment "
                    f"branch policy admits tags alone; use "
                    f"{BRANCH_ENVIRONMENT}, which admits main, or the job is "
                    "refused before any step runs"
                )


def assert_the_mint_records_the_state_it_tagged_on(release_tag: str) -> None:
    """A release minted without first-contact proof must say so in the mint's record.

    Run the real readback step twice, once per state the proof gate can report,
    and read the step summary it wrote. Two identical summaries mean the mint
    tagged on the machine preflight alone and left no trace of it, which is the
    gap this whole change exists to close: the mint has computed this state
    since 2026-09-02 and, until 2026-09-04, wrote it only to GITHUB_OUTPUT,
    where nothing in the workflow read it back.
    """

    source = tag_readback_source(release_tag)
    readable = (0, RELEASE_GATE_FIXTURE_SHA)

    held: list[str] = []
    held_run = execute_tag_readback(
        source, [readable], stranger_state="pending", summary=held
    )
    if held_run.returncode != 0:
        raise AssertionError(
            "the mint failed on a stranger-pending tag, which is the release "
            f"the founder asked to ship: {held_run.stdout}{held_run.stderr}"
        )
    proven: list[str] = []
    proven_run = execute_tag_readback(
        source,
        [readable],
        stranger_state="complete",
        stranger_arms="green+brown+vcs",
        stranger_driver="account",
        summary=proven,
    )
    if proven_run.returncode != 0:
        raise AssertionError(
            f"the mint failed on a fully proven tag: {proven_run.stdout}{proven_run.stderr}"
        )

    # Read the ROW, not the whole summary. Asserting on the summary text lets a
    # prose sentence elsewhere in it satisfy the check: the complete branch's
    # own claim sentence contains the word "pending" (it says the release does
    # NOT carry a pending notice), which silently defeated a whole-summary
    # search for that word and was caught only by this function's own
    # falsification arm.
    def row(summary: str) -> str:
        rows = [
            line for line in summary.splitlines() if MINT_FIRST_CONTACT_ROW in line
        ]
        if len(rows) != 1:
            raise AssertionError(
                "the minted-tag summary must carry exactly one first-contact row "
                f"({MINT_FIRST_CONTACT_ROW}); the step summary is the mint's own "
                f"record of what the tag it just created may claim: {summary}"
            )
        return rows[0]

    held_row, proven_row = row(held[0]), row(proven[0])
    if held_row == proven_row:
        raise AssertionError(
            "the mint writes the same record whether or not the tag carries "
            "first-contact proof, so a release minted on the machine preflight "
            "alone is indistinguishable from a proven one"
        )
    if "pending" not in held_row:
        raise AssertionError(
            f"the minted-tag summary must name a pending first contact: {held_row}"
        )
    if "pending" in proven_row:
        raise AssertionError(
            f"the minted-tag summary calls a proven tag pending: {proven_row}"
        )
    if "green+brown+vcs" not in proven_row:
        raise AssertionError(
            f"the minted-tag summary must name the arms that proved it: {proven_row}"
        )



QUEUE_STATUS_CONTEXT = "Release preflight"


def assert_the_queue_status_never_stands_in_for_the_record(release_tag: str, release_cut: str) -> None:
    """The merge queue waits on a commit status; the mint never reads it.

    release-cut.yml posts "Release preflight" on a merge group commit so the
    queue lands a release only once its preflight record is filed. Any workflow
    holding statuses: write could post that name, so it may decide WHEN a queue
    lands and never WHAT is minted: the mint reads the filed record through its
    proof gate, and a success is posted only after the publish job filed one.
    """

    if any(QUEUE_STATUS_CONTEXT in line for line in active_lines(release_tag)):
        raise AssertionError(
            f"release-tag.yml reads the {QUEUE_STATUS_CONTEXT!r} commit status, which any "
            "statuses: write token can post; the mint must read the filed preflight record instead"
        )
    report = workflow_job_blocks(release_cut).get("report-to-queue", "")
    needs = re.search(r"(?m)^    needs: \[(.*)\]$", report)
    if needs is None or "publish" not in [name.strip() for name in needs.group(1).split(",")]:
        raise AssertionError("the queue report must wait for the publish job whose record it announces")
    if 'if [ "$PUBLISH" = success ]; then' not in report:
        raise AssertionError("the queue report may post success for a proof only once the record was filed")


def check_two_tier_proof_contract(
    proof_gate: str,
    release: str,
    release_cut: str,
    release_tag: str,
    workflow_sources: dict[Path, str],
) -> None:
    # The two-tier proof contract, and a falsification arm per direction it
    # could collapse. Each mutant is the smallest edit that would actually be
    # made by someone "tidying up", not a syntactic wreck.
    assert_branch_triggered_workflows_avoid_the_tag_environment(workflow_sources)
    expect_assertion(
        "a scheduled workflow reaches the tag-only release environment",
        "whose deployment branch policy admits tags alone",
        lambda mutant={
            **workflow_sources,
            WORKFLOWS / "release-promote.yml": (
                WORKFLOWS / "release-promote.yml"
            ).read_text(encoding="utf-8").replace(
                "environment: release-tag", "environment: release", 1
            ),
        }: assert_branch_triggered_workflows_avoid_the_tag_environment(mutant),
    )
    # docker.yml fires on a push to main and on nothing scheduled, so only the
    # push clause can see it reach for the tag-only environment.
    docker = (WORKFLOWS / "docker.yml").read_text(encoding="utf-8")
    docker_job = docker.split("\njobs:", 1)[1]
    expect_assertion(
        "a branch-push workflow reaches the tag-only release environment",
        "whose deployment branch policy admits tags alone",
        lambda mutant={
            **workflow_sources,
            WORKFLOWS / "docker.yml": docker.replace(
                docker_job,
                docker_job.replace(
                    "    runs-on: ubuntu-latest\n",
                    "    environment: release\n    runs-on: ubuntu-latest\n",
                    1,
                ),
                1,
            ),
        }: assert_branch_triggered_workflows_avoid_the_tag_environment(mutant),
    )
    if fires_from_a_branch((WORKFLOWS / "release.yml").read_text(encoding="utf-8").split("\njobs:", 1)[0]):
        raise AssertionError("release.yml runs on a tag push, and a tag is not a branch")
    assert_the_queue_status_never_stands_in_for_the_record(release_tag, release_cut)
    expect_assertion(
        "the mint starts trusting the queue's commit status",
        "the mint must read the filed preflight record instead",
        lambda mutant=release_tag.replace(
            "            Windows installer + vector release build\n            PR text hygiene\n",
            "            Windows installer + vector release build\n            Release preflight\n            PR text hygiene\n",
            1,
        ): assert_the_queue_status_never_stands_in_for_the_record(mutant, release_cut),
    )
    expect_assertion(
        "the queue report stops waiting for the record it announces",
        "must wait for the publish job",
        lambda mutant=release_cut.replace(
            "    needs: [select, preflight, publish]\n    if: always() && needs.select.outputs.queue_sha != ''",
            "    needs: [select, preflight]\n    if: always() && needs.select.outputs.queue_sha != ''",
            1,
        ): assert_the_queue_status_never_stands_in_for_the_record(release_tag, mutant),
    )
    assert_stranger_driver_credential_is_either(release_cut)
    expect_assertion(
        "the cut demands an API key even when a local model drives",
        "must let a local model drive the stranger",
        lambda mutant=release_cut.replace("vars.KIN_STRANGER_LOCAL_MODEL", "vars.KIN_STRANGER_UNUSED"): (
            assert_stranger_driver_credential_is_either(mutant)
        ),
    )
    expect_assertion(
        "the cut accepts a driverless run with no credential at all",
        "must still refuse when NEITHER",
        lambda mutant=release_cut.replace(
            'elif [ -z "${STRANGER_KEY:-}" ]; then', 'elif false; then', 1
        ): assert_stranger_driver_credential_is_either(mutant),
    )
    assert_tag_tier_requires_only_the_machine_proof(release_tag)
    assert_the_mint_lists_preflight_only_candidates(release_tag)
    expect_assertion(
        "the mint goes back to listing only fully evidenced candidates",
        "requires a stranger record again",
        lambda mutant=release_tag.replace(
            r'map(capture("^evidence/(?<sha>[0-9a-f]{40})/preflight\\.json$") | .sha)'
            "\n              | unique",
            r'map(capture("^evidence/(?<sha>[0-9a-f]{40})/(?<name>preflight\\.json|stranger\\.env)$"))'
            "\n              | group_by(.sha)"
            "\n              | map(select((map(.name) | unique | length) == 2) | .[0].sha)",
            1,
        ): assert_the_mint_lists_preflight_only_candidates(mutant),
    )
    expect_assertion(
        "the mint computes a stranger state and reads it back nowhere",
        "reads it back",
        lambda mutant=release_tag.replace(
            "          STRANGER_STATE: ${{ steps.proof.outputs.stranger_state }}\n",
            "",
            1,
        ): assert_the_mint_lists_preflight_only_candidates(mutant),
    )
    assert_the_mint_records_the_state_it_tagged_on(release_tag)
    expect_assertion(
        "the minted-tag summary drops its first-contact row",
        "must carry exactly one first-contact row",
        lambda mutant=release_tag.replace(
            '            echo "| first contact | ${first_contact} |"\n', "", 1
        ): assert_the_mint_records_the_state_it_tagged_on(mutant),
    )
    expect_assertion(
        "the mint calls every tag proven regardless of the state it read",
        "must name a pending first contact",
        lambda mutant=release_tag.replace(
            '          if [ "$STRANGER_STATE" = complete ]; then\n',
            '          if true; then\n',
            1,
        ): assert_the_mint_records_the_state_it_tagged_on(mutant),
    )
    expect_assertion(
        "the mint writes one fixed record for both first-contact states",
        "indistinguishable from a proven one",
        lambda mutant=release_tag.replace(
            '            echo "| first contact | ${first_contact} |"\n',
            '            echo "| first contact | recorded |"\n',
            1,
        ): assert_the_mint_records_the_state_it_tagged_on(mutant),
    )
    assert_the_hosted_stranger_never_reddens_the_rail(release_cut)
    # Scoped to the stranger job's own block, not a whole-file replace: the dispatch fix
    # gave the arm job a second, step-level `continue-on-error: true` (for the
    # dispatch-token mint), which sorts earlier in the file and contains this
    # exact 4-space-indented needle as a substring of its own 8-space line, so
    # an unanchored `release_cut.replace(..., 1)` strips the wrong one and this
    # falsification stops failing for the right reason.
    stranger_block = workflow_job_blocks(release_cut)["stranger"]
    expect_assertion(
        "the hosted stranger can fail a cut run again",
        "must carry continue-on-error: true",
        lambda mutant=release_cut.replace(
            stranger_block,
            stranger_block.replace("    continue-on-error: true\n", "", 1),
            1,
        ): assert_the_hosted_stranger_never_reddens_the_rail(mutant),
    )
    expect_assertion(
        "the record publisher trusts a result continue-on-error has masked",
        "which continue-on-error masks",
        lambda mutant=release_cut.replace(
            "needs.stranger.outputs.recorded == 'true'",
            "needs.stranger.result == 'success'",
            1,
        ): assert_the_hosted_stranger_never_reddens_the_rail(mutant),
    )
    assert_the_stranger_labels_a_release_and_never_holds_it(release)
    assert_latest_claims_gate_on_promotion(release)
    assert_require_modes_match_the_gate(release_tag, release, proof_gate)
    expect_assertion(
        "the mint goes back to requiring a stranger before a tag",
        "must run the proof gate with",
        lambda mutant=release_tag.replace(
            "            KIN_RELEASE_REQUIRE=preflight \\\n", "", 1
        ): assert_tag_tier_requires_only_the_machine_proof(mutant),
    )
    expect_assertion(
        "GitHub Latest waits on the stranger again",
        "holds a release back from GitHub Latest again",
        lambda mutant=release.replace(
            '            echo "### Promoted with first contact ${STRANGER_STATE}"\n',
            '            echo "promoted=false" >> "$GITHUB_OUTPUT"\n',
            1,
        ): assert_the_stranger_labels_a_release_and_never_holds_it(mutant),
    )
    expect_assertion(
        "promotion stops reading the state, so nothing is labelled",
        "must still branch on the stranger state",
        lambda mutant=release.replace(
            'if [ "$STRANGER_STATE" != complete ]; then',
            'if [ "$GITHUB_REF_NAME" = never ]; then',
            1,
        ): assert_the_stranger_labels_a_release_and_never_holds_it(mutant),
    )
    expect_assertion(
        "the pending notice is written onto a prerelease tag too",
        "written only for a pending",
        lambda mutant=release.replace(
            "            steps.proof.outputs.stranger_state == 'pending' &&\n"
            "            !contains(github.ref_name, '-')\n",
            "            steps.proof.outputs.stranger_state == 'pending'\n",
            1,
        ): assert_the_stranger_labels_a_release_and_never_holds_it(mutant),
    )
    expect_assertion(
        "GHCR latest moves on a release that was never promoted",
        "must gate on needs.finalize_release.outputs.promoted",
        lambda mutant=release.replace(
            "        needs.config.outputs.release_channel == 'latest' &&\n"
            "        needs.finalize_release.outputs.promoted == 'true' &&\n"
            "        startsWith(github.ref, 'refs/tags/v') &&\n"
            "        !contains(github.ref_name, '-')\n"
            "      }}\n"
            "    runs-on: ubuntu-latest\n"
            "    environment: release\n"
            "    # Least privilege for one registry-side tag move",
            "        needs.config.outputs.release_channel == 'latest' &&\n"
            "        startsWith(github.ref, 'refs/tags/v') &&\n"
            "        !contains(github.ref_name, '-')\n"
            "      }}\n"
            "    runs-on: ubuntu-latest\n"
            "    environment: release\n"
            "    # Least privilege for one registry-side tag move",
            1,
        ): assert_latest_claims_gate_on_promotion(mutant),
    )
    expect_assertion(
        "a workflow asks for a mode the gate module does not know",
        "that it does not declare",
        lambda mutant=release_tag.replace(
            "KIN_RELEASE_REQUIRE=preflight", "KIN_RELEASE_REQUIRE=machine", 1
        ): assert_require_modes_match_the_gate(mutant, release, proof_gate),
    )
    # The coupling must also be able to notice that nothing asks at all, or it
    # would pass forever on a tree where the mode was deleted from both sides.
    expect_assertion(
        "no workflow asks the gate for a mode at all",
        "so this coupling checks nothing",
        lambda: assert_require_modes_match_the_gate(
            release_tag.replace("KIN_RELEASE_REQUIRE=preflight", "", 1),
            release.replace("KIN_RELEASE_REQUIRE=preflight", "", 1),
            proof_gate,
        ),
    )
