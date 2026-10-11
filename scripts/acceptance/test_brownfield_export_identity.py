#!/usr/bin/env python3
"""The export grader selects identity, never a same-named module's references."""
import copy
import unittest

import brownfield_repro as repro


def body(identity, kind, path, references):
    return {"focal_entity": {"id": identity, "name": "Router", "kind": kind,
                             "projection": {"path": path}},
            "references": references, "total_upstream": len(references)}


def sectioned(*bodies):
    for item in bodies:
        item["entity_id"] = item["focal_entity"]["id"]
        item["owner_qualified_name"] = item["focal_entity"]["name"]
    return {"ambiguous_focal": True, "candidate_count": len(bodies),
            "candidates_by_owner": list(bodies)}


class Fixture:
    def __init__(self, payload):
        self.payload = payload

    def fixture(self, name):
        return "express"

    def sweep_gate(self, name):
        pass

    def cached(self, repo, tool, arguments):
        return self.payload


class ExportIdentityTests(unittest.TestCase):
    def test_sectioned_export_is_graded_by_its_own_references(self):
        export = body("export", "constant", "lib/express.js", [{"entity_id": "caller"}])
        module = body("module", "module", "test/Router.js", [])
        result = repro.check_7(Fixture(sectioned(module, export)))
        self.assertEqual(result.status, repro.PASS)

    def test_module_references_cannot_rescue_an_unused_export(self):
        export = body("export", "constant", "lib/express.js", [])
        module = body("module", "module", "test/Router.js", [{"entity_id": "caller"}])
        result = repro.check_7(Fixture(sectioned(module, export)))
        self.assertEqual(result.status, repro.FAIL)

    def test_wrong_identity_or_duplicate_export_is_unreadable(self):
        export = body("export", "constant", "lib/express.js", [{"entity_id": "caller"}])
        module = body("module", "module", "test/Router.js", [{"entity_id": "caller"}])
        duplicate = copy.deepcopy(export)
        duplicate["focal_entity"]["id"] = "duplicate"
        for payload in [module, sectioned(export, duplicate)]:
            with self.subTest(payload=payload):
                self.assertEqual(repro.check_7(Fixture(payload)).status, repro.UNREADABLE)

    def test_false_absence_claim_still_fails(self):
        export = body("export", "constant", "lib/express.js", [{"entity_id": "caller"}])
        export["negative"] = {"safe_to_conclude_absent": True}
        self.assertEqual(repro.check_7(Fixture(export)).status, repro.FAIL)


if __name__ == "__main__":
    unittest.main()
