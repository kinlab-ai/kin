# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Checks that span every workflow.

Release-critical runner pools, the job census, Rust cache and action pins,
token permissions and the workflows that must stay retired.
"""

from __future__ import annotations

import importlib.util
import re
import tempfile
import textwrap
from pathlib import Path

from .test_helpers import (
    RELEASE_VERSION_FALSIFIER,
    ROOT,
    RUST_CACHE_ACTION,
    WORKFLOWS,
    active_lines,
    assert_rust_cache_steps,
    assert_workflow_job_census,
    expect_assertion,
    strip_block_comments,
    workflow_paths,
)


# Which tag each workflow declares it is about to create. Only the mint creates
# one, so only the mint names it. The train resolves drift from a base tag it
# never mints, and handing that base over as mint intent refuses exactly when a
# record covers it, which is always the moment a record is written: the mint
# only ever creates `v$(workspace version)`, so main's version equals the stuck
# tag until the train opens the bump the record exists to unblock. The empty
# argument is spelled as a literal rather than an expanded variable so that
# refilling it is a visible diff here and not a silent assignment upstream.
# Third-party actions inside the workflows that produce presence-required release
# contexts. A required context is release evidence, so whatever can write into it
# is release supply chain and has to be pinned to an immutable object rather than
# a tag anyone upstream can move. `actions/*` are first-party and governed
# separately, and `./`-prefixed actions are this repository's own tree, which
# moves only through a reviewed pull request here; these are the ones outside
# that trust boundary.
EXPECTED_REQUIRED_CONTEXT_ACTION_PINS = {
    ".github/workflows/sast.yml": {
        "taiki-e/install-action": "6a1bd70eaac3c8bdf093356838d7ee09fda951cf",
    },
}

MAIN_ONLY_CACHE_SAVE = "save-if: ${{ github.ref == 'refs/heads/main' }}"

CACHE_AUTHORITY_ADVERSARIAL_WORKFLOWS = (
    (
        "escaped action key and value",
        "canonical unquoted `key:` syntax",
        r"""
name: Escaped action
on: push
permissions:
  contents: read
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden moving cache action
        "u\u0073es": "Swatinem/rust-ca\u0063he@v2"
        with:
          save-if: ${{ true }}
""",
    ),
    (
        "aliased action value",
        "direct action scalar",
        r"""
name: Aliased action
on: push
permissions:
  contents: read
env:
  HIDDEN_CACHE: &hidden_cache "Swatinem/rust-ca\u0063he@v2"
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden alias cache action
        uses: *hidden_cache
        with:
          save-if: ${{ true }}
""",
    ),
    (
        "multiline escaped action value",
        "direct action scalar",
        r"""
name: Multiline escaped action
on: push
permissions:
  contents: read
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden multiline cache action
        uses: "Swatinem/rust-ca\
          che@v2"
        with:
          save-if: ${{ true }}
""",
    ),
    (
        "flow-mapping action step",
        "canonical block mapping",
        r"""
name: Flow action
on: push
permissions:
  contents: read
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - {name: Hidden flow cache action, "u\u0073es": "Swatinem/rust-ca\u0063he@v2", with: {save-if: "${{ true }}"}}
""",
    ),
    (
        "escaped save-if key",
        "canonical unquoted `key:` syntax",
        r"""
name: Escaped cache policy
on: push
permissions:
  contents: read
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden cache save policy
        uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4
        with:
          "save\u002dif": ${{ true }}
""",
    ),
    (
        "aliased save-if value",
        "must be the exact main-only scalar",
        r"""
name: Aliased cache policy
on: push
permissions:
  contents: read
env:
  HIDDEN_SAVE: &hidden_save "${{ true }}"
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden aliased save policy
        uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4
        with:
          save-if: *hidden_save
""",
    ),
    (
        "flow-mapping with policy",
        "with field must be a canonical block mapping",
        r"""
name: Flow cache policy
on: push
permissions:
  contents: read
jobs:
  adversarial:
    runs-on: ubuntu-latest
    steps:
      - name: Hidden flow save policy
        uses: Swatinem/rust-cache@c19371144df3bb44fab255c43d04cbc2ab54d1c4
        with: {save-if: "${{ true }}"}
""",
    ),
)

EXTERNAL_REQUIRED_CONTEXT_SPOOF = textwrap.dedent(
    """\
    name: Required Context Spoof

    on:
      push:
        branches: [main]

    permissions:
      contents: read

    jobs:
      delay:
        name: Delay context creation
        runs-on: ubuntu-latest
        steps:
          - run: sleep 1
      spoof:
        name: Check & Test
        needs: delay
        runs-on: ubuntu-latest
        strategy:
          matrix:
            os: [ubuntu-latest, macos-latest]
        steps:
          - run: echo success
    """
)

# A fixture in the exact shape ci.yml carries: a `case` arm whose glob ends in
# `/*`, a cache key holding `**/`, and a line between them that every assertion
# reading this file depends on being able to see.
ACTIVE_LINES_FIXTURE = """\
case "$path" in
  *.md | docs/*) ;;
esac
run: python3 ./scripts/the-line-between.py
key: ${{ hashFiles('**/Cargo.lock') }}
/* a real block comment
   spanning two lines */
const kept = 1;
/* a one-line block comment */
const alsoKept = 2;
"""
ACTIVE_LINES_SURVIVOR = "run: python3 ./scripts/the-line-between.py"


def assert_active_lines_cannot_span_unrelated_shell(workflows: dict[Path, str]) -> None:
    """Keep the comment stripper from eating the lines every assertion reads.

    `active_lines` used to strip `<# #>`, `/* */` and `<!-- -->` with three
    DOTALL regexes over the whole text, which cannot tell a comment from a pair
    of shell globs. In ci.yml the `docs/*)` arm of a `case` opened one and
    `hashFiles('**/Cargo.lock')` closed it: 46,138 characters, 48% of the file,
    were removed before any assertion saw them, and release-train.yml lost 24%
    the same way. An assertion that cannot see the line it names passes exactly
    like one that checked it, which is the shape this whole suite exists to
    prevent.

    No workflow contains a real block comment, so that stripping never removed
    one. The last check below is what keeps that true: if a workflow ever does
    open a block comment, this fails and asks for the decision to be reviewed
    here rather than discovered as a silent hole later.
    """

    kept = active_lines(ACTIVE_LINES_FIXTURE)
    if ACTIVE_LINES_SURVIVOR not in kept:
        raise AssertionError(
            "active_lines dropped a line between two unrelated shell globs; a "
            "glob is not a block comment and an assertion reading past one "
            "would pass without seeing what it names"
        )
    for survivor in ("const kept = 1;", "const alsoKept = 2;"):
        if survivor not in kept:
            raise AssertionError(f"active_lines dropped live source: {survivor}")
    for comment in ("a real block comment", "spanning two lines */", "a one-line block comment"):
        if any(comment in line for line in kept):
            raise AssertionError(
                f"active_lines kept a real block comment: {comment}; a commented-out "
                "validator would then satisfy a guard"
            )

    # The fixture has to be able to tell the two implementations apart. Without
    # this, a fixture that both forms handle identically would report a fix
    # that is not there.
    superseded = ACTIVE_LINES_FIXTURE
    for pattern in (r"<#.*?#>", r"/\*.*?\*/", r"<!--.*?-->"):
        superseded = re.sub(pattern, "", superseded, flags=re.DOTALL)
    if ACTIVE_LINES_SURVIVOR in {line.strip() for line in superseded.splitlines()}:
        raise AssertionError(
            "the active_lines fixture cannot distinguish the DOTALL form from "
            "the line-oriented one, so it proves nothing"
        )

    for workflow, content in sorted(workflows.items()):
        lines = content.splitlines()
        dropped = len(lines) - len(strip_block_comments(lines))
        if dropped:
            raise AssertionError(
                f"{workflow.relative_to(ROOT).as_posix()} opens a block comment "
                f"and loses {dropped} line(s) before any assertion reads it. If "
                "that comment is real, review this census; if it is a glob, the "
                "stripper is wrong again"
            )


def assert_required_context_action_pins(workflows: dict[Path, str]) -> None:
    """Pin the supply chain of anything that can write a required release context.

    A required context is the evidence a release is minted from, so an action
    running inside its producer can decide what that evidence says. A floating
    tag leaves that decision with whoever can move the tag upstream.
    """

    for path, expected in EXPECTED_REQUIRED_CONTEXT_ACTION_PINS.items():
        content = workflows.get(ROOT / path)
        if content is None:
            raise AssertionError(f"required-context producer is missing: {path}")
        # Every reference is collected, not the last one seen per action. Each
        # `uses:` executes, so a single unpinned reference is an unpinned
        # execution however many pinned ones sit beside it, and whichever order
        # they appear in. Keying on one reference per action hid exactly that:
        # a floating duplicate placed above the pinned line was overwritten by
        # it and the guard stayed green while the floating ref still ran.
        observed: dict[str, set[str]] = {}
        for reference in re.findall(r"uses:\s*(\S+)", content):
            if reference.startswith("actions/"):
                continue
            # A `./` reference resolves inside the checkout this workflow already
            # made, so it carries no upstream that could move under the pin and
            # has no immutable object to name. It changes only by a reviewed pull
            # request to this repository, which is the same control the pin buys.
            if reference.startswith("./"):
                continue
            action, _, version = reference.partition("@")
            observed.setdefault(action, set()).add(version)
        if set(observed) != set(expected):
            raise AssertionError(
                f"{path} produces a presence-required release context, so its "
                "third-party action set must stay exactly as reviewed: "
                f"expected={sorted(expected)} actual={sorted(observed)}"
            )
        for action, references in sorted(observed.items()):
            pin = expected[action]
            if not re.fullmatch(r"[0-9a-f]{40}", pin):
                raise AssertionError(
                    f"{path} pins {action} to '{pin}', which is a movable ref "
                    "rather than an immutable commit"
                )
            drifted = sorted(reference for reference in references if reference != pin)
            if drifted:
                raise AssertionError(
                    f"{path} runs {action} at {drifted} alongside its reviewed "
                    f"pin {pin}; every reference to it must be that pin, "
                    "because each one executes"
                )


def assert_release_probe_fixtures() -> None:
    spec = importlib.util.spec_from_file_location("release_probes", RELEASE_VERSION_FALSIFIER)
    assert spec is not None and spec.loader is not None
    probes = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probes)
    with tempfile.TemporaryDirectory() as temporary:
        tree = probes.probe_tree(Path(temporary))
        result = probes.run_suite(tree, probes.INTENT_SUITE)
        assert result.returncode == 0, (
            "release intent probe baseline failed\n" + result.stdout + result.stderr
        )
        for name in (
            "resolve-release-intent.mjs",
            "resolve-release-intent.test.mjs",
            "release-intent-attestations.json",
        ):
            fixture = tree / "scripts" / name
            original = fixture.read_bytes()
            fixture.unlink()
            try:
                broken = probes.run_suite(tree, probes.INTENT_SUITE)
                assert broken.returncode != 0, f"missing {name} did not fail the probe suite"
            finally:
                fixture.write_bytes(original)



def check_release_probes_and_retired_workflows() -> None:
    assert_release_probe_fixtures()
    retired = (
        "auto-tag-release.yml",
        "daemon-image.yml",
        "kin-dependency-wave.yml",
        "publish-install-scripts.yml",
    )
    for name in retired:
        if (WORKFLOWS / name).exists():
            raise AssertionError(
                f"{name} must stay retired; release.yml owns protected public writes"
            )



def check_workflow_job_census(workflow_sources: dict[Path, str]) -> None:
    assert_workflow_job_census(workflow_sources)



def check_job_census_falsifiers(workflow_sources: dict[Path, str]) -> None:
    external_context_spoof = dict(workflow_sources)
    external_context_spoof[WORKFLOWS / "required-context-spoof.yaml"] = (
        EXTERNAL_REQUIRED_CONTEXT_SPOOF
    )
    expect_assertion(
        "external workflow emits the release-required Check & Test matrix contexts",
        "workflow-wide required-check producer census",
        lambda: assert_workflow_job_census(external_context_spoof),
    )

    quoted_job_spoof = dict(workflow_sources)
    quoted_job_spoof[WORKFLOWS / "ci.yml"] = quoted_job_spoof[
        WORKFLOWS / "ci.yml"
    ].replace(
        "jobs:\n",
        'jobs:\n  "context-spoof":\n'
        "    name: Check & Test\n"
        "    runs-on: ubuntu-latest\n"
        "    steps:\n"
        "      - run: echo success\n",
        1,
    )
    expect_assertion(
        "quoted job id hides a required-context producer before the first job",
        "canonical unquoted scalar",
        lambda: assert_workflow_job_census(quoted_job_spoof),
    )

    unnamed_workflow = WORKFLOWS / "registry-index-migrate.yml"
    if unnamed_workflow not in workflow_sources:
        raise AssertionError(
            "workflow census falsification could not identify an unnamed job"
        )
    unnamed_source = workflow_sources[unnamed_workflow]
    unnamed_lines = unnamed_source.splitlines()
    try:
        unnamed_job_start = unnamed_lines.index("  migrate:")
    except ValueError as error:
        raise AssertionError(
            "workflow census falsification could not identify the unnamed job block"
        ) from error
    for label, child_indent in (
        ("one-space job child mapping", 1),
        ("three-space job child mapping", 3),
        ("wider four-space job child mapping", 4),
    ):
        delta = child_indent - 2
        reindented_children = []
        for line in unnamed_lines[unnamed_job_start + 1 :]:
            if not line.strip():
                reindented_children.append(line)
            elif delta > 0:
                reindented_children.append(" " * delta + line)
            else:
                remove = -delta
                if len(line) - len(line.lstrip()) < remove:
                    raise AssertionError(
                        f"workflow census falsification could not reindent {label}"
                    )
                reindented_children.append(line[remove:])
        indentation_spoof = dict(workflow_sources)
        indentation_spoof[unnamed_workflow] = (
            "\n".join(
                unnamed_lines[: unnamed_job_start + 1]
                + [" " * (2 + child_indent) + "name: cargo-deny"]
                + reindented_children
            )
            + "\n"
        )
        expect_assertion(
            f"{label} hides a required context on an expected unnamed job",
            "canonical two-space child indentation",
            lambda indentation_spoof=indentation_spoof: assert_workflow_job_census(
                indentation_spoof
            ),
        )

    for label, alternate_name_key in (
        ("double-quoted job name key", '"name": cargo-deny'),
        ("single-quoted job name key", "'name': cargo-deny"),
        ("job name key with whitespace before the colon", "name : cargo-deny"),
        ("tagged job name key", "!!str name: cargo-deny"),
    ):
        alternate_name_spoof = dict(workflow_sources)
        alternate_name_spoof[unnamed_workflow] = textwrap.dedent(
            f"""\
            name: Registry Index Migrate
            on:
              push:
                branches: [main]
            permissions:
              contents: read
            jobs:
              migrate:
                {alternate_name_key}
                runs-on: ubuntu-latest
                steps:
                  - run: echo success
            """
        )
        expect_assertion(
            f"{label} emits a required context from an expected unnamed job",
            "canonical unquoted `key:` syntax",
            lambda alternate_name_spoof=alternate_name_spoof: (
                assert_workflow_job_census(alternate_name_spoof)
            ),
        )

    auxiliary_dco = WORKFLOWS / "dco.yml"
    promoted_dco = dict(workflow_sources)
    if promoted_dco[auxiliary_dco].count("    name: DCO sign-off") != 1:
        raise AssertionError(
            "workflow census falsification could not identify auxiliary DCO name"
        )
    promoted_dco[auxiliary_dco] = promoted_dco[auxiliary_dco].replace(
        "    name: DCO sign-off",
        "    name: DCO Sign-off",
        1,
    )
    expect_assertion(
        "auxiliary DCO workflow claims the release-required DCO context",
        "workflow-wide required-check producer census",
        lambda: assert_workflow_job_census(promoted_dco),
    )

    install_proof_workflow = WORKFLOWS / "install-proof.yml"
    dynamic_context_spoof = dict(workflow_sources)
    matrix_row = '{"os":"ubuntu-latest","setup-shell":"bash"}'
    if dynamic_context_spoof[install_proof_workflow].count(matrix_row) != 2:
        raise AssertionError(
            "workflow census falsification could not identify install-proof matrix"
        )
    dynamic_context_spoof[install_proof_workflow] = dynamic_context_spoof[
        install_proof_workflow
    ].replace(
        matrix_row,
        '{"os":"Check & Test (ubuntu-latest)","setup-shell":"bash"}',
        1,
    )
    expect_assertion(
        "dynamic matrix-only job resolves to a release-required context",
        "exact reviewed matrix expansions",
        lambda: assert_workflow_job_census(dynamic_context_spoof),
    )



def check_active_lines_stay_within_their_shell(
    workflow_sources: dict[Path, str]
) -> None:
    assert_active_lines_cannot_span_unrelated_shell(workflow_sources)



def check_rust_cache_and_action_pins(workflow_sources: dict[Path, str]) -> None:
    # Cargo caches are restore-anywhere, save-from-main-only, so one reusable
    # warm entry per job stays alive under the repository cache budget instead
    # of being evicted by per-pull-request entries no other run can read.
    #
    # The justification is budget, NOT trust. A run on refs/heads/main can only
    # restore entries scoped to refs/heads/main, so GitHub already prevents a
    # fork or pull request from planting a cache a trusted run would restore;
    # save-if does not create that boundary. Do not reason about a related
    # change as though it did.
    #
    # Scope: this covers Swatinem/rust-cache uses only. One cargo actions/cache
    # use remains deliberately outside it: fuzz may still write a cache on a
    # qualifying pull request. This is not a repository-wide no-PR-writes
    # invariant.
    #
    # windows-installer used to be the third. It hand-rolled an actions/cache
    # whose key carried hashFiles('Cargo.lock') over a path that included
    # `target`, so every repin minted a fresh multi-GB Windows entry, and on
    # 59c2239c2 the post-step archive ran past the job's 60-minute timeout and
    # cancelled a run whose work had already finished. It now carries the same
    # rust-cache block as the three sibling Windows legs, which is why this
    # assertion covers it.
    assert_rust_cache_steps(workflow_sources)
    assert_required_context_action_pins(workflow_sources)



def check_workflow_enumeration_and_cache_falsifiers(
    ci_workflow: str, workflow_sources: dict[Path, str]
) -> None:
    with tempfile.TemporaryDirectory() as directory:
        fixture_directory = Path(directory)
        for name in ("recognized.yml", "recognized.yaml", "ignored.txt"):
            (fixture_directory / name).write_text("fixture\n", encoding="utf-8")
        enumerated = [path.name for path in workflow_paths(fixture_directory)]
        if enumerated != ["recognized.yaml", "recognized.yml"]:
            raise AssertionError(
                "workflow enumeration must include both .yml and .yaml files"
            )

    ci_path = WORKFLOWS / "ci.yml"
    check_start = ci_workflow.index("\n  check:")
    # The slice has to stop at the very next job. `check-macos` follows `check`
    # and caches too, and `falsify-guards` and the feature workers follow that, so
    # slicing to any of them would hand this falsification several jobs and count
    # save lines that are not the check job's.
    check_end = ci_workflow.index("\n  check-macos:", check_start)
    check_job = ci_workflow[check_start:check_end]
    save_line = f"          {MAIN_ONLY_CACHE_SAVE}\n"
    if check_job.count(save_line) != 1:
        raise AssertionError(
            "cache falsification could not identify the check job's main-only save"
        )
    check_without_save = check_job.replace(save_line, "", 1)
    unbound_ci = (
        ci_workflow[:check_start] + check_without_save + ci_workflow[check_end:]
    )

    comment_compensation = dict(workflow_sources)
    comment_compensation[ci_path] = unbound_ci
    fuzz_path = WORKFLOWS / "fuzz.yml"
    comment_compensation[fuzz_path] += f"\n# {MAIN_ONLY_CACHE_SAVE}\n"
    expect_assertion(
        "missing rust-cache save-if compensated by a comment",
        "with mapping and one main-only save-if",
        lambda: assert_rust_cache_steps(comment_compensation),
    )

    field_compensation = dict(workflow_sources)
    field_compensation[ci_path] = unbound_ci
    # Matched by prefix, not by the whole line. What this falsification needs is
    # an active field on an unrelated cache step; which inputs the fuzz key
    # happens to hash, and whether it carries a toolchain pin, is incidental to
    # that and changes whenever the fuzz job is retuned. Pinning the exact text
    # here would fail the release-authority suite for an unrelated edit. The
    # `key:` prefix still distinguishes it from the `restore-keys:` entries,
    # which are indented further and carry no field name.
    fuzz_key_prefix = "          key: ${{ runner.os }}-parser-fuzz-"
    fuzz_key_matches = [
        line
        for line in field_compensation[fuzz_path].splitlines(keepends=True)
        if line.startswith(fuzz_key_prefix)
    ]
    if len(fuzz_key_matches) != 1:
        raise AssertionError(
            "cache falsification could not identify the unrelated fuzz cache step"
        )
    fuzz_key = fuzz_key_matches[0]
    field_compensation[fuzz_path] = field_compensation[fuzz_path].replace(
        fuzz_key,
        fuzz_key + f"          {MAIN_ONLY_CACHE_SAVE}\n",
        1,
    )
    expect_assertion(
        "missing rust-cache save-if compensated by an unrelated active field",
        "with mapping and one main-only save-if",
        lambda: assert_rust_cache_steps(field_compensation),
    )

    same_step_field = dict(workflow_sources)
    same_step_field[ci_path] = (
        ci_workflow[:check_start]
        + check_job.replace(
            save_line,
            f"        env:\n          {MAIN_ONLY_CACHE_SAVE}\n",
            1,
        )
        + ci_workflow[check_end:]
    )
    expect_assertion(
        "rust-cache save-if moved from with to an env field in the same step",
        "one canonical with mapping and one main-only save-if",
        lambda: assert_rust_cache_steps(same_step_field),
    )

    pinned_use = f"uses: {RUST_CACHE_ACTION}"
    if workflow_sources[ci_path].count(pinned_use) < 1:
        raise AssertionError(
            "cache falsification could not identify a pinned rust-cache use"
        )
    quoted_pinned = dict(workflow_sources)
    quoted_pinned[ci_path] = quoted_pinned[ci_path].replace(
        pinned_use,
        f'uses: "{RUST_CACHE_ACTION}"',
        1,
    )
    assert_rust_cache_steps(quoted_pinned)

    single_quoted_pinned = dict(workflow_sources)
    single_quoted_pinned[ci_path] = single_quoted_pinned[ci_path].replace(
        pinned_use,
        f"uses: '{RUST_CACHE_ACTION}'",
        1,
    )
    assert_rust_cache_steps(single_quoted_pinned)

    quoted_unpinned = dict(workflow_sources)
    quoted_unpinned[ci_path] = quoted_unpinned[ci_path].replace(
        pinned_use,
        'uses: "Swatinem/rust-cache@v2"',
        1,
    )
    expect_assertion(
        "quoted rust-cache use at a moving ref",
        "uses rust-cache at an unpinned ref",
        lambda: assert_rust_cache_steps(quoted_unpinned),
    )

    yaml_unpinned = dict(workflow_sources)
    yaml_unpinned[WORKFLOWS / "adversarial-cache.yaml"] = f"""\
jobs:
  adversarial:
    steps:
      - uses: "Swatinem/rust-cache@v2"
        with:
          {MAIN_ONLY_CACHE_SAVE}
"""
    expect_assertion(
        ".yaml workflow containing a moving rust-cache ref",
        "uses rust-cache at an unpinned ref",
        lambda: assert_rust_cache_steps(yaml_unpinned),
    )

    for label, escaped_action in (
        ("hex-escaped rust-cache separator", r"Swatinem/rust-cache\x40v2"),
        ("unicode-escaped rust-cache separator", r"Swatinem/rust-cache\u0040v2"),
        ("unicode-escaped rust-cache repository", r"Swatinem/rust-ca\u0063he@v2"),
    ):
        escaped_use = dict(workflow_sources)
        escaped_use[ci_path] = escaped_use[ci_path].replace(
            pinned_use,
            f'uses: "{escaped_action}"',
            1,
        )
        expect_assertion(
            label,
            "must not contain YAML escape sequences",
            lambda escaped_use=escaped_use: assert_rust_cache_steps(escaped_use),
        )

    for index, (label, expected_error, fixture) in enumerate(
        CACHE_AUTHORITY_ADVERSARIAL_WORKFLOWS,
        start=1,
    ):
        adversarial = dict(workflow_sources)
        adversarial[WORKFLOWS / f"cache-authority-adversarial-{index}.yml"] = (
            textwrap.dedent(fixture).lstrip()
        )
        expect_assertion(
            label,
            expected_error,
            lambda adversarial=adversarial: assert_rust_cache_steps(adversarial),
        )

    non_action_text = dict(workflow_sources)
    non_action_text[ci_path] = non_action_text[ci_path].replace(
        "jobs:\n",
        f"env:\n  UNACCOUNTED_CACHE_ACTION: {RUST_CACHE_ACTION}\n\njobs:\n",
        1,
    )
    assert_rust_cache_steps(non_action_text)



def check_workflow_token_permissions() -> None:
    for workflow in workflow_paths():
        content = workflow.read_text(encoding="utf-8")
        header = content.split("\njobs:", maxsplit=1)[0]
        if re.search(r"(?m)^permissions:\s*$", header) is None:
            raise AssertionError(
                f"{workflow.name} must set explicit top-level token permissions"
            )
        if re.search(r"(?m)^  contents:\s*read\s*$", header) is None:
            raise AssertionError(
                f"{workflow.name} must default its workflow token to contents: read"
            )
        main_branch = re.search(
            r"(?m)^\s+branches:\s*\[\s*['\"]?main['\"]?\s*\]\s*$",
            content,
        ) or re.search(
            r"(?m)^\s+branches:\s*$\n(?:\s+-.*\n)*?\s+-\s*['\"]?main['\"]?\s*$",
            content,
        )
        if main_branch:
            writes = re.findall(
                r"(?m)^\s+(contents|packages|id-token):\s*write\s*$", content
            )
            dev_image_oidc_only = (
                workflow.name == "docker.yml" and writes == ["id-token"]
            )
            if writes and not dev_image_oidc_only:
                raise AssertionError(
                    f"{workflow.name} grants {sorted(set(writes))} on a main-push workflow"
                )

        for forbidden_secret in ("WIF_PROVIDER", "WIF_SERVICE_ACCOUNT"):
            if forbidden_secret in content:
                raise AssertionError(
                    f"{workflow.name} still consumes infra-only {forbidden_secret}"
                )

        if "workflow_dispatch:" in content and re.search(
            r"secrets\.(?:KIN_[A-Z0-9_]*(?:TOKEN|KEY|APP_ID)|NPM_TOKEN|WIF_PROVIDER|WIF_SERVICE_ACCOUNT)",
            content,
        ):
            raise AssertionError(
                f"{workflow.name} exposes branch-selectable dispatch with a release-capable secret"
            )
