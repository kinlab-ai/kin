#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC

"""Falsify direct replacement during an existing-install installer run."""

import os
import hashlib
import json
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest


INSTALLER = Path(__file__).with_name("install.sh")


class ManagedInstallerRouting(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kin-installer-routing-")
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.root = self.home / "managed root"
        self.bin = self.root / "bin"
        self.bin.mkdir(parents=True)
        self.calls = self.home / "native calls.jsonl"
        self.tools = self.home / "tools"
        self.tools.mkdir()
        curl = self.tools / "curl"
        curl.write_text("#!/bin/sh\nprintf 'unexpected network\\n' >&2\nexit 97\n")
        curl.chmod(0o755)
        self.cli = self.bin / "kin"
        self.cli.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" >> \"$KIN_TEST_NATIVE_CALLS\"\n"
            "case \"$1\" in\n"
            " update) exit \"${KIN_TEST_UPDATE_EXIT:-0}\" ;;\n"
            " runtime) printf '{\"schema\":\"kin.runtime-status.v1\",\"ready\":true}\\n'; "
            "exit \"${KIN_TEST_STATUS_EXIT:-0}\" ;;\n"
            " *) exit 91 ;;\n"
            "esac\n"
        )
        self.cli.chmod(0o755)
        self.daemon = self.bin / "kin-daemon"
        self.daemon.write_bytes(b"existing daemon bytes")
        self.before_cli = self.cli.read_bytes()
        self.before_daemon = self.daemon.read_bytes()

    def run_installer(self, **overrides):
        env = {
            **os.environ,
            "HOME": str(self.home),
            "KIN_HOME": str(self.root),
            "KIN_NO_SETUP": "1",
            "KIN_TEST_NATIVE_CALLS": str(self.calls),
            "PATH": f"{self.tools}:/usr/bin:/bin",
        }
        for key in ["KIN_VERSION", "KIN_BASE_URL", "KIN_DIR", "KIN_RUNTIME_MODE"]:
            env.pop(key, None)
        env.update(overrides)
        return subprocess.run(
            ["sh", str(INSTALLER)], env=env, capture_output=True, text=True, timeout=10
        )

    def assert_retained(self):
        self.assertEqual(self.cli.read_bytes(), self.before_cli)
        self.assertEqual(self.daemon.read_bytes(), self.before_daemon)

    def test_existing_install_only_calls_transaction_then_status(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.calls.read_text().splitlines(), ["runtime status --json", "update --apply", "runtime status --json"])
        self.assert_retained()

    def test_refused_transaction_does_not_replace_or_download(self):
        result = self.run_installer(KIN_TEST_UPDATE_EXIT="17")
        self.assertEqual(result.returncode, 17)
        self.assertEqual(self.calls.read_text().splitlines(), ["runtime status --json", "update --apply"])
        self.assert_retained()

    def test_successful_transaction_without_ready_status_is_not_install_success(self):
        result = self.run_installer(KIN_TEST_STATUS_EXIT="1")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.calls.read_text().splitlines(), ["runtime status --json", "update --apply", "runtime status --json"])
        self.assertNotIn("Managed runtime updated and verified", result.stdout)
        self.assert_retained()


    def test_partial_existing_pair_is_not_treated_as_first_install(self):
        self.cli.unlink()
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete", result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertEqual(self.daemon.read_bytes(), self.before_daemon)

    def test_arbitrary_pin_cannot_be_reported_as_latest_update(self):
        result = self.run_installer(KIN_VERSION="0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Pinned or mirrored reinstalls", result.stderr)
        self.assertFalse(self.calls.exists())
        self.assert_retained()

    def test_windows_posix_installer_directs_powershell_before_probe_or_mutation(self):
        uname = self.tools / "uname"
        uname.write_text("#!/bin/sh\nprintf 'MINGW64_NT-10.0\\n'\n")
        uname.chmod(0o755)
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("official PowerShell installer", result.stderr)
        self.assertIn("irm https://get.kinlab.dev/install.ps1 | iex", result.stderr)
        self.assertFalse(self.calls.exists())
        self.assertNotIn("unexpected network", result.stderr)
        self.assert_retained()


class LegacyInstallerBootstrap(unittest.TestCase):
    version = "9.9.9"
    source = "b" * 40

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="kin-installer-bootstrap-")
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.root = self.home / "managed root"
        (self.root / "bin").mkdir(parents=True)
        (self.root / "lib").mkdir()
        self.stage_parent = self.home / "private stages"
        self.stage_parent.mkdir()
        self.tools = self.home / "tools"
        self.tools.mkdir()
        self.native_calls = self.home / "bootstrap.jsonl"
        self.old_calls = self.home / "old-calls.log"
        self.network_calls = self.home / "network.jsonl"
        self.complete = self.home / "native-completed"
        self.status_file = self.home / "final-status.json"
        self.cli = self.root / "bin" / "kin"
        self.daemon = self.root / "bin" / "kin-daemon"
        self.daemon.write_bytes(b"old daemon bytes")
        self.daemon.chmod(0o755)
        (self.root / "registry.json").write_bytes(b"existing registry bytes")
        (self.root / "lib" / "optional-shim").write_bytes(b"old optional bytes")
        machine = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}[platform.machine().lower()]
        os_name = {"Darwin": "macos", "Linux": "linux"}[platform.system()]
        self.target = f"{os_name}-{machine}"
        self.rust_target = f"{machine}-apple-darwin" if os_name == "macos" else f"{machine}-unknown-linux-musl"
        self.archive_name = f"kin-{self.target}.tar.gz"
        self.set_legacy_version("0.8.1")
        identity = {
            "schema": "kin.runtime-status.v1", "ready": True, "mode": "managed",
            "pair_matches": True, "restart_needed": False, "kin_home": str(self.root.resolve()),
        }
        for component, filename in (("cli", "kin"), ("daemon", "kin-daemon")):
            identity[component] = {
                "path": str((self.root / "bin" / filename).resolve()), "version": self.version,
                "commit_sha": self.source, "source_dirty": False, "target": self.rust_target,
                "build_profile": "release", "sha256": "c" * 64,
            }
        self.identity = identity
        self.status_file.write_text(json.dumps(identity))
        self.release_file = self.home / "release.json"
        self.release_file.write_text(json.dumps({"tag_name": "v" + self.version, "draft": False, "prerelease": False}))
        self.commit_file = self.home / "commit.json"
        self.commit_file.write_text(json.dumps({"ref": "refs/tags/v" + self.version, "object": {"type": "commit", "sha": self.source}, "target_commitish": "ignored-branch"}))
        archive_root = self.home / f"kin-{self.target}"
        archive_root.mkdir()
        staged = archive_root / "kin"
        staged.write_text(
            f"#!{sys.executable}\n"
            "import json,os,pathlib,sys\n"
            "record={'argv':sys.argv[1:],'cwd':os.getcwd(),'kin_home':os.environ.get('KIN_HOME'),'kin_dir':os.environ.get('KIN_DIR'),'mode':os.environ.get('KIN_RUNTIME_MODE'),'overrides':{name:os.environ.get(name) for name in ('KIN_DAEMON_BIN','KIN_MANAGED_BIN','KIN_BINARY_PATH','KIN_MCP_KIN_BINARY','KIN_DAEMON_URL')}}\n"
            "with open(os.environ['KIN_TEST_BOOTSTRAP_CALLS'],'a') as f: f.write(json.dumps(record)+'\\n')\n"
            "if sys.argv[1:3] != ['update','--bootstrap-published']: sys.exit(91)\n"
            "result=int(os.environ.get('KIN_TEST_BOOTSTRAP_EXIT','0'))\n"
            "if result == 0: pathlib.Path(os.environ['KIN_TEST_COMPLETED']).write_text('native transaction complete')\n"
            "sys.exit(result)\n"
        )
        staged.chmod(0o755)
        (archive_root / "kin-daemon").write_bytes(b"incoming daemon bytes")
        if os_name == "macos":
            contents = archive_root / "KinNotifier.app" / "Contents"
            (contents / "MacOS").mkdir(parents=True)
            (contents / "Info.plist").write_text("<plist>fixture</plist>")
            notifier = contents / "MacOS" / "KinNotifier"
            notifier.write_text("#!/bin/sh\nexit 0\n")
            notifier.chmod(0o755)
        self.archive = self.home / self.archive_name
        with tarfile.open(self.archive, "w:gz") as bundle:
            bundle.add(archive_root, arcname=archive_root.name)
        self.digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.checksum = self.home / (self.archive_name + ".sha256")
        self.checksum.write_text(f"{self.digest}  {self.archive_name}\n")
        self.network_map = self.home / "network-map.json"
        self.network_map.write_text(json.dumps({
            f"https://api.github.com/repos/kinlab-ai/kin/releases/tags/v{self.version}": str(self.release_file),
            f"https://api.github.com/repos/kinlab-ai/kin/git/ref/tags/v{self.version}": str(self.commit_file),
            f"https://github.com/kinlab-ai/kin/releases/download/v{self.version}/{self.archive_name}": str(self.archive),
            f"https://github.com/kinlab-ai/kin/releases/download/v{self.version}/{self.archive_name}.sha256": str(self.checksum),
        }))
        curl = self.tools / "curl"
        curl.write_text(
            f"#!{sys.executable}\n"
            "import json,os,shutil,sys\n"
            "args=sys.argv[1:]; url=next(a for a in args if a.startswith('https://'))\n"
            "with open(os.environ['KIN_TEST_NETWORK_CALLS'],'a') as f: f.write(json.dumps({'url':url})+'\\n')\n"
            "if url.endswith('/releases/latest'): print('https://github.com/kinlab-ai/kin/releases/tag/v9.9.9',end=''); sys.exit(0)\n"
            "mapping=json.load(open(os.environ['KIN_TEST_NETWORK_MAP']))\n"
            "if url not in mapping: sys.exit(97)\n"
            "shutil.copyfile(mapping[url],args[args.index('-o')+1])\n"
        )
        curl.chmod(0o755)
        self.baseline = self.snapshot()

    def set_legacy_version(self, version):
        self.cli.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" >> \"$KIN_TEST_OLD_CALLS\"\n"
            "if [ \"$1\" = runtime ] && [ -f \"$KIN_TEST_COMPLETED\" ]; then\n"
            " cat \"$KIN_TEST_FINAL_STATUS\"; exit \"${KIN_TEST_FINAL_STATUS_EXIT:-0}\"\nfi\n"
            f"if [ \"$1\" = --version ]; then printf 'kin {version}\\n'; exit 0; fi\n"
            "printf 'unknown command runtime\\n' >&2\nexit 2\n"
        )
        self.cli.chmod(0o755)

    def snapshot(self):
        return {str(p.relative_to(self.root)): (stat.S_IMODE(p.stat().st_mode), p.read_bytes() if p.is_file() else None)
                for p in self.root.rglob("*")}

    def run_installer(self, **overrides):
        env = {**os.environ, "HOME": str(self.home), "TMPDIR": str(self.stage_parent),
               "KIN_HOME": str(self.root), "KIN_DIR": "ignored alias", "KIN_RUNTIME_MODE": "development",
               "KIN_NO_SETUP": "1", "KIN_TEST_BOOTSTRAP_CALLS": str(self.native_calls),
               "KIN_TEST_OLD_CALLS": str(self.old_calls), "KIN_TEST_COMPLETED": str(self.complete),
               "KIN_TEST_FINAL_STATUS": str(self.status_file), "KIN_TEST_NETWORK_MAP": str(self.network_map),
               "KIN_TEST_NETWORK_CALLS": str(self.network_calls),
               "KIN_DAEMON_BIN": "/unrelated/custom-daemon", "KIN_MANAGED_BIN": "/unrelated/custom-kin",
               "KIN_BINARY_PATH": "/unrelated/custom-cli", "KIN_MCP_KIN_BINARY": "/unrelated/custom-mcp",
               "KIN_DAEMON_URL": "http://unrelated-daemon.invalid",
               "PATH": f"{self.tools}:{Path(sys.executable).parent}:/usr/bin:/bin"}
        for key in ("KIN_VERSION", "KIN_BASE_URL"):
            env.pop(key, None)
        env.update(overrides)
        return subprocess.run(["sh", str(INSTALLER)], env=env, capture_output=True, text=True, timeout=20, cwd=self.home)

    def test_legacy_081_and_086_stage_exact_official_tuple_without_direct_mutation(self):
        for version in ("0.8.1", "0.8.6"):
            with self.subTest(version=version):
                self.set_legacy_version(version)
                baseline = self.snapshot()
                self.complete.unlink(missing_ok=True)
                self.native_calls.unlink(missing_ok=True)
                self.old_calls.unlink(missing_ok=True)
                result = self.run_installer()
                self.assertEqual(result.returncode, 0, result.stderr)
                calls = [json.loads(line) for line in self.native_calls.read_text().splitlines()]
                self.assertEqual(len(calls), 1)
                self.assertEqual(calls[0]["argv"], ["update", "--bootstrap-published", "--channel", "stable",
                    "--expect-version", self.version, "--expect-sha", self.source,
                    "--expect-archive-sha256", self.digest, "--expect-target", self.rust_target,
                    "--expect-archive-name", self.archive_name, "--apply"])
                self.assertEqual(calls[0]["kin_home"], str(self.root))
                self.assertIsNone(calls[0]["kin_dir"])
                self.assertEqual(calls[0]["mode"], "managed")
                self.assertTrue(all(value is None for value in calls[0]["overrides"].values()))
                self.assertTrue(Path(calls[0]["cwd"]).is_relative_to(self.stage_parent))
                self.assertFalse(Path(calls[0]["cwd"]).is_relative_to(self.root))
                self.assertEqual(self.old_calls.read_text().splitlines(), ["runtime status --json", "runtime status --json"])
                self.assertEqual(self.snapshot(), baseline)
                self.assertEqual(list(self.stage_parent.iterdir()), [])

    def test_native_preflight_refusal_preserves_installed_state_and_exact_stage(self):
        result = self.run_installer(KIN_TEST_BOOTSTRAP_EXIT="17")
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertEqual(self.old_calls.read_text().splitlines(), ["runtime status --json"])
        stages = list(self.stage_parent.iterdir())
        self.assertEqual(len(stages), 1)
        self.assertEqual(hashlib.sha256((stages[0] / self.archive_name).read_bytes()).hexdigest(), self.digest)
        self.assertTrue((stages[0] / "release.json").is_file())
        self.assertTrue((stages[0] / "tag-ref.json").is_file())
        self.assertIn("Migration is pending", result.stderr)
        self.assertIn(str(stages[0]), result.stderr)

    def test_final_status_failure_retains_stage_and_never_claims_completion(self):
        self.identity["ready"] = False
        self.status_file.write_text(json.dumps(self.identity))
        result = self.run_installer(KIN_TEST_FINAL_STATUS_EXIT="1")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertEqual(len(list(self.stage_parent.iterdir())), 1)
        self.assertIn("Completion remains pending", result.stderr)
        self.assertNotIn("Managed runtime migrated and verified", result.stdout)

    def test_same_version_wrong_source_does_not_pass_final_proof(self):
        self.identity["daemon"]["commit_sha"] = "d" * 40
        self.status_file.write_text(json.dumps(self.identity))
        result = self.run_installer()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertEqual(len(list(self.stage_parent.iterdir())), 1)
        self.assertNotIn("Managed runtime migrated and verified", result.stdout)

    def test_unresolved_tag_or_draft_release_refuses_before_native_execution(self):
        for bad_metadata in ({"object": {"type": "commit", "sha": "main"}}, {"object": {"type": "commit", "sha": "b" * 39}}, {"object": {"type": "tree", "sha": "b" * 40}}):
            with self.subTest(metadata=bad_metadata):
                self.commit_file.write_text(json.dumps({"ref": "refs/tags/v" + self.version, **bad_metadata}))
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.snapshot(), self.baseline)
                self.assertFalse(self.native_calls.exists())
                self.assertEqual(list(self.stage_parent.iterdir()), [])
        self.commit_file.write_text(json.dumps({"ref": "refs/tags/v" + self.version, "object": {"type": "commit", "sha": self.source}}))
        self.release_file.write_text(json.dumps({"tag_name": "v" + self.version, "draft": True, "prerelease": False}))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertFalse(self.native_calls.exists())

    def test_archive_checksum_mismatch_refuses_before_native_execution(self):
        self.checksum.write_text(f"{'f' * 64}  {self.archive_name}\n")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("SHA-256 checksum mismatch", result.stderr)
        self.assertFalse(self.native_calls.exists())
        self.assertEqual(self.snapshot(), self.baseline)

    def test_relative_original_home_survives_private_staging_cwd(self):
        result = self.run_installer(KIN_HOME="managed root")
        self.assertEqual(result.returncode, 0, result.stderr)
        call = json.loads(self.native_calls.read_text())
        self.assertEqual(call["kin_home"], str(self.root))
        self.assertEqual(self.snapshot(), self.baseline)

    def test_stage_parent_inside_managed_home_refuses_without_creating_entries(self):
        result = self.run_installer(TMPDIR=str(self.root))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside the managed Kin home", result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertFalse(self.native_calls.exists())

    def test_annotated_tag_is_peeled_and_annotation_identity_is_verified(self):
        tag_sha = "e" * 40
        self.commit_file.write_text(json.dumps({"ref": "refs/tags/v" + self.version,
            "object": {"type": "tag", "sha": tag_sha}}))
        annotation = self.home / "annotation.json"
        annotation.write_text(json.dumps({"sha": tag_sha, "object": {"type": "commit", "sha": self.source}}))
        mapping = json.loads(self.network_map.read_text())
        mapping[f"https://api.github.com/repos/kinlab-ai/kin/git/tags/{tag_sha}"] = str(annotation)
        self.network_map.write_text(json.dumps(mapping))
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        call = json.loads(self.native_calls.read_text())
        self.assertEqual(call["argv"][call["argv"].index("--expect-sha") + 1], self.source)
        self.assertEqual(self.snapshot(), self.baseline)
        self.complete.unlink()
        self.native_calls.unlink()
        annotation.write_text(json.dumps({"sha": "f" * 40, "object": {"type": "commit", "sha": self.source}}))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requested tag object", result.stderr)
        self.assertFalse(self.native_calls.exists())
        self.assertEqual(self.snapshot(), self.baseline)

    def test_missing_json_parser_refuses_legacy_migration_without_installed_mutation(self):
        no_parser_tools = self.home / "tools-without-parser"
        no_parser_tools.mkdir()
        for command in ("sh", "uname", "git", "grep", "sed", "mktemp", "chmod", "rm", "id", "tr", "awk"):
            executable = shutil.which(command)
            if executable is not None:
                (no_parser_tools / command).symlink_to(executable)
        (no_parser_tools / "curl").symlink_to(self.tools / "curl")
        result = self.run_installer(PATH=str(no_parser_tools))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("requires python3, node, or jq", result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)
        self.assertFalse(self.native_calls.exists())
        self.assertEqual(list(self.stage_parent.iterdir()), [])

    def test_annotated_tag_cycle_has_a_bounded_refusal(self):
        tag_sha = "e" * 40
        self.commit_file.write_text(json.dumps({"ref": "refs/tags/v" + self.version,
            "object": {"type": "tag", "sha": tag_sha}}))
        annotation = self.home / "cyclic-annotation.json"
        annotation.write_text(json.dumps({"sha": tag_sha, "object": {"type": "tag", "sha": tag_sha}}))
        mapping = json.loads(self.network_map.read_text())
        annotation_url = f"https://api.github.com/repos/kinlab-ai/kin/git/tags/{tag_sha}"
        mapping[annotation_url] = str(annotation)
        self.network_map.write_text(json.dumps(mapping))
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("annotation depth", result.stderr)
        requests = [json.loads(line)["url"] for line in self.network_calls.read_text().splitlines()]
        self.assertEqual(requests.count(annotation_url), 5)
        self.assertFalse(self.native_calls.exists())
        self.assertEqual(self.snapshot(), self.baseline)

    def test_same_version_wrong_target_dirty_source_or_restart_fence_is_not_ready(self):
        for component, field, value in (("cli", "target", "different-target"),
                                        ("daemon", "source_dirty", True),
                                        (None, "restart_needed", True)):
            with self.subTest(field=field):
                status = json.loads(json.dumps(self.identity))
                (status[component] if component else status)[field] = value
                self.status_file.write_text(json.dumps(status))
                self.complete.unlink(missing_ok=True)
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("Managed runtime migrated and verified", result.stdout)
                self.assertEqual(self.snapshot(), self.baseline)

    def test_native_report_with_equivalent_physical_parent_alias_is_accepted(self):
        alias = self.home / "root alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.identity["kin_home"] = str(alias)
        self.identity["cli"]["path"] = str(alias / "bin" / "kin")
        self.identity["daemon"]["path"] = str(alias / "bin" / "kin-daemon")
        self.status_file.write_text(json.dumps(self.identity))
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.snapshot(), self.baseline)



if __name__ == "__main__":
    unittest.main()
