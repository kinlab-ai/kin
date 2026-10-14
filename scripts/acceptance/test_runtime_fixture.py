"""Consumer contract tests. No product daemon or operator home is used."""

import copy
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
import urllib.error
import urllib.request
from unittest.mock import patch

from runtime_fixture import (BUILD_FIELDS, IDENTITY_FIELDS, IDENTITY_HEADER,
                             native_request_headers, prepare_runtime_fixture,
                             runtime_fingerprint, runtime_namespace)


IDENTITY = dict(zip(IDENTITY_FIELDS, (
    "0.8.7", "0123456789abcdef0123456789abcdef01234567", False, True,
    "aarch64-apple-darwin", "release",
)))


class RuntimeFixtureTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.cli = self.root / "source" / "kin"
        self.daemon = self.cli.with_name("kin-daemon")
        self.cli.parent.mkdir()
        self.cli.write_bytes(b"selected-cli-bytes")
        self.daemon.write_bytes(b"selected-daemon-bytes")
        self.home = self.root / "fixture-home"
        self.env = {}
        self.identity = copy.deepcopy(IDENTITY)
        self.commands = []

    def probe(self, command, env, cwd=None):
        self.commands.append(command)
        if "bench-meta" in command:
            return dict(zip(BUILD_FIELDS, self.identity.values()),
                        schema="kin.runtime-build-identity.v1")
        if "--compat-json" in command:
            return {"runtime_identity": self.identity}
        self.assertEqual(command[1:], ["runtime", "status", "--json"])
        self.assertEqual(env["KIN_HOME"], str(self.home))
        self.assertEqual(env["KIN_RUNTIME_MODE"], "managed")
        status = {"schema": "kin.runtime-status.v1", "mode": "managed",
                  "ready": True, "pair_matches": True}
        for key, name in (("cli", "kin"), ("daemon", "kin-daemon")):
            path = self.home / "bin" / name
            status[key] = dict(self.identity, path=str(path),
                               sha256=hashlib.sha256(path.read_bytes()).hexdigest())
        return status

    def test_managed_fixture_copies_verified_pair_and_checks_canonical_status(self):
        with patch("runtime_fixture._probe", side_effect=self.probe):
            cli, daemon = prepare_runtime_fixture(self.cli, self.daemon, self.env, self.home)
        self.assertEqual(Path(cli).read_bytes(), self.cli.read_bytes())
        self.assertEqual(Path(daemon).read_bytes(), self.daemon.read_bytes())
        self.assertEqual(self.env["KIN_DAEMON_BIN"], daemon)
        self.assertEqual(len(self.commands), 3)
        receipt = json.loads((self.home / "acceptance-runtime.json").read_text())
        self.assertEqual(receipt["mode"], "managed")
        self.assertEqual(receipt["release_kind"], "unknown")

    def test_development_fixture_never_claims_managed_or_copies_pair(self):
        self.identity.update(source_dirty=True, build_profile="debug")
        with patch("runtime_fixture._probe", side_effect=self.probe):
            cli, daemon = prepare_runtime_fixture(self.cli, self.daemon, self.env, self.home)
        self.assertEqual(cli, str(self.cli))
        self.assertEqual(daemon, str(self.daemon))
        self.assertEqual(self.env["KIN_RUNTIME_MODE"], "development")
        self.assertFalse((self.home / "bin").exists())
        self.assertEqual(len(self.commands), 2)

    def test_clean_unknown_release_is_refused(self):
        self.identity["source_known"] = False
        with patch("runtime_fixture._probe", side_effect=self.probe):
            with self.assertRaisesRegex(ValueError, "known full source"):
                prepare_runtime_fixture(self.cli, self.daemon, self.env, self.home)
        self.assertFalse((self.home / "bin").exists())

    def test_mismatched_pair_is_refused_before_copy(self):
        metadata = dict(zip(BUILD_FIELDS, self.identity.values()),
                        schema="kin.runtime-build-identity.v1")
        wrong = dict(self.identity, commit_sha="f" * 40)
        with patch("runtime_fixture._probe", side_effect=[metadata, {"runtime_identity": wrong}]):
            with self.assertRaisesRegex(ValueError, "different build identities"):
                prepare_runtime_fixture(self.cli, self.daemon, self.env, self.home)
        self.assertFalse((self.home / "bin").exists())

    def test_existing_different_canonical_bytes_are_not_overwritten(self):
        target = self.home / "bin" / "kin"
        target.parent.mkdir(parents=True)
        target.write_bytes(b"another-fixture-cli")
        with patch("runtime_fixture._probe", side_effect=self.probe):
            with self.assertRaisesRegex(ValueError, "already differs"):
                prepare_runtime_fixture(self.cli, self.daemon, self.env, self.home)
        self.assertEqual(target.read_bytes(), b"another-fixture-cli")

    def test_fingerprint_matches_independent_wire_format_vector(self):
        scrambled = dict(reversed(tuple(IDENTITY.items())))
        value = runtime_fingerprint(scrambled, "managed", "namespace-test-token")
        self.assertEqual(value, "eb3e41afeee8714da6f0157260842e1c9a6fc57f02e98fc9ab3bc0869090435d")
        self.assertNotEqual(value, runtime_fingerprint(scrambled, "development", "namespace-test-token"))
        self.assertNotEqual(value, runtime_fingerprint(scrambled, "managed", "other-namespace"))

    def service(self, **overrides):
        self.home.mkdir(exist_ok=True)
        return dict({"schema": "kin.runtime-service.v1", "component": "daemon",
                     "identity": self.identity, "mode": "managed", "pid": 1,
                     "namespace": runtime_namespace(self.home)}, **overrides)

    def headers(self, service):
        with patch("runtime_fixture._probe", side_effect=self.probe), \
                patch("runtime_fixture.urllib.request.urlopen",
                      return_value=io.BytesIO(json.dumps(service).encode())) as opened:
            headers = native_request_headers(self.cli, {"KIN_RUNTIME_MODE": "managed",
                                                       "KIN_HOME": str(self.home)},
                                             "http://127.0.0.1:12345/lsp/sweep/status",
                                             token="fixture-token")
        self.assertEqual(opened.call_args.args[0].full_url,
                         "http://127.0.0.1:12345/runtime/identity")
        return headers

    def test_positive_native_request_verifies_selected_cli_before_deriving_header(self):
        headers = self.headers(self.service())
        self.assertEqual(headers["Authorization"], "Bearer fixture-token")
        self.assertEqual(headers[IDENTITY_HEADER], runtime_fingerprint(
            IDENTITY, "managed", runtime_namespace(self.home)))
        self.assertEqual(self.commands[0][1:],
                         ["bench-meta", "--json", "--runtime-identity-only"])

    def test_header_refuses_wrong_build_mode_namespace_or_hosted_component(self):
        for service in (self.service(identity=dict(IDENTITY, version="0.8.6")),
                        self.service(mode="development"), self.service(namespace=None),
                        self.service(namespace="different-same-build-fixture"),
                        self.service(component="hosted-daemon"), self.service(schema="legacy")):
            with self.subTest(service=service), self.assertRaises(ValueError):
                self.headers(service)

    def test_hosted_url_is_not_given_native_headers(self):
        with patch("runtime_fixture._probe") as probe:
            with self.assertRaisesRegex(ValueError, "controlled loopback"):
                native_request_headers(self.cli, {}, "https://hosted.invalid/locate")
        probe.assert_not_called()

    def test_same_path_replacement_changes_namespace(self):
        self.home.mkdir()
        original = runtime_namespace(self.home)
        self.home.rename(self.root / "retained-original-home")
        self.home.mkdir()
        self.assertNotEqual(original, runtime_namespace(self.home))

    def test_controlled_peer_requires_handshake_and_refuses_before_handlers(self):
        from memory_pressure_refusal import DaemonStub

        service = self.service()
        env = {"KIN_HOME": str(self.home), "KIN_RUNTIME_MODE": "managed"}
        with DaemonStub(200, {}, runtime_metadata=service) as peer:
            url = peer.url + "/mcp/tools/call"
            body = json.dumps({"name": "semantic_locate"}).encode()
            for headers in ({}, {IDENTITY_HEADER: "wrong-generation"}):
                request = urllib.request.Request(url, data=body, headers=headers)
                with self.assertRaises(urllib.error.HTTPError) as error:
                    urllib.request.urlopen(request, timeout=5)
                self.assertEqual(error.exception.code, 409)
                error.exception.close()
                self.assertEqual(peer.handler_calls, 0)
            with patch("runtime_fixture._probe", side_effect=self.probe):
                headers = native_request_headers(self.cli, env, url)
            request = urllib.request.Request(url, data=body, headers=headers)
            with urllib.request.urlopen(request, timeout=5) as response:
                self.assertEqual(response.status, 200)
                self.assertTrue(json.load(response)["content"])
            self.assertEqual(peer.handler_calls, 1)

    def test_coverage_control_keeps_selected_pair_and_requires_owned_stop(self):
        from coverage_read_open import (IN_SYNC, PASS, UNMEASURED_BASIS,
                                        UNREADABLE, check_diverged)

        for all_stopped in (True, False):
            with self.subTest(all_stopped=all_stopped):
                repo = self.root / ("coverage-" + str(all_stopped))
                (repo / "src").mkdir(parents=True)
                original_env = {"KIN_RUNTIME_MODE": "managed",
                                "KIN_DAEMON_BIN": "selected-canonical-daemon",
                                "KIN_DAEMON_AUTH_TOKEN": "fixture-token"}
                test = self

                class Suite:
                    def status(self):
                        return {IN_SYNC: True}, ""

                    def status_text(self):
                        if self.env is original_env:
                            return "as admitted 0s ago"
                        test.assertEqual(self.env["KIN_DAEMON_BIN"], original_env["KIN_DAEMON_BIN"])
                        test.assertEqual(self.env["KIN_RUNTIME_MODE"], "managed")
                        test.assertEqual(self.env["KIN_DAEMON_BIND_HOST"], "0.0.0.0")
                        test.assertEqual(self.env["KIN_DAEMON_REQUIRE_TOKEN"], "0")
                        test.assertNotIn("KIN_DAEMON_AUTH_TOKEN", self.env)
                        return UNMEASURED_BASIS

                    def kin_run(self, args):
                        test.assertEqual(args, ["daemon", "stop", "--json"])
                        return 0, json.dumps({"schema": "kin.daemon-stop.v1",
                                              "scope": "current-repo",
                                              "all_stopped": all_stopped})

                suite = Suite()
                suite.repo = str(repo)
                suite.env = original_env
                result = check_diverged(suite)
                self.assertEqual(result.status, PASS if all_stopped else UNREADABLE)
                self.assertIs(suite.env, original_env)


if __name__ == "__main__":
    unittest.main()
