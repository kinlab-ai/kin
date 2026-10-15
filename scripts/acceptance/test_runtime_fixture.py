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
                             REPOSITORY_IDENTITY_HEADER, fixture_daemon_metadata,
                             native_request_headers, prepare_runtime_fixture,
                             repository_fingerprint, runtime_fingerprint,
                             runtime_namespace)


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
        self.repo = self.root / "fixture-repo"
        self.repo.mkdir()
        self.store = self.root / "selected-store"
        self.store.mkdir()
        (self.store / "manifest.json").write_text(json.dumps({"repo_id": "legacy/名"}))

    def probe(self, command, env, cwd=None):
        self.commands.append(command)
        if "bench-meta" in command:
            return dict(zip(BUILD_FIELDS, self.identity.values()),
                        schema="kin.runtime-build-identity.v1")
        if "--compat-json" in command:
            return {"runtime_identity": self.identity}
        if command[1:] == ["store", "path", "--json"]:
            self.assertEqual(cwd, str(self.repo))
            return {"schema": "kin.store-path.v1", "store": str(self.store),
                    "working_dir": str(self.repo), "external": True}
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
                     "namespace": runtime_namespace(self.home),
                     "repository": {"repository_id": "legacy/名",
                                    "store_namespace": runtime_namespace(self.store)}}, **overrides)

    def headers(self, service, endpoint="/lsp/sweep/status", on_metadata=None):
        def reply(*args, **kwargs):
            if on_metadata:
                on_metadata()
            return io.BytesIO(json.dumps(service).encode())

        with patch("runtime_fixture._probe", side_effect=self.probe), \
                patch("runtime_fixture.urllib.request.urlopen",
                      side_effect=reply) as opened:
            headers = native_request_headers(self.cli, {"KIN_RUNTIME_MODE": "managed",
                                                       "KIN_HOME": str(self.home)},
                                             "http://127.0.0.1:12345" + endpoint,
                                             token="fixture-token", cwd=self.repo)
        self.assertEqual(opened.call_args.args[0].full_url,
                         "http://127.0.0.1:12345/runtime/identity")
        return headers

    def test_positive_native_request_verifies_selected_cli_before_deriving_header(self):
        headers = self.headers(self.service())
        self.assertEqual(headers["Authorization"], "Bearer fixture-token")
        self.assertEqual(headers[IDENTITY_HEADER], runtime_fingerprint(
            IDENTITY, "managed", runtime_namespace(self.home)))
        self.assertEqual(headers[REPOSITORY_IDENTITY_HEADER], repository_fingerprint(
            {"repository_id": "legacy/名", "store_namespace": runtime_namespace(self.store)}))
        self.assertEqual(self.commands[0][1:],
                         ["bench-meta", "--json", "--runtime-identity-only"])

    def test_repository_fingerprint_matches_independent_utf8_wire_vector(self):
        claim = {"store_namespace": "namespace-token", "repository_id": "legacy/名"}
        # Fixed bytes hashed independently of this helper's JSON serializer.
        expected = "ce6ab8d12b725df66b4fd95d79f8da5b281e9ece8c2f267abc2ae6346d9cee30"
        self.assertEqual(repository_fingerprint(claim), expected)
        for altered in (dict(claim, repository_id="other"), dict(claim, store_namespace="other")):
            self.assertNotEqual(repository_fingerprint(altered), expected)

    def test_protected_headers_refuse_absent_malformed_or_swapped_repository_claim(self):
        for claim in (None, {}, {"repository_id": "legacy/名"},
                      {"repository_id": "", "store_namespace": "x"},
                      {"repository_id": 1, "store_namespace": "x"},
                      {"repository_id": "other", "store_namespace": runtime_namespace(self.store)},
                      {"repository_id": "legacy/名", "store_namespace": "other-native-store"}):
            with self.subTest(claim=claim), self.assertRaises(ValueError):
                self.headers(self.service(repository=claim))

    def test_selected_manifest_identity_must_be_present_nonempty_string(self):
        for manifest in ({}, {"repo_id": ""}, {"repo_id": "   "}, {"repo_id": 1}):
            (self.store / "manifest.json").write_text(json.dumps(manifest))
            self.home.mkdir(exist_ok=True)
            env = {"KIN_HOME": str(self.home), "KIN_RUNTIME_MODE": "managed"}
            with self.subTest(manifest=manifest), patch("runtime_fixture._probe", side_effect=self.probe), \
                    patch("runtime_fixture.urllib.request.urlopen") as opened:
                with self.assertRaises(ValueError):
                    native_request_headers(self.cli, env, "http://127.0.0.1:12345/locate", cwd=self.repo)
            opened.assert_not_called()

    def test_store_config_change_during_metadata_is_refused(self):
        def replace_manifest():
            (self.store / "manifest.json").write_text(json.dumps({"repo_id": "changed"}))
        with self.assertRaisesRegex(ValueError, "changed during metadata"):
            self.headers(self.service(), on_metadata=replace_manifest)

    def test_same_repo_id_with_replaced_native_store_during_metadata_is_refused(self):
        def replace_store():
            self.store.rename(self.root / "retained-original-store")
            self.store.mkdir()
            (self.store / "manifest.json").write_text(json.dumps({"repo_id": "legacy/名"}))
        with self.assertRaisesRegex(ValueError, "changed during metadata"):
            self.headers(self.service(), on_metadata=replace_store)

    def test_store_replacement_during_anchored_manifest_read_is_refused(self):
        import os
        original_fdopen = os.fdopen

        def replace_before_read(descriptor, *args, **kwargs):
            self.store.rename(self.root / "retained-manifest-store")
            self.store.mkdir()
            (self.store / "manifest.json").write_text(json.dumps({"repo_id": "other"}))
            return original_fdopen(descriptor, *args, **kwargs)

        self.home.mkdir()
        env = {"KIN_HOME": str(self.home), "KIN_RUNTIME_MODE": "managed"}
        with patch("runtime_fixture._probe", side_effect=self.probe), \
                patch("runtime_fixture.os.fdopen", side_effect=replace_before_read), \
                patch("runtime_fixture.urllib.request.urlopen") as opened:
            with self.assertRaisesRegex(ValueError, "changed during manifest"):
                native_request_headers(self.cli, env, "http://127.0.0.1:12345/locate", cwd=self.repo)
        opened.assert_not_called()

    def test_selected_cli_must_bind_the_intended_working_directory(self):
        self.home.mkdir()
        env = {"KIN_HOME": str(self.home), "KIN_RUNTIME_MODE": "managed"}

        def wrong_directory(command, env, cwd=None):
            value = self.probe(command, env, cwd)
            if "store" in command:
                value["working_dir"] = str(self.root)
            return value

        with patch("runtime_fixture._probe", side_effect=wrong_directory), \
                patch("runtime_fixture.urllib.request.urlopen") as opened:
            with self.assertRaisesRegex(ValueError, "intended fixture repository"):
                native_request_headers(self.cli, env, "http://127.0.0.1:12345/locate", cwd=self.repo)
        opened.assert_not_called()

    def test_warming_diagnostics_do_not_fabricate_repository_pin(self):
        for endpoint in ("/health", "/ready", "/readiness", "/auth/rotation", "/runtime/identity",
                         "/v2/health", "/v2/runtime/identity"):
            with self.subTest(endpoint=endpoint):
                self.commands.clear()
                headers = self.headers(self.service(repository=None), endpoint)
                self.assertIn(IDENTITY_HEADER, headers)
                self.assertNotIn(REPOSITORY_IDENTITY_HEADER, headers)
                self.assertEqual(len(self.commands), 1)

    def test_fixture_peer_metadata_uses_the_selected_actual_store_claim(self):
        self.home.mkdir()
        env = {"KIN_RUNTIME_MODE": "managed", "KIN_HOME": str(self.home)}
        with patch("runtime_fixture._probe", side_effect=self.probe):
            metadata = fixture_daemon_metadata(self.cli, env, cwd=self.repo)
        self.assertEqual(metadata["repository"], {"repository_id": "legacy/名",
                                                "store_namespace": runtime_namespace(self.store)})

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
            correct_runtime = runtime_fingerprint(IDENTITY, "managed", runtime_namespace(self.home))
            for headers in ({}, {IDENTITY_HEADER: "wrong-generation"},
                            {IDENTITY_HEADER: correct_runtime},
                            {IDENTITY_HEADER: correct_runtime, REPOSITORY_IDENTITY_HEADER: "wrong-repository"}):
                request = urllib.request.Request(url, data=body, headers=headers)
                with self.assertRaises(urllib.error.HTTPError) as error:
                    urllib.request.urlopen(request, timeout=5)
                self.assertEqual(error.exception.code, 409)
                error.exception.close()
                self.assertEqual(peer.handler_calls, 0)
            with patch("runtime_fixture._probe", side_effect=self.probe):
                headers = native_request_headers(self.cli, env, url, cwd=self.repo)
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
