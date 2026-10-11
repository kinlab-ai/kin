#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""A current attempt must finish before a recall precondition is graded."""
import contextlib
import importlib.util
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch


spec = importlib.util.spec_from_file_location(
    "brownfield", Path(__file__).with_name("brownfield_repro.py"))
brownfield = importlib.util.module_from_spec(spec)
spec.loader.exec_module(brownfield)


START = "LSP cold sweep started, enriching all entities\n"
CLEAN = ("LSP cold sweep complete files=1 total_files=1 relations=1 "
         "enriched=1 server_unavailable=0 source_unreadable=0 not_visited=0 "
         "unaccounted=0 ended_early=false published=true\n")


class LatestSweepWait(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.log = self.repo / ".kin" / "daemon.log"
        self.log.parent.mkdir()
        self.now = 0
        self.update = None
        self.monotonic = patch.object(brownfield.time, "monotonic", lambda: self.now)
        self.sleep = patch.object(brownfield.time, "sleep", self.tick)
        self.monotonic.start()
        self.sleep.start()
        self.addCleanup(self.monotonic.stop)
        self.addCleanup(self.sleep.stop)

    def tick(self, seconds):
        self.now += seconds
        if self.update:
            callback, self.update = self.update, None
            callback()

    def test_terminal_attempt_needs_no_wait(self):
        self.log.write_text(START + CLEAN)
        attempts = brownfield.wait_for_latest_sweep(self.log)
        self.assertEqual(self.now, 0)
        self.assertTrue(brownfield.sweep_verdict(attempts)[0])

    def test_old_finish_is_not_borrowed_for_new_running_attempt(self):
        self.log.write_text(START + CLEAN + START)
        self.update = lambda: self.log.write_text(START + CLEAN + START + CLEAN)
        attempts = brownfield.wait_for_latest_sweep(self.log)
        self.assertEqual(self.now, 1)
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[-1]["finish"], "complete")

    def test_unfinished_attempt_remains_unreadable_at_deadline(self):
        self.log.write_text(START + CLEAN + START)
        attempts = brownfield.wait_for_latest_sweep(self.log, wait_seconds=3)
        self.assertEqual(self.now, 3)
        self.assertEqual(attempts[-1]["finish"], "running")
        self.assertFalse(brownfield.sweep_verdict(attempts)[0])

    def test_interrupted_attempt_is_refused_without_starting_more_work(self):
        self.log.write_text(START + CLEAN.replace("sweep complete", "sweep interrupted; unfinished work remains owed"))
        attempts = brownfield.wait_for_latest_sweep(self.log)
        self.assertEqual(self.now, 0)
        self.assertFalse(brownfield.sweep_verdict(attempts)[0])

    def test_missing_log_is_bounded_and_never_completes(self):
        self.assertEqual(brownfield.wait_for_latest_sweep(self.log, wait_seconds=2), [])
        self.assertEqual(self.now, 2)

    def test_completed_gate_is_rechecked_after_a_new_attempt_starts(self):
        self.log.write_text(START + CLEAN)
        suite = brownfield.Suite.__new__(brownfield.Suite)
        suite.fixture = Mock(return_value=str(self.repo))
        suite.store_path = Mock(return_value=str(self.log.parent))
        with contextlib.redirect_stdout(io.StringIO()):
            suite.sweep_gate("requests")
            self.log.write_text(START + CLEAN + START)
            with self.assertRaisesRegex(brownfield.ProbeError, "started but not finished"):
                suite.sweep_gate("requests")
        self.assertEqual(self.now, 10)


if __name__ == "__main__":
    unittest.main()
