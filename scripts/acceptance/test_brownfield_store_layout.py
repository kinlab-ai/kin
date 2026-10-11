#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Fixture ownership and evidence follow the store Kin actually discovered."""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import brownfield_repro as brownfield


class StoreLayoutTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.work = self.root / "work"
        self.repo = self.work / "requests"
        self.repo.mkdir(parents=True)
        self.suite = brownfield.Suite("kin", str(self.work), str(self.root / "cache"))
        self.store = Path(self.suite.kin_home) / "stores" / "fixture"
        self.store.mkdir(parents=True)
        (self.store / "manifest.json").write_text("{}")
        self.answer = {"schema": "kin.store-path.v1", "working_dir": str(self.repo),
                       "store": str(self.store), "external": True}
        self.suite.kin_run = Mock(side_effect=self.reply)

    def reply(self, args, repo, **kwargs):
        self.assertEqual(args, ["store", "path", "--json"])
        self.assertEqual(repo, str(self.repo))
        self.assertEqual(kwargs, {"timeout": 30})
        return 0, json.dumps(self.answer), ""

    def discover(self):
        return self.suite.store_path(str(self.repo))

    def test_central_store_is_resolved_without_a_tree_marker(self):
        self.suite.store_layout = "central"
        self.assertFalse((self.repo / ".kin").exists())
        self.assertEqual(self.discover(), str(self.store))
        self.assertEqual(self.suite.stores[str(self.repo)]["external"], True)

    def test_existing_in_tree_store_uses_the_same_discovery_contract(self):
        store = self.repo / ".kin"
        store.mkdir()
        (store / "manifest.json").write_text("{}")
        self.answer.update(store=str(store), external=False)
        self.suite.store_layout = "in-tree"
        self.assertEqual(self.discover(), str(store))

    def test_unknown_parent_or_foreign_binding_never_authorizes_cleanup(self):
        original = dict(self.answer)
        cases = [[], {}, {"schema": "wrong"}, {"working_dir": str(self.work)},
                 {"store": "relative"}, {"store": str(self.root / "work-other")},
                 {"store": None}, {"external": "true"}, {"external": False}]
        for change in cases:
            with self.subTest(change=change):
                self.answer = change if isinstance(change, list) or not change else {**original, **change}
                with self.assertRaises(brownfield.ProbeError):
                    self.discover()
        self.answer = original
        self.suite.kin_run = Mock(return_value=(1, "", "unavailable"))
        with self.assertRaises(brownfield.ProbeError):
            self.discover()

    def test_foreign_store_symlink_and_manifest_symlink_are_refused(self):
        foreign = self.root / "outside"
        foreign.mkdir()
        (foreign / "manifest.json").write_text("{}")
        alias = self.work / "alias"
        alias.symlink_to(foreign, target_is_directory=True)
        self.answer["store"] = str(alias)
        with self.assertRaises(brownfield.ProbeError):
            self.discover()
        self.answer["store"] = str(self.store)
        (self.store / "manifest.json").unlink()
        (self.store / "manifest.json").symlink_to(foreign / "manifest.json")
        with self.assertRaises(brownfield.ProbeError):
            self.discover()

    def test_explicit_layout_is_checked_instead_of_only_exported(self):
        self.suite.store_layout = "in-tree"
        with self.assertRaises(brownfield.ProbeError):
            self.discover()

    def test_inherited_store_and_registry_cannot_redirect_fixtures(self):
        with patch.dict(os.environ, {"KIN_STORE": "/foreign", "KIN_STORE_LAYOUT": "in-tree",
                                     "KIN_REGISTRY_PATH": "/foreign/registry.toml",
                                     "KIN_ALLOW_PARENT_STORE": "1"}):
            suite = brownfield.Suite("kin", str(self.work), "cache")
        self.assertNotIn("KIN_STORE", suite.env)
        self.assertNotIn("KIN_STORE_LAYOUT", suite.env)
        self.assertNotIn("KIN_ALLOW_PARENT_STORE", suite.env)
        self.assertEqual(suite.env["KIN_REGISTRY_PATH"],
                         os.path.join(suite.kin_home, "registry.toml"))

    def test_diagnostics_read_the_central_log_only(self):
        (self.store / "daemon.log").write_text("LSP query failed at the central store\n")
        self.suite.fixtures["requests"] = str(self.repo)
        report = self.root / "brownfield.json"
        result = brownfield.retain_failure_diagnostics(self.suite, str(report), 2)
        artifact = report.parent / result["directory"] / "requests-daemon.txt"
        self.assertIn("at the central store", artifact.read_text())


if __name__ == "__main__":
    unittest.main()
