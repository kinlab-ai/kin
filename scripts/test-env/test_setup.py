# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Test the nextest setup boundary without starting Cargo or a daemon."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SETUP = Path(__file__).with_name("setup.sh")
RUNNER = Path(__file__).with_name("cargo-runner.sh")

# A stand-in for any executable Cargo starts: prints its arguments and the
# environment the runner may set, one per line, then exits with $PROBE_EXIT or
# kills itself with $PROBE_SIGNAL.
PROBE = """#!/bin/sh
for arg in "$@"; do printf 'arg=%s\\n' "$arg"; done
printf 'KIN_REGISTRY_PATH=%s\\n' "${KIN_REGISTRY_PATH-<unset>}"
printf 'KIN_HOME=%s\\n' "${KIN_HOME-<unset>}"
printf 'KIN_TEST_FLUSH=%s\\n' "${KIN_TEST_FLUSH-<unset>}"
printf 'TMPDIR=%s\\n' "${TMPDIR-<unset>}"
if [ -n "${PROBE_SIGNAL:-}" ]; then kill -s "$PROBE_SIGNAL" $$; fi
exit "${PROBE_EXIT:-0}"
"""


class SharedScratchTests(unittest.TestCase):
    def test_shared_scratch_keeps_stores_and_projection_on_one_filesystem(self):
        for suffix in ("", "/"):
            with self.subTest(suffix=suffix), tempfile.TemporaryDirectory(prefix="kn-") as owned:
                scratch = Path(owned)
                exports = scratch / "nextest.env"
                inherited = str(scratch) + suffix
                env = dict(os.environ, NEXTEST_ENV=str(exports), TMPDIR=inherited,
                           KIN_TEST_SHARED_TMPDIR="1", KIN_TEST_FULL_FLUSH="0")
                subprocess.run(["sh", str(SETUP)], env=env, check=True,
                               capture_output=True, text=True)
                settings = dict(line.split("=", 1) for line in exports.read_text().splitlines())
                self.assertNotIn("TMPDIR", settings, "shared mode must retain the caller's scratch")
                registry = Path(settings["KIN_REGISTRY_PATH"])
                self.assertEqual(Path(settings["KIN_HOME"]), registry.parent)
                self.assertTrue(registry.is_relative_to(scratch), settings)
                self.assertEqual(registry.parent.stat().st_dev, scratch.stat().st_dev)
                self.assertEqual(registry.parent.stat().st_mode & 0o777, 0o700)
                self.assertEqual(settings["KIN_TEST_FLUSH"], "fsync")

                # The failure this setup caused was EXDEV during this exact
                # store-to-working-tree rename. Prove the required operation.
                store = registry.parent / "stores" / "repository"
                store.mkdir(parents=True)
                staged = store / "staged-source"
                staged.write_bytes(b"answer: 42\n")
                working = scratch / "working-tree"
                working.mkdir()
                published = working / "answer.yaml"
                staged.rename(published)
                self.assertEqual(published.read_bytes(), b"answer: 42\n")

    def test_invalid_shared_switch_refuses_before_creating_a_run(self):
        with tempfile.TemporaryDirectory(prefix="kn-") as owned:
            scratch = Path(owned)
            exports = scratch / "nextest.env"
            env = dict(os.environ, NEXTEST_ENV=str(exports), TMPDIR=str(scratch),
                       KIN_TEST_SHARED_TMPDIR="yes", KIN_TEST_FULL_FLUSH="0")
            result = subprocess.run(["sh", str(SETUP)], env=env,
                                    capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("KIN_TEST_SHARED_TMPDIR must be unset, 0 or 1", result.stderr)
            self.assertEqual(list(scratch.iterdir()), [])


class CargoRunnerTests(unittest.TestCase):
    """The runner Cargo starts every executable through (.cargo/config.toml)."""

    def run_through_runner(self, directory, args, **env_changes):
        probe = Path(directory) / "probe"
        probe.parent.mkdir(parents=True, exist_ok=True)
        probe.write_text(PROBE)
        probe.chmod(0o755)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("NEXTEST", "KIN_"))}
        env.update(env_changes)
        result = subprocess.run([str(RUNNER), str(probe), *args], env=env,
                                capture_output=True, text=True)
        seen = dict(line.split("=", 1) for line in result.stdout.splitlines()
                    if not line.startswith("arg="))
        passed = [line[4:] for line in result.stdout.splitlines() if line.startswith("arg=")]
        return result, passed, seen

    def test_a_program_cargo_runs_passes_through_unchanged(self):
        args = ["--flag", "two words", "", "*", "$HOME", "-"]
        with tempfile.TemporaryDirectory(prefix="kn-") as owned:
            result, passed, seen = self.run_through_runner(
                Path(owned) / "target/debug", args, PROBE_EXIT="7", TMPDIR=owned)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(passed, args)
        self.assertEqual(seen["KIN_REGISTRY_PATH"], "<unset>")
        self.assertEqual(seen["KIN_HOME"], "<unset>")
        self.assertEqual(seen["KIN_TEST_FLUSH"], "<unset>")
        self.assertEqual(seen["TMPDIR"], owned)

    def test_a_signal_that_ends_a_program_reaches_cargo(self):
        with tempfile.TemporaryDirectory(prefix="kn-") as owned:
            result, _, _ = self.run_through_runner(
                Path(owned) / "target/debug", [], PROBE_SIGNAL="TERM")
        self.assertEqual(result.returncode, -15, result.stderr)

    def test_a_test_binary_starts_in_the_test_environment(self):
        with tempfile.TemporaryDirectory(prefix="kn-") as owned:
            result, passed, seen = self.run_through_runner(
                Path(owned) / "target/debug/deps", ["--exact", "a::test"], PROBE_EXIT="3",
                TMPDIR=owned, KIN_TEST_SHARED_TMPDIR="1")
            self.assertEqual(result.returncode, 3, result.stderr)
            self.assertEqual(passed, ["--exact", "a::test"])
            registry = Path(seen["KIN_REGISTRY_PATH"])
            self.assertEqual(Path(seen["KIN_HOME"]), registry.parent)
            self.assertTrue(registry.is_relative_to(owned), seen)
            self.assertTrue(registry.parent.is_dir(), seen)
            self.assertEqual(seen["KIN_TEST_FLUSH"], "fsync")
            self.assertEqual(seen["TMPDIR"], owned, "shared mode keeps the caller's TMPDIR")

    def test_nextest_has_already_set_the_environment(self):
        with tempfile.TemporaryDirectory(prefix="kn-") as owned:
            for marker in ({"NEXTEST": "1"}, {"NEXTEST_RUN_ID": "run"}):
                with self.subTest(marker=marker):
                    result, _, seen = self.run_through_runner(
                        Path(owned) / "target/debug/deps", [], TMPDIR=owned, **marker)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(seen["KIN_REGISTRY_PATH"], "<unset>")
                    self.assertEqual(seen["KIN_HOME"], "<unset>")
                    self.assertEqual(seen["TMPDIR"], owned)


if __name__ == "__main__":
    unittest.main()
