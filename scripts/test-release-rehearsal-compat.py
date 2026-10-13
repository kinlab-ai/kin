#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Negative controls for the fixed, released Windows compatibility baseline."""
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

SOURCE = Path(__file__).with_name("release-rehearsal-compat.py")
SPEC = importlib.util.spec_from_file_location("release_rehearsal_compat", SOURCE)
COMPAT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COMPAT)


def baseline_meta():
    return {
        "kin_version": "0.8.6",
        "kin_commit": "522ede0d0dfbdd33ff8ba3a5798bb3bed6902c08",
        "kin_dirty": False,
        "kin_source_known": True,
        "dependency_provenance": "9db830e29224527847b5a122e0883a2c9f968d806e34b4bf4d5052c4ceb60495",
    }


def baseline_health():
    return {
        "checks": [{"id": name, "status": "healthy"}
                   for name in COMPAT.REQUIRED_HEALTHY]
                  + [{"id": "repo_init", "status": "unsupported"},
                     {"id": "embedding_model", "status": "pending"},
                     {"id": "memory_floor", "status": "degraded"}],
        "verdict": "needs_attention", "healthy": False,
    }


class BaselineIdentity(unittest.TestCase):
    def test_both_versions_accept_released_metadata(self):
        for name in ("kin", "kin-daemon"):
            COMPAT.validate_version(f"{name} 0.8.6 ({COMPAT.BASELINE['commit']} clean)\r\n", name)
            COMPAT.validate_version(f"{name} 0.8.6", name)

    def test_versions_refuse_substitution_and_ambiguous_output(self):
        for value in ("kin 0.8.60", "kin 0.8.7", "kin-daemon 0.8.6", "kin 0.8.6\nkin 0.8.7",
                      "kin 0.8.6 unexpected", "0.8.6"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                COMPAT.validate_version(value, "kin")

    def test_exact_baseline_needs_no_future_feature_metadata(self):
        COMPAT.validate_meta(baseline_meta())

    def test_wrong_or_absent_provenance_is_rejected(self):
        wrong = {"kin_version": "0.8.7", "kin_commit": "0" * 40, "kin_dirty": True,
                 "kin_source_known": False, "dependency_provenance": "0" * 64}
        for key, value in wrong.items():
            for remove in (False, True):
                meta = baseline_meta()
                if remove:
                    del meta[key]
                else:
                    meta[key] = value
                with self.subTest(key=key, remove=remove), self.assertRaises(ValueError):
                    COMPAT.validate_meta(meta)

    def test_boolean_provenance_cannot_be_numeric(self):
        for key, value in (("kin_dirty", 0), ("kin_source_known", 1)):
            meta = baseline_meta()
            meta[key] = value
            with self.subTest(key=key), self.assertRaises(ValueError):
                COMPAT.validate_meta(meta)


class BaselineHealth(unittest.TestCase):
    def test_known_first_run_attention_is_accepted(self):
        rows = COMPAT.validate_health(baseline_health(), "setup-status")
        self.assertEqual(rows["commit_author"], "healthy")

    def test_author_regression_is_required_in_each_report(self):
        for label in ("setup-status", "doctor"):
            for status in ("missing", "misconfigured", "unsupported", "pending", None):
                report = baseline_health()
                report["checks"] = [row for row in report["checks"] if row["id"] != "commit_author"]
                if status is not None:
                    report["checks"].append({"id": "commit_author", "status": status})
                with self.subTest(label=label, status=status), self.assertRaisesRegex(ValueError, "commit_author"):
                    COMPAT.validate_health(report, label)

    def test_setup_and_binary_rows_cannot_disappear(self):
        for name in ("kin_binary", "kin_daemon_binary", "shell_path", "setup_ledger"):
            report = baseline_health()
            report["checks"] = [row for row in report["checks"] if row["id"] != name]
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, name):
                COMPAT.validate_health(report, "doctor")

    def test_honest_ready_report_is_accepted(self):
        report = baseline_health()
        report["checks"] = [row for row in report["checks"] if row["status"] != "pending" and row["status"] != "degraded"]
        report.update(verdict="ready", healthy=True)
        COMPAT.validate_health(report, "doctor")

    def test_ready_claim_over_pending_rows_is_rejected(self):
        for verdict, healthy in (("ready", True), ("needs_attention", True), (None, False)):
            report = baseline_health()
            report.update(verdict=verdict, healthy=healthy)
            with self.subTest(verdict=verdict, healthy=healthy), self.assertRaisesRegex(ValueError, "aggregate"):
                COMPAT.validate_health(report, "doctor")

    def test_duplicate_rows_cannot_hide_missing_author(self):
        report = baseline_health()
        report["checks"].append({"id": "commit_author", "status": "missing"})
        with self.assertRaisesRegex(ValueError, "duplicate"):
            COMPAT.validate_health(report, "doctor")

    def test_malformed_rows_are_rejected(self):
        for row in ({"id": "extra", "status": "unknown"}, {"id": "extra", "status": []},
                    {"status": "healthy"}, None):
            report = baseline_health()
            report["checks"].append(row)
            with self.subTest(row=row), self.assertRaises(ValueError):
                COMPAT.validate_health(report, "doctor")

    def test_honest_failure_still_cannot_pass_compatibility(self):
        report = baseline_health()
        report["checks"].append({"id": "extra_install_component", "status": "missing"})
        report["verdict"] = "failing"
        with self.assertRaisesRegex(ValueError, "missing or misconfigured"):
            COMPAT.validate_health(report, "doctor")


class ExecutionEvidence(unittest.TestCase):
    def test_setup_ledger_requires_real_artifact_rows(self):
        self.assertEqual(COMPAT.validate_ledger({"entries": [{"path": "client-config.json"}]}), 1)
        for ledger in ({}, {"entries": []}, {"entries": "one"}, {"entries": [None]}, None):
            with self.subTest(ledger=ledger), self.assertRaises(ValueError):
                COMPAT.validate_ledger(ledger)

    def test_persistent_and_non_native_hosts_are_refused(self):
        env = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted"}
        COMPAT.validate_host("win32", "AMD64", env)
        for system, machine, modified in (("darwin", "arm64", env), ("win32", "ARM64", env),
                                          ("win32", "AMD64", {}),
                                          ("win32", "AMD64", dict(env, RUNNER_ENVIRONMENT="self-hosted"))):
            with self.subTest(system=system, machine=machine, env=modified), self.assertRaises(ValueError):
                COMPAT.validate_host(system, machine, modified)

    def test_binary_evidence_requires_real_nonempty_files(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "kin.exe"
            with self.assertRaises(ValueError):
                COMPAT.binary_info(path)
            path.touch()
            with self.assertRaises(ValueError):
                COMPAT.binary_info(path)
            path.write_bytes(b"installed executable bytes")
            info = COMPAT.binary_info(path)
            self.assertEqual(info["size_bytes"], path.stat().st_size)
            self.assertEqual(len(info["sha256"]), 64)
            link = path.with_name("other.exe")
            try:
                link.symlink_to(path)
            except OSError:
                return
            with self.assertRaises(ValueError):
                COMPAT.binary_info(link)

    def test_failure_retains_stdout_stderr_and_exit_code(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            command = [sys.executable, "-c", "import sys; print('output'); print('reason', file=sys.stderr); sys.exit(3)"]
            with self.assertRaises(ValueError):
                COMPAT.run_command(command, "negative", output, output, dict(os.environ))
            self.assertEqual((output / "negative.stdout.txt").read_text().strip(), "output")
            self.assertEqual((output / "negative.stderr.txt").read_text().strip(), "reason")
            self.assertEqual(json.loads((output / "negative.exit.json").read_text())["exit_code"], 3)


if __name__ == "__main__":
    unittest.main()
