#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Failed-run evidence survives the CI upload boundary without changing a verdict."""
import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import types
import unittest
from unittest import mock

import brownfield_repro as brownfield


class FailureDiagnosticsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.work = self.root / "work"
        self.work.mkdir()
        self.report = self.root / "acceptance" / "brownfield.json"
        self.report.parent.mkdir()
        self.fixture = self.work / "requests"
        (self.fixture / ".kin").mkdir(parents=True)
        self.log = self.fixture / ".kin" / "daemon.log"
        self.log.write_text("LSP query failed\nLSP cold sweep ended with incomplete enrichment\n")
        (self.work / "mcp-payloads.json").write_text('[{"answer": "partial"}]\n')
        self.suite = types.SimpleNamespace(workdir=str(self.work),
                                          fixtures={"requests": str(self.fixture)},
                                          store_path=lambda repo: str(Path(repo) / ".kin"))

    def capture(self):
        result = brownfield.retain_failure_diagnostics(self.suite, str(self.report), 2)
        directory = self.report.parent / result["directory"]
        manifest = (directory / "manifest.json").read_bytes()
        self.assertEqual(hashlib.sha256(manifest).hexdigest(), result["manifest_sha256"])
        return directory, json.loads(manifest)

    def test_reports_keep_exact_normal_logs_after_workdir_is_gone(self):
        directory, manifest = self.capture()
        original = self.log.read_bytes()
        self.log.unlink()
        self.assertEqual((directory / "requests-daemon.txt").read_bytes(), original)
        row = manifest["files"][1]
        self.assertEqual(row["source_sha256"], hashlib.sha256(original).hexdigest())
        self.assertEqual(manifest["expected_suite_exit"], 2)
        self.assertNotIn(str(self.root), json.dumps(manifest))

    def test_credentials_and_environment_lines_are_not_uploaded(self):
        self.log.write_text("LSP query timed out\nAuthorization: Bearer private-value\n"
                            "OPENAI_API_KEY=private-value\nkin_core::env_registry KIN_X=private-value\n"
                            "https://example.invalid/?token=private-value\nLSP sweep incomplete\n")
        (self.fixture / ".kin" / "daemon.token").write_text("private-token-file")
        directory, manifest = self.capture()
        data = (directory / "requests-daemon.txt").read_text()
        self.assertNotIn("private-value", data)
        self.assertIn("LSP query timed out", data)
        self.assertIn("LSP sweep incomplete", data)
        self.assertEqual(manifest["files"][1]["redacted_lines"], 4)
        self.assertEqual(sorted(p.name for p in directory.iterdir()),
                         ["manifest.json", "mcp-payloads.txt", "requests-daemon.txt"])

    def test_bounded_tail_discards_partial_line_and_marks_hash_scope(self):
        self.log.write_text("Authorization: " + "x" * 100 + "\nlast complete line\n")
        with mock.patch.object(brownfield, "DIAGNOSTIC_BYTES", 50):
            directory, manifest = self.capture()
        self.assertEqual((directory / "requests-daemon.txt").read_text(), "last complete line\n")
        row = manifest["files"][1]
        self.assertTrue(row["truncated"])
        self.assertIsNone(row["source_sha256"])
        self.assertEqual(row["captured_bytes"], 50)

    def test_multiline_private_key_is_not_retained(self):
        self.log.write_text("LSP query failed\n-----BEGIN TEST DIAGNOSTIC-----\n"
                            "private-base64-material\n-----END TEST DIAGNOSTIC-----\n"
                            "LSP sweep incomplete\n")
        directory, manifest = self.capture()
        data = (directory / "requests-daemon.txt").read_text()
        self.assertNotIn("private-base64-material", data)
        self.assertEqual(data, "LSP query failed\nLSP sweep incomplete\n")
        self.assertEqual(manifest["files"][1]["redacted_lines"], 3)

    def test_clipped_private_key_body_is_omitted_until_its_end(self):
        self.log.write_text("-----BEGIN TEST DIAGNOSTIC-----\n" + "x" * 200 + "\n"
                            "private-base64-material\n-----END TEST DIAGNOSTIC-----\nLSP sweep incomplete\n")
        with mock.patch.object(brownfield, "DIAGNOSTIC_BYTES", 100):
            directory, manifest = self.capture()
        self.assertEqual((directory / "requests-daemon.txt").read_text(), "LSP sweep incomplete\n")
        self.assertTrue(manifest["files"][1]["truncated"])

    def test_missing_and_foreign_files_are_disclosed_without_following_them(self):
        self.log.unlink()
        foreign = self.root / "foreign"
        foreign.write_text("foreign secret")
        self.log.symlink_to(foreign)
        (self.work / "mcp-payloads.json").unlink()
        directory, manifest = self.capture()
        self.assertEqual(manifest["files"][0]["capture_error"], "FileNotFoundError")
        self.assertEqual(manifest["files"][1]["capture_error"], "ValueError")
        self.assertEqual([p.name for p in directory.iterdir()], ["manifest.json"])

    def run_main(self, status, capture_failure=False):
        kin = self.root / "kin"
        kin.touch()
        result = brownfield.Result("2", "test", "synthetic check")
        getattr(result, {brownfield.PASS: "ok", brownfield.FAIL: "bad",
                         brownfield.UNREADABLE: "unknown"}[status])("unchanged gold")
        cleanup = brownfield.Result("cleanup", "test", "cleanup")
        cleanup.ok("no worker")
        self.suite.payloads = {}
        self.suite.shutdown = mock.Mock(return_value=cleanup)
        capture = mock.patch.object(brownfield, "retain_failure_diagnostics",
                                    side_effect=OSError("private exception path")) if capture_failure else contextlib.nullcontext()
        with mock.patch.object(brownfield, "Suite", return_value=self.suite), \
             mock.patch.object(brownfield, "run", return_value=(0, "kin test\n", "")), \
             mock.patch.object(brownfield, "CHECKS", [("2", lambda _: result)]), \
             contextlib.redirect_stdout(io.StringIO()), capture:
            code = brownfield.main(["--kin", str(kin), "--workdir", str(self.work),
                                    "--corpus-cache", str(self.work), "--only", "2",
                                    "--json", str(self.report)])
        report = json.loads(self.report.read_text())
        self.assertEqual(report["results"][0]["status"], status)
        self.assertEqual(report["results"][0]["detail"], "unchanged gold")
        return code, report

    def test_main_preserves_failed_and_unreadable_exit_codes(self):
        for status, code in [(brownfield.FAIL, 1), (brownfield.UNREADABLE, 2)]:
            with self.subTest(status=status):
                actual, report = self.run_main(status)
                self.assertEqual(actual, code)
                self.assertTrue((self.report.parent / report["failure_diagnostics"]["directory"]).is_dir())

    def test_passing_run_does_not_retain_diagnostic_files(self):
        code, report = self.run_main(brownfield.PASS)
        self.assertEqual(code, 0)
        self.assertIsNone(report["failure_diagnostics"])
        self.assertFalse(list(self.report.parent.glob("*.diagnostics-*")))

    def test_capture_failure_does_not_hide_the_real_verdict(self):
        code, report = self.run_main(brownfield.FAIL, capture_failure=True)
        self.assertEqual(code, 1)
        self.assertEqual(report["failure_diagnostics"], {"capture_error": "OSError"})
        self.assertNotIn("private exception path", self.report.read_text())


if __name__ == "__main__":
    unittest.main()
