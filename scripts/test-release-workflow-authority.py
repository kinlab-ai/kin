#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Fail closed when Kin release authority drifts back onto main pushes.

The checks live in the release_authority package beside this script: one
module per workflow they pin, with shared paths and helpers in
test_helpers.py. This runner reads the shared sources and runs every check in
a fixed order, so the command line, the exit status and the final line stay
the same as when the suite was one file.
"""

from __future__ import annotations

import sys
from pathlib import Path

# Python puts this script's directory on sys.path unless safe-path mode is on
# (-P, -I or PYTHONSAFEPATH). Add it either way so the package always imports.
sys.path.insert(0, str(Path(__file__).resolve().parent))

from release_authority import (
    test_ci,
    test_docker,
    test_docs,
    test_install_proof,
    test_installers,
    test_proof_tiers,
    test_rc_build,
    test_release,
    test_release_cut,
    test_release_recovery,
    test_release_tag,
    test_release_train,
    test_sast,
    test_windows_support,
    test_workflows,
)
from release_authority.test_helpers import (
    ADVISORY_SWEEP,
    CAPABILITY_CONTRACT,
    DEV_IMAGE_PUBLISHER,
    DEV_IMAGE_TOOL,
    DOCKERFILE,
    HEALTH,
    HOLD_ALARM,
    INSTALLER_CALLBACK,
    INSTALL_PROOF,
    INSTALL_PROOF_CANARY,
    INSTALL_PS1,
    INSTALL_SH,
    MCP_TOOLS_DOC,
    NPM_CANONICAL_README,
    PORTED_VALIDATOR,
    PREFLIGHT_DRIVER,
    PROOF_GATE,
    QUICKSTART_DOC,
    README,
    README_REFERENCE_DOC,
    RELEASE,
    RELEASE_BOT_DOC,
    RELEASE_CUT,
    RELEASE_RECOVERY,
    RELEASE_TAG,
    RELEASE_TRAIN,
    SAST,
    SETUP,
    UPDATE_TRUST,
    WORKFLOWS,
    workflow_paths,
)


def main() -> None:
    test_workflows.check_release_probes_and_retired_workflows()

    release = RELEASE.read_text(encoding="utf-8")
    # The release archives are compiled by rc-build.yml alone; release.yml
    # ships those bytes and builds nothing, so the checks about how an archive
    # is built read rc-build.yml.
    rc_build = (WORKFLOWS / "rc-build.yml").read_text(encoding="utf-8")

    test_rc_build.check_core_archive_build_profile(release, rc_build)

    release_cut = RELEASE_CUT.read_text(encoding="utf-8")

    test_release_cut.check_native_startup_and_dispatch_identity(release_cut)

    release_recovery = RELEASE_RECOVERY.read_text(encoding="utf-8")
    release_tag = RELEASE_TAG.read_text(encoding="utf-8")
    release_train = RELEASE_TRAIN.read_text(encoding="utf-8")
    sast = SAST.read_text(encoding="utf-8")
    advisory_sweep = ADVISORY_SWEEP.read_text(encoding="utf-8")
    hold_alarm = HOLD_ALARM.read_text(encoding="utf-8")
    proof_gate = PROOF_GATE.read_text(encoding="utf-8")
    release_bot_doc = RELEASE_BOT_DOC.read_text(encoding="utf-8")
    install_proof = INSTALL_PROOF.read_text(encoding="utf-8")
    install_proof_bytes = INSTALL_PROOF.read_bytes()
    ported_validator = PORTED_VALIDATOR.read_text(encoding="utf-8")
    preflight_driver = PREFLIGHT_DRIVER.read_text(encoding="utf-8")
    install_proof_canary = INSTALL_PROOF_CANARY.read_text(encoding="utf-8")
    capability_contract = CAPABILITY_CONTRACT.read_text(encoding="utf-8")
    readme = README.read_text(encoding="utf-8")
    ci_workflow = (WORKFLOWS / "ci.yml").read_text(encoding="utf-8")
    installer_callback = INSTALLER_CALLBACK.read_text(encoding="utf-8")
    update_trust = UPDATE_TRUST.read_text(encoding="utf-8")
    install_sh = INSTALL_SH.read_text(encoding="utf-8")
    install_ps1 = INSTALL_PS1.read_text(encoding="utf-8")
    health = HEALTH.read_text(encoding="utf-8")
    setup = SETUP.read_text(encoding="utf-8")
    quickstart = QUICKSTART_DOC.read_text(encoding="utf-8")
    readme_reference = README_REFERENCE_DOC.read_text(encoding="utf-8")
    mcp_tools = MCP_TOOLS_DOC.read_text(encoding="utf-8")
    npm_canonical_readme = NPM_CANONICAL_README.read_text(encoding="utf-8")
    docker_workflow = (WORKFLOWS / "docker.yml").read_text(encoding="utf-8")
    dev_image_publisher = DEV_IMAGE_PUBLISHER.read_text(encoding="utf-8")
    dev_image_tool = DEV_IMAGE_TOOL.read_text(encoding="utf-8")
    workflow_sources = {
        workflow: workflow.read_text(encoding="utf-8") for workflow in workflow_paths()
    }

    test_rc_build.check_release_critical_runner_pools(workflow_sources)
    test_workflows.check_workflow_job_census(workflow_sources)
    test_docker.check_dev_image_publish_policy(
        ci_workflow, dev_image_publisher, dev_image_tool, docker_workflow
    )
    test_workflows.check_job_census_falsifiers(workflow_sources)
    test_installers.check_installer_registry_authority(install_ps1, install_sh)
    test_release.check_release_trigger_and_completion_seal(release)
    test_release.check_release_consumes_rc_build(release)
    test_release_tag.check_release_tag_authority(release_tag)
    test_release_train.check_release_train_authority(release_train)
    test_release_tag.check_abandoned_tag_admission(
        release_recovery, release_tag, release_train
    )
    test_release_train.check_release_train_forbidden_authority(release_train)
    test_docs.check_release_bot_and_update_trust_docs(release_bot_doc, update_trust)
    test_release_recovery.check_release_recovery_policy(release_recovery)
    test_install_proof.check_capability_canary_contract(
        capability_contract, install_proof, install_proof_canary
    )
    test_release_recovery.check_recovery_alarms_and_abandonment(release_recovery)
    test_release_tag.check_release_proof_key_authority(
        proof_gate, release, release_tag, release_train
    )
    test_release_train.check_release_hold_marker_contract(hold_alarm, release_train)
    test_release_recovery.check_recovery_forbidden_authority(release_recovery)
    test_docs.check_readme_latest_release_policy(readme, readme_reference)
    embedding, first_run, graph_query, validation = (
        test_install_proof.check_first_run_captures_and_embedding(install_proof)
    )
    test_windows_support.check_windows_public_support_and_admission(
        install_proof,
        install_ps1,
        npm_canonical_readme,
        quickstart,
        readme,
        readme_reference,
        update_trust,
    )
    test_install_proof.check_install_proof_legs_and_pull_request_gate(
        ci_workflow,
        install_proof,
        install_proof_bytes,
        ported_validator,
        preflight_driver,
    )
    test_install_proof.check_preflight_validators_and_status_contract(
        embedding, first_run, graph_query, install_proof, validation
    )
    test_install_proof.check_installed_mcp_launchers_and_proof_reports(
        health, install_proof, mcp_tools, npm_canonical_readme, quickstart, setup
    )
    test_windows_support.check_windows_admission_script()

    dockerfile = DOCKERFILE.read_text(encoding="utf-8")

    # The pull-request musl compile guard follows the matrix that compiles the
    # Linux release archives, which is rc-build.yml's build job.
    test_ci.check_windows_legs_and_release_target_compile_guard(ci_workflow, rc_build)
    test_docker.check_base_image_pins_and_container_bases(
        docker_workflow, dockerfile, rc_build
    )
    test_installers.check_installer_callback(installer_callback, update_trust)
    test_docker.check_docker_workflow_pins(docker_workflow)
    attestation_job, image_job = test_release.check_release_image_and_publish_jobs(
        ci_workflow, release, update_trust
    )
    test_ci.check_windows_installer_admission(ci_workflow)
    classifier = test_ci.check_docs_only_classifier_wiring(ci_workflow)
    test_ci.check_rust_toolchain_prune()
    test_sast.check_cargo_deny_and_advisory_sweep(advisory_sweep, release_train, sast)
    test_rc_build.check_retired_vfs_runtime_absent_from_archives(
        ci_workflow, install_proof, release, workflow_sources
    )
    test_installers.check_installer_guards_wired(ci_workflow, release)
    test_workflows.check_active_lines_stay_within_their_shell(workflow_sources)
    test_ci.check_release_version_and_reachability_gates(ci_workflow)
    test_ci.check_ci_job_authority(ci_workflow)
    test_ci.check_docs_only_classifier_authority(
        ci_workflow, classifier, workflow_sources
    )
    test_release_tag.check_release_tag_required_check_gate()
    test_workflows.check_rust_cache_and_action_pins(workflow_sources)
    test_release_tag.check_tag_readback_and_ruleset_mirror(release_tag)
    test_proof_tiers.check_two_tier_proof_contract(
        proof_gate, release, release_cut, release_tag, workflow_sources
    )
    test_release_tag.check_soft_decline_is_legible(release_tag)
    test_release_recovery.check_recovery_escalation()
    test_release_tag.check_mint_trigger_survives_advisory_flakes(release_tag)
    test_ci.check_release_pr_ci_scope(ci_workflow)
    test_workflows.check_workflow_enumeration_and_cache_falsifiers(
        ci_workflow, workflow_sources
    )
    test_release.check_npm_release_path_and_action_pins(
        attestation_job, ci_workflow, image_job, release
    )
    test_workflows.check_workflow_token_permissions()

    print(
        "release workflow authority is reviewed-PR to App-tag automatic, "
        "protected, pinned, GCP-free on release writes, cross-platform "
        "byte-canonical, with dev-only Artifact Registry OIDC and npm automatic "
        "through short-lived OIDC with post-public proof"
    )


if __name__ == "__main__":
    main()
