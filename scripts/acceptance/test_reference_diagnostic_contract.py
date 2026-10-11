#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Acceptance requests must ask for the diagnostics their assertions read."""
import copy
import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock


def load(name):
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


brownfield = load("brownfield_repro")
hydration = load("hydration_semantics_repro")
magic = load("magic_repro")
limits = load("verdict_limits_repro")
freshness = load("working_copy_freshness_repro")


class DiagnosticRequests(unittest.TestCase):
    def test_brownfield_cache_requests_full_without_changing_explicit_choice(self):
        suite = brownfield.Suite.__new__(brownfield.Suite)
        suite.payloads = {}
        suite.mcp = Mock(return_value={})
        original = {"query": "target"}
        suite.cached("repo", "find_references", original)
        self.assertIs(suite.mcp.call_args.args[2].get("answer_only"), False)
        self.assertEqual(original, {"query": "target"})
        suite.cached("repo", "find_references", {**original, "answer_only": True})
        self.assertIs(suite.mcp.call_args.args[2]["answer_only"], True)
        suite.cached("repo", "semantic_search", original)
        self.assertNotIn("answer_only", suite.mcp.call_args.args[2])

    def test_hydration_asks_for_the_classes_it_waits_for(self):
        suite = hydration.Suite.__new__(hydration.Suite)
        suite.kin_run = Mock()
        payload = {"references": [{"name": "caller"}],
                   "_kin": {"completeness": {"classes": {"calls": "present"}}}}
        suite.mcp = Mock(return_value=payload)
        self.assertIs(suite.find_references(attempts=1), payload)
        self.assertIs(suite.mcp.call_args.args[1].get("answer_only"), False)

    def test_magic_asks_for_arrival_and_negative_diagnostics(self):
        suite = magic.Suite.__new__(magic.Suite)
        suite.mcp = Mock(return_value=({"focal_entity": {"id": "target"}}, {}))
        suite.references("repo", "target", relation_kinds=["calls"])
        self.assertIs(suite.mcp.call_args.args[2].get("answer_only"), False)
        self.assertEqual(suite.mcp.call_args.args[2]["relation_kinds"], ["calls"])

    def test_limits_asks_for_full_even_with_a_response_budget(self):
        suite = limits.Suite.__new__(limits.Suite)
        suite.fixture = Mock(return_value="repo")
        suite.kin_run = Mock()
        suite.payloads = {}
        suite.mcp = Mock(return_value={"_kin": {
            "completeness": {"classes": {"calls": "present"}}}})
        suite.references(kinds=["calls"], attempts=1, extra={"max_chars": 2000})
        args = suite.mcp.call_args.args[2]
        self.assertIs(args.get("answer_only"), False)
        self.assertEqual(args["max_chars"], 2000)

    def test_freshness_asks_for_the_negative_block_it_grades(self):
        suite = Mock()
        suite.mcp.return_value = {2: {"negative": {
            "safe_to_conclude_absent": False,
            "trust_reason": "cross_file_edges_absent"}}}
        freshness.check_absence(suite)
        calls = suite.mcp.call_args.args[0]
        self.assertIs(calls[0][1].get("answer_only"), False)


def compact(state, safe=False, factor=None):
    return {"references": [{"name": "caller"}], "_kin": {
        "shape": "answer_only", "verdict": {
            "state": state, "safe_to_conclude_absent": safe,
            "limiting_factor": factor}}}


class VerdictReaders(unittest.TestCase):
    def test_compact_and_full_read_the_same_authoritative_verdict(self):
        for state, expected, factor in [
            ("certified", "certify", None),
            ("inconclusive", "refuse", "cross_file_edges_absent"),
        ]:
            with self.subTest(state=state):
                small = compact(state, factor=factor)
                full = copy.deepcopy(small)
                del full["_kin"]["shape"]
                full["negative"] = {"trust": "authoritative" if state == "certified"
                                    else "inconclusive", "safe_to_conclude_absent": False}
                for payload in (small, full):
                    surfaces = brownfield.verdict_surfaces(payload)
                    self.assertEqual(surfaces["_kin.verdict"][0], expected)
                    self.assertEqual(brownfield.surface_conflict(surfaces), (None, None))

    def test_contradictory_full_payload_is_still_a_failure(self):
        bad = compact("certified")
        bad["negative"] = {"trust": "inconclusive", "safe_to_conclude_absent": False}
        yes, no = brownfield.surface_conflict(brownfield.verdict_surfaces(bad))
        self.assertIn("_kin.verdict", yes)
        self.assertIn("negative", no)

    def test_malformed_verdict_is_unreadable_never_certified(self):
        for verdict in ({}, {"state": "new_state"}, {"state": "certified"}):
            with self.subTest(verdict=verdict):
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.verdict_surfaces({"_kin": {"verdict": verdict}})

    def test_compact_internal_contradictions_still_fail(self):
        for payload in (compact("certified", factor="cross_file_edges_absent"),
                        compact("certified", factor=""),
                        compact("inconclusive", safe=True, factor="cross_file_edges_absent")):
            with self.subTest(payload=payload):
                yes, no = brownfield.surface_conflict(brownfield.verdict_surfaces(payload))
                self.assertTrue(yes and no)

    def test_compact_cannot_stand_in_for_unreported_diagnostics(self):
        payload = compact("certified")
        self.assertTrue(hydration.verdict_problems(payload, False))
        self.assertIsNone(limits.verdict_honours_classes(payload)[0])
        self.assertIsNone(limits.factor_carries_every_refusing_input(payload)[0])
        self.assertNotEqual(freshness.grade_absence_names_the_gap_it_is_withheld_for(payload)[0],
                            freshness.PASS)

    def test_missing_payload_has_no_readable_verdict(self):
        self.assertEqual(brownfield.verdict_surfaces({}), {})


class DeclarationVerdicts(unittest.TestCase):
    def declaration_only(self):
        payload = compact("certified", safe=True)
        payload["edge_coverage"] = {
            "classes": {"declarations": "complete"}, "scope": "absence_scope",
            "scan": "skipped_no_edge_dependency", "requested_classes": [],
            "cross_unit_classes": []}
        return payload

    def test_a_complete_declaration_filter_does_not_depend_on_an_edge_scan(self):
        payload = self.declaration_only()
        readings = brownfield.verdict_surfaces(payload)
        self.assertEqual(readings["edge_coverage"][0], "certify")
        self.assertEqual(brownfield.surface_conflict(readings), (None, None))

    def test_skipped_scans_with_edge_or_unknown_scope_still_refuse(self):
        for update in ({"requested_classes": ["calls"]},
                       {"requested_classes": None}, {"requested_classes": {}},
                       {"cross_unit_classes": ["references"]},
                       {"cross_unit_classes": None},
                       {"classes": {"references": "present"}},
                       {"scope": "repository"}, {"scope": None},
                       {"scan": "skipped_budget"}):
            with self.subTest(update=update):
                payload = self.declaration_only()
                payload["edge_coverage"].update(update)
                readings = brownfield.verdict_surfaces(payload)
                self.assertEqual(readings["edge_coverage"][0], "refuse")
                yes, no = brownfield.surface_conflict(readings)
                self.assertIn("_kin.verdict", yes)
                self.assertIn("edge_coverage", no)
        for field in ("requested_classes", "cross_unit_classes", "scope"):
            with self.subTest(missing=field):
                payload = self.declaration_only()
                del payload["edge_coverage"][field]
                self.assertEqual(brownfield.verdict_surfaces(payload)["edge_coverage"][0], "refuse")

    def test_declaration_states_are_closed_and_class_specific(self):
        for state, missing in [("complete", []), ("complete_except", ["declarations"]),
                               ("unmeasured", ["declarations"])]:
            with self.subTest(state=state):
                self.assertEqual(brownfield.class_shortfall(
                    {"declarations": state}, "edge_coverage.classes"), missing)
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.class_shortfall({"references": state}, "edge_coverage.classes")

    def test_legacy_states_keep_their_original_meaning(self):
        for name in ("declarations", "calls", "references"):
            for state in ("present", "absent", "unknown", "unproduced"):
                with self.subTest(name=name, state=state):
                    self.assertEqual(brownfield.class_shortfall({name: state}, "classes"),
                                     [] if state == "present" else [name])

    def test_new_or_malformed_states_remain_unreadable(self):
        for state in ("partial", "new_state", "", None, True, 0, [], {}):
            with self.subTest(state=state):
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.class_shortfall({"declarations": state}, "classes")
        for classes in (None, [], {None: "complete"}, {"": "complete"}):
            with self.subTest(classes=classes):
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.class_shortfall(classes, "classes")

    def test_residue_refuses_every_class_surface_and_exposes_false_certification(self):
        for state in ("complete_except", "unmeasured"):
            for surface in ("edge_coverage", "completeness"):
                with self.subTest(state=state, surface=surface):
                    payload = compact("inconclusive", factor="declaration_census_shortfall")
                    classes = {"declarations": state}
                    if surface == "completeness":
                        payload["_kin"][surface] = {"status": "unknown", "bound": "at_least",
                                                  "classes": classes}
                        key = "completeness.classes"
                    else:
                        payload[surface] = {"classes": classes}
                        key = surface
                    readings = brownfield.verdict_surfaces(payload)
                    self.assertEqual(readings[key][0], "refuse")
                    self.assertEqual(brownfield.surface_conflict(readings), (None, None))
                    payload["_kin"]["verdict"] = {"state": "certified",
                        "safe_to_conclude_absent": True, "limiting_factor": None}
                    yes, no = brownfield.surface_conflict(brownfield.verdict_surfaces(payload))
                    self.assertIn("_kin.verdict", yes)
                    self.assertIn(key, no)

    def test_recorded_declaration_residue_stays_a_refusal(self):
        fixture = Path(__file__).parents[2] / "crates/kin-mcp/src/testdata/plain_flask_locate_name_miss.json"
        payload = json.loads(fixture.read_text())
        self.assertEqual(payload["edge_coverage"]["classes"], {"declarations": "complete_except"})
        self.assertEqual(brownfield.class_shortfall(payload["edge_coverage"]["classes"], "classes"),
                         ["declarations"])
        # The recorded census and qualified verdict agree about concrete residue.
        self.assertTrue(payload["edge_coverage"]["declarations"]["matching_unadmitted"])
        self.assertEqual(payload["_kin"]["verdict"]["state"], "certified_except")
        self.assertIs(payload["_kin"]["verdict"]["safe_to_conclude_absent"], False)
        readings = brownfield.verdict_surfaces(payload)
        self.assertTrue(readings)
        self.assertTrue(all(value[0] == "refuse" for value in readings.values()))
        self.assertEqual(brownfield.surface_conflict(readings), (None, None))


class QualifiedVerdicts(unittest.TestCase):
    def test_a_named_remainder_does_not_certify_full_completeness(self):
        payload = compact("certified_except", factor="named_leftovers")
        readings = brownfield.verdict_surfaces(payload)
        self.assertEqual(readings["_kin.verdict"][0], "refuse")
        self.assertEqual(readings["_kin.verdict.limiting_factor"][0], "refuse")
        self.assertEqual(brownfield.surface_conflict(readings), (None, None))

    def test_a_qualified_verdict_requires_a_named_limiting_factor(self):
        for factor in (None, "", " ", "\t\n"):
            with self.subTest(factor=factor):
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.verdict_surfaces(compact("certified_except", factor=factor))

    def test_a_qualified_verdict_cannot_certify_absence(self):
        readings = brownfield.verdict_surfaces(
            compact("certified_except", safe=True, factor="named_leftovers"))
        yes, no = brownfield.surface_conflict(readings)
        self.assertIn("_kin.verdict.absence", yes)
        self.assertIn("_kin.verdict", no)

    def test_qualified_rows_cannot_claim_whole_counts(self):
        for completeness in ({"status": "complete"}, {"bound": "exact"}):
            with self.subTest(completeness=completeness):
                payload = compact("certified_except", factor="named_leftovers")
                payload["_kin"]["completeness"] = completeness
                yes, no = brownfield.surface_conflict(brownfield.verdict_surfaces(payload))
                self.assertIn("completeness", yes)
                self.assertIn("_kin.verdict", no)

    def test_new_verdict_words_stay_unreadable(self):
        for state in ("complete_except", "certified_partial", "qualified"):
            with self.subTest(state=state):
                with self.assertRaises(brownfield.UnknownClassState):
                    brownfield.verdict_surfaces(compact(state, factor="named_leftovers"))


class AdmittedDurability(unittest.TestCase):
    def evidence(self):
        """The status and the function source as served: by entity and module, no file."""
        status = {"_kin": {"runtime": "repo-daemon", "answered_by": {"repo_id": "repo"},
                  "freshness": {"state": "recorded", "at": "observed", "age_seconds": 0},
                  "durability": {"state": "live_uncommitted", "live_entities": 10,
                                 "durable_entities": 6, "live_only_entities": 4}}}
        source = {"_kin": copy.deepcopy(status["_kin"]), "id": "function", "kind": "Function",
                  "name": freshness.ADMITTED_FUNCTION_NAME, "module": freshness.ADMITTED_MODULE,
                  "semantic_owners": [{"entity_id": "module", "kind": "module",
                                       "name": "predicates",
                                       "qualified_name": freshness.ADMITTED_MODULE,
                                       "relationship": "Contains",
                                       "resolution": "type_resolved"}],
                  "body": freshness.ADMITTED_FUNCTION_BODY, "span_coherence": "digest_verified",
                  "source_base": {"ticket": "sb2.function.3." + "3f" * 16},
                  "source_identity": {"entity_id": "function", "repository_id": "repo",
                                      "workspace_id": "workspace"}}
        return status, source

    def grade(self, status, source):
        return freshness.grade_durability_after_admission(
            status, source, freshness.ADMITTED_MODULE)[0]

    def test_exact_admitted_graph_body_explains_missing_behind(self):
        status, source = self.evidence()
        self.assertEqual(freshness.grade_durability_withholds_the_all_clear(status)[0], freshness.FAIL)
        self.assertEqual(self.grade(status, source), freshness.PASS)

    def test_missing_source_or_unbound_source_never_passes(self):
        for key, value in [("body", "stale body"), ("module", "linkgraph.other"),
                           ("kind", "Module"), ("name", "another_function"),
                           ("span_coherence", "unverified"), ("source_base", {}),
                           ("semantic_owners", []), ("source_identity", {}), ("id", "")]:
            with self.subTest(key=key):
                status, source = self.evidence()
                source[key] = value
                self.assertNotEqual(self.grade(status, source), freshness.PASS)
        status, _ = self.evidence()
        self.assertNotEqual(self.grade(status, None), freshness.PASS)

    def test_bad_counts_or_cross_repository_evidence_still_fail(self):
        for change in (lambda p: p["_kin"]["durability"].update(state="recorded"),
                       lambda p: p["_kin"]["durability"].update(live_only_entities=0),
                       lambda p: p["_kin"]["durability"].update(live_only_entities=3),
                       lambda p: p["_kin"].update(behind={}),
                       lambda p: p["_kin"]["answered_by"].update(repo_id="other"),
                       lambda p: p["_kin"].update(freshness={})):
            with self.subTest(change=change):
                status, source = self.evidence()
                change(status)
                self.assertEqual(self.grade(status, source), freshness.FAIL)

    def test_a_ticket_for_another_read_does_not_prove_admission(self):
        for ticket in ("sb2.other.3." + "3f" * 16, "sb2.function.3.short", "sb2.invalid", None):
            with self.subTest(ticket=ticket):
                status, source = self.evidence()
                source["source_base"]["ticket"] = ticket
                self.assertEqual(self.grade(status, source), freshness.FAIL)

    def test_module_placement_and_module_ownership_are_independent(self):
        status, source = self.evidence()
        source["module"] = "linkgraph.other"
        self.assertEqual(self.grade(status, source), freshness.FAIL)
        status, source = self.evidence()
        source["semantic_owners"][0]["qualified_name"] = "linkgraph.other"
        self.assertEqual(self.grade(status, source), freshness.FAIL)

    def test_whole_module_cannot_substitute_for_targeted_function(self):
        status, source = self.evidence()
        source.update(kind="Module", name="predicates", body=freshness.LINKGRAPH_SRC)
        self.assertEqual(self.grade(status, source), freshness.FAIL)

    def test_original_false_all_clear_does_not_retry_until_it_passes(self):
        suite = Mock()
        suite.ground_truth.return_value = 2
        suite.mcp.return_value = {2: freshness.SHIPPED_0_6_1}
        self.assertEqual(freshness.check_durability(suite).status, freshness.FAIL)
        self.assertEqual(suite.mcp.call_count, 1)

    def test_actual_route_verifies_both_initial_and_final_status(self):
        status, source = self.evidence()
        suite = Mock()
        suite.unadmitted_path = "linkgraph/predicates.py"
        suite.ground_truth.return_value = 2
        for final, expected in [(status, freshness.PASS),
                                (freshness.SHIPPED_0_6_1, freshness.FAIL)]:
            suite.mcp.side_effect = [{2: status},
                                    {2: {"results": [{"id": "module", "kind": "module"},
                                                     {"id": "function", "kind": "function",
                                                      "name": freshness.ADMITTED_FUNCTION_NAME}]}},
                                    {2: source, 3: final}]
            self.assertEqual(freshness.check_durability(suite).status, expected)
            self.assertEqual(suite.mcp.call_args.args[0][0],
                             ("get_entity_source", {"entity_id": "function"}))


if __name__ == "__main__":
    unittest.main()
