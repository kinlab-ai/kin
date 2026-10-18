#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Falsify positive-reference acceptance and preserve failed MCP exchanges."""
import copy
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "freshness", Path(__file__).with_name("working_copy_freshness_repro.py"))
freshness = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(freshness)


def owners(module_id="fixture-module", module="linkgraph.predicates"):
    return [{"entity_id": module_id, "kind": "module", "name": module.rsplit(".", 1)[-1],
             "qualified_name": module, "relationship": "Contains",
             "resolution": "type_resolved"}]


def fixture():
    """The positive answer as served: every entity by id and module, no file anywhere."""
    source_text = freshness.LINKGRAPH_SRC
    module, repo_id, workspace_id = "linkgraph.predicates", "fixture-repository", "fixture-workspace"
    focal_id, caller_id = "fixture-focal", "fixture-caller"
    # The use sits one line below the first line of `dangling_links`, and Kin
    # quotes the constant's name there.
    payload = {
        "focal_entity": {"id": focal_id, "name": freshness.SYMBOL, "kind": "constant",
                         "module": module},
        "references": [{"entity_id": caller_id, "name": freshness.ADMITTED_FUNCTION_NAME,
                        "kind": "Function", "role": "source", "module": module,
                        "semantic_owners": owners(),
                        "resolution": "type_resolved", "relation_kinds": ["references"],
                        "site_count": 1,
                        "sites": [{"line_in_entity": 1, "callee": freshness.SYMBOL}],
                        "sites_absent_reason": None, "sites_partial_reason": None,
                        "via_override_of": None}],
        "total_upstream": 1, "candidates": [], "unconfirmed_candidates": 0,
        "negative": {"kind": "qualified_answer", "interpretation": "qualified_answer",
                     "result_count": 1, "safe_to_conclude_absent": False, "trust": "authoritative",
                     "trust_reason": "structural_authoritative: no degraded signals"},
        "cross_repo": {"authority_anchor": {"entity_id": focal_id, "repo_id": repo_id}},
        "_kin": {"runtime": "repo-daemon", "repository": {"authority_context": "fixture-authority"},
                 "verdict": {"state": "certified", "absence_claim": "not_applicable",
                             "safe_to_conclude_absent": False}},
    }
    def source(entity_id, name, kind, body):
        return {"id": entity_id, "name": name, "kind": kind, "body": body, "module": module,
                "semantic_owners": owners(), "span_coherence": "digest_verified",
                "source_base": {"ticket": "sb2.%s.3.%s" % (entity_id, "3f" * 16)},
                "source_identity": {"entity_id": entity_id, "repository_id": repo_id,
                                    "workspace_id": workspace_id},
                "_kin": {"runtime": "repo-daemon", "freshness": {"state": "recorded", "at": "now", "age_seconds": 0}}}
    return (payload,
            source(focal_id, freshness.SYMBOL, "Constant", source_text.splitlines()[0]),
            source(caller_id, freshness.ADMITTED_FUNCTION_NAME, "Function", freshness.ADMITTED_FUNCTION_BODY),
            module, repo_id)


def call_domain_fixture():
    """The positive answer bounded by an unsettled call-site reading, as served."""
    args = list(fixture())
    payload = args[0]
    codes = "call_sites_owed; call_sites_unresolved"
    payload["negative"]["trust"] = "inconclusive"
    payload["negative"]["advice"] = (
        "this answer returned rows, so it asserts no absence ... Limiting factor: "
        "call_sites_owed: 4 of the 9 callers ...; call_sites_unresolved: 2 of the 3 call sites ...")
    payload["_kin"]["verdict"] = {
        "state": "inconclusive", "absence_claim": "not_applicable", "safe_to_conclude_absent": False,
        "limiting_factor": codes,
        "inputs": {"absence_gate": "inconclusive", "call_sites": "inconclusive",
                   "caller_arrival": "not_applicable", "completeness": "certified",
                   "cross_repo": "certified", "edge_coverage": "certified",
                   "graph_freshness": "not_applicable", "withheld_candidates": "certified"}}
    payload["call_sites"] = {
        "by_state": {"proven_target": 1, "unresolved": 2}, "call_names": [freshness.SYMBOL],
        "callers": 9, "callers_owed_enrichment": 4, "candidate_count": 2,
        "candidates": [
            {"callee": "split", "caller": "parse-key-id", "caller_name": "parse_key",
             "line_in_entity": 1,
             "reason": "focal escapes as a value", "state": "unresolved", "state_reason": "no_answer"},
            {"callee": "append", "caller": "store-id", "caller_name": "store",
             "line_in_entity": 1,
             "reason": "focal escapes as a value", "state": "unresolved", "state_reason": "no_answer"}],
        "clauses": ["call_sites_owed: 4 of the 9 callers in the store's callers that could call "
                    "the focal have call sites the graph has not settled yet",
                    "call_sites_unresolved: 2 of the 3 call sites in the store's callers that "
                    "could call the focal got an answer that proves no target"],
        "focal_escape": {"escape": "unknown", "reason": "dynamic reflective access in the domain"},
        "settled": False, "sites": 3}
    return tuple(args)


def settled_retry_fixture():
    args = list(fixture())
    payload = args[0]
    graph = {"generation": 5, "graph_root": "a1" * 32}
    reason = ("reference_completion_unmeasured: non-call completion is unmeasured; "
              "retrieval_degraded: this query reported degradations [graph_authority:retry], "
              "so it did not run at full capability")
    payload.update(
        relation_kinds=["calls", "imports", "references"],
        call_sites={"settled": True, "clauses": [], "candidates": [], "candidate_count": 0},
        degradations=[{"component": "graph_authority", "reason": "retry", "attempt_number": 1,
                       "detail": "the second attempt was confirmed current before it was returned. "
                                 "Every row in it is settled."}])
    payload["cross_repo"]["authority_roots"] = {
        payload["cross_repo"]["authority_anchor"]["repo_id"]: graph["graph_root"]}
    payload["negative"].update(
        trust="inconclusive", trust_reason=reason, advice=reason, graph_as_of=copy.deepcopy(graph),
        degraded_signals=["graph_authority:retry"], bounding_signals=["graph_authority:retry"])
    payload["_kin"].update(
        graph_as_of=graph,
        completeness={"status": "unknown", "bound": "at_least", "counted": {"exact": False},
                      "limits": ["verdict_inconclusive"]})
    payload["_kin"]["verdict"].update(
        state="inconclusive", limiting_factor="reference_completion_unmeasured; retrieval_degraded",
        inputs={"reference_completion": "inconclusive", "absence_gate": "inconclusive",
                "call_sites": "certified", "degradations": "inconclusive"})
    return tuple(args)


class SettledReferenceRetry(unittest.TestCase):
    def test_settled_retry_still_requires_the_exact_admitted_use(self):
        self.assertEqual(freshness.grade_admitted_reference(*settled_retry_fixture())[0], freshness.PASS)

    def test_two_agreeing_roots_do_not_replace_the_independent_authority_root(self):
        args = list(copy.deepcopy(settled_retry_fixture()))
        args[0]["_kin"]["graph_as_of"]["graph_root"] = "b2" * 32
        args[0]["negative"]["graph_as_of"]["graph_root"] = "b2" * 32
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)
        args = list(copy.deepcopy(settled_retry_fixture()))
        args[0]["_kin"]["graph_as_of"]["generation"] = 1
        args[0]["negative"]["graph_as_of"]["generation"] = True
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)

    def test_retry_does_not_license_other_degradations_or_missing_proofs(self):
        mutations = [
            (0, ["degradations", 0, "reason"], "mutation_in_flight"),
            (0, ["degradations", 0, "reason"], "settled_replay"),
            (0, ["degradations", 0, "reason"], "unknown"),
            (0, ["degradations", 0, "attempt_number"], 0),
            (0, ["degradations", 0, "attempt_number"], True),
            (0, ["degradations", 0, "detail"], "read not current"),
            (0, ["degradations"], []),
            (0, ["negative", "graph_as_of", "graph_root"], "b2" * 32),
            (0, ["_kin", "graph_as_of", "generation"], True),
            (0, ["negative", "graph_as_of", "generation"], True),
            (0, ["cross_repo", "authority_roots"], {}),
            (0, ["cross_repo", "authority_roots", "fixture-repository"], "b2" * 32),
            (0, ["negative", "degraded_signals"], ["graph_authority:retry", "other:gap"]),
            (0, ["_kin", "verdict", "inputs", "source_body"], "inconclusive"),
            (0, ["_kin", "verdict", "safe_to_conclude_absent"], True),
            (0, ["_kin", "completeness", "counted", "exact"], True),
            (0, ["call_sites", "settled"], False),
            (0, ["call_sites", "candidate_count"], 1),
            (0, ["cross_repo", "authority_anchor", "repo_id"], "foreign"),
            (0, ["references", 0, "resolution"], "name_only"),
            (1, ["source_base"], {}),
            (1, ["source_identity", "entity_id"], "stale"),
            (1, ["source_identity", "repository_id"], "foreign"),
            (2, ["body"], "stale caller body"),
            (2, ["source_identity", "workspace_id"], "other-workspace"),
        ]
        for side, path, value in mutations:
            with self.subTest(side=side, path=path, value=value):
                args = list(copy.deepcopy(settled_retry_fixture()))
                target = args[side]
                for key in path[:-1]:
                    target = target[key]
                target[path[-1]] = value
                self.assertNotEqual(freshness.grade_admitted_reference(*args)[0], freshness.PASS)


class PositiveReference(unittest.TestCase):
    def test_exact_graph_sources_prove_the_use_without_claiming_absence(self):
        args = fixture()
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.PASS)
        self.assertEqual(freshness.grade_absence_names_the_gap_it_is_withheld_for(args[0])[0], freshness.FAIL)

    def test_independent_positive_falsifiers(self):
        mutations = [
            (0, ["focal_entity", "id"], "other"),
            (0, ["focal_entity", "name"], "other"),
            (0, ["focal_entity", "module"], "linkgraph.other"),
            (0, ["focal_entity", "module"], None),
            # A module that only ends like the right one is another module.
            (0, ["focal_entity", "module"], "legacy.linkgraph.predicates"),
            # A focal that still carries a bare file path is the retired shape.
            (0, ["focal_entity", "file_path"], "linkgraph/predicates.py"),
            (0, ["cross_repo", "authority_anchor", "repo_id"], "foreign"),
            (0, ["cross_repo", "authority_anchor", "entity_id"], "other"),
            (0, ["references", 0, "entity_id"], "other"),
            (0, ["references", 0, "name"], "other"),
            (0, ["references", 0, "module"], "notekeeper.storage"),
            (0, ["references", 0, "module"], None),
            (0, ["references", 0, "semantic_owners"], []),
            (0, ["references", 0, "semantic_owners"], owners(module="notekeeper.storage")),
            (0, ["references", 0, "semantic_owners"], owners() + owners(module_id="second")),
            # A row that still carries a file line is the retired shape.
            (0, ["references", 0, "file_path"], "linkgraph/predicates.py"),
            (0, ["references", 0, "start_line"], 4),
            (0, ["references", 0, "reference_lines"], [5]),
            (0, ["references", 0, "sites", 0, "line_in_entity"], 0),
            (0, ["references", 0, "sites", 0, "line_in_entity"], 2),
            (0, ["references", 0, "sites", 0, "line_in_entity"], 4),
            (0, ["references", 0, "sites", 0, "line_in_entity"], 1.0),
            (0, ["references", 0, "sites", 0, "line_in_entity"], True),
            (0, ["references", 0, "sites", 0, "line_in_entity"], None),
            (0, ["references", 0, "sites", 0, "callee"], None),
            (0, ["references", 0, "sites", 0, "callee"], "conn.execute"),
            (0, ["references", 0, "sites", 0, "callee"], "RESOLVE"),
            # Names the constant, and is not text the use line holds.
            (0, ["references", 0, "sites", 0, "callee"], "links." + freshness.SYMBOL),
            (0, ["references", 0, "sites", 0, "callee_unavailable"], "caller_source_unavailable"),
            (0, ["references", 0, "sites"], []),
            (0, ["references", 0, "sites"], [{"line_in_entity": 1, "callee": freshness.SYMBOL}] * 2),
            (0, ["references", 0, "sites"], [1]),
            (0, ["references", 0, "site_count"], True),
            (0, ["references", 0, "site_count"], 2),
            (0, ["references", 0, "resolution"], "name_only"),
            (0, ["references", 0, "sites_absent_reason"], "no_evidence_span"),
            (0, ["references", 0, "sites_partial_reason"], "unproven"),
            (0, ["references"], []),
            (0, ["total_upstream"], True),
            (0, ["total_upstream"], "1"),
            (0, ["total_upstream"], 2),
            (0, ["negative", "result_count"], True),
            (0, ["negative", "result_count"], "1"),
            (0, ["negative", "result_count"], 0),
            (0, ["negative", "safe_to_conclude_absent"], True),
            (0, ["negative", "kind"], "absence"),
            (0, ["negative", "trust"], "inconclusive"),
            (0, ["negative"], {}),
            (0, ["_kin", "verdict", "state"], "inconclusive"),
            (0, ["_kin", "verdict", "absence_claim"], "authoritative"),
            (0, ["_kin", "verdict", "safe_to_conclude_absent"], True),
            (0, ["_kin", "runtime"], "embedded"),
            (0, ["_kin", "behind"], {"unadmitted_paths": 1}),
            (0, ["_kin", "self_check"], {"status": "contradicted"}),
            (0, ["candidates"], [{"name": "dangling_links"}]),
        ]
        for side in (1, 2):
            mutations.extend((side, path, value) for path, value in [
                (["id"], "other"), (["name"], "other"), (["kind"], "Variable"),
                (["body"], "unrelated body"), (["span_coherence"], "unproven"),
                (["module"], "linkgraph.other"), (["module"], None),
                (["semantic_owners"], []),
                (["semantic_owners"], owners(module="linkgraph.other")),
                # Owned by another module entity than the one the row names.
                (["semantic_owners"], owners(module_id="other-module")),
                (["source_identity", "entity_id"], "other"),
                (["source_identity", "repository_id"], "foreign"),
                (["source_identity", "workspace_id"], ""),
                # Read in another workspace than the other source.
                (["source_identity", "workspace_id"], "other-workspace"),
                (["source_base", "ticket"], "sb2.other.3." + "3f" * 16),
                (["source_base", "ticket"], "sb2.invalid"),
                (["source_base"], {}),
                (["_kin", "freshness", "state"], "unknown"),
                (["_kin", "runtime"], "embedded"),
                (["_kin", "self_check"], {"status": "contradicted"}),
            ])
        for side, path, value in mutations:
            with self.subTest(side=side, path=path, value=value):
                args = list(copy.deepcopy(fixture())); obj = args[side]
                for key in path[:-1]: obj = obj[key]
                obj[path[-1]] = value
                self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)

    def test_the_repository_is_the_one_the_registry_recorded(self):
        for repo_id in ("another-repository", None, ""):
            with self.subTest(repo_id=repo_id):
                args = list(copy.deepcopy(fixture())); args[4] = repo_id
                self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)

    def test_a_module_the_answer_was_not_asked_about_fails(self):
        args = list(copy.deepcopy(fixture())); args[3] = "notekeeper.storage"
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)

    def test_a_missing_source_proof_is_unreadable(self):
        for side in (1, 2):
            with self.subTest(side=side):
                args = list(copy.deepcopy(fixture())); args[side] = None
                self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.UNREADABLE)

    def test_a_call_domain_bound_on_proven_rows_passes_and_every_weakening_fails(self):
        # The shape 0.8.2 serves while the callers of a just-admitted module
        # still owe enrichment: the rows are proven, and the reading names the
        # call sites it could not settle instead of certifying the whole set.
        args = call_domain_fixture()
        status, detail = freshness.grade_admitted_reference(*args)
        self.assertEqual(status, freshness.PASS, detail)
        self.assertIn("call_sites_owed, call_sites_unresolved", detail)
        mutations = [
            (["negative", "trust"], "authoritative"),
            (["_kin", "verdict", "state"], "certified"),
            (["_kin", "verdict", "inputs", "call_sites"], "certified"),
            (["_kin", "verdict", "inputs", "edge_coverage"], "inconclusive"),
            (["_kin", "verdict", "inputs", "graph_freshness"], "inconclusive"),
            (["_kin", "verdict", "inputs"], None),
            (["_kin", "verdict", "limiting_factor"], None),
            (["_kin", "verdict", "limiting_factor"], "call_sites_owed; graph_behind_working_tree"),
            (["_kin", "verdict", "limiting_factor"], "call_sites_owed"),
            (["negative", "advice"], "treat these rows as a lower bound"),
            (["call_sites"], None),
            (["call_sites", "settled"], True),
            (["call_sites", "clauses"], []),
            (["call_sites", "clauses"], ["call_sites_owed: 4 of the 9 callers are owed"]),
            (["call_sites", "clauses"], ["some callers are owed"]),
            (["call_sites", "candidate_count"], 3),
            (["call_sites", "candidate_count"], True),
            # A negative withheld count cannot balance the ledger: 1 = 2 + -1.
            (["call_sites", "candidate_count"], 1),
            (["call_sites", "candidates_withheld"], -1),
            (["call_sites", "candidates_withheld"], True),
            (["call_sites", "candidates", 0, "line_in_entity"], None),
            (["call_sites", "candidates", 0, "caller"], ""),
            (["call_sites", "candidates", 0, "reason"], None),
            (["call_sites", "candidates", 0, "file_path"], "notekeeper/parsing.py"),
            (["call_sites", "candidates", 0, "start_line"], 4),
            (["_kin", "verdict", "safe_to_conclude_absent"], True),
            (["negative", "safe_to_conclude_absent"], True),
            (["_kin", "verdict", "absence_claim"], "authoritative"),
        ]
        for path, value in mutations:
            with self.subTest(path=path, value=value):
                args = list(call_domain_fixture()); obj = args[0]
                for key in path[:-1]: obj = obj[key]
                if value is None and path[-1] in ("call_sites", "inputs", "limiting_factor"):
                    del obj[path[-1]]
                else:
                    obj[path[-1]] = value
                self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)
        # The two counts together, each individually well typed.
        args = list(call_domain_fixture())
        args[0]["call_sites"].update(candidate_count=1, candidates_withheld=-1)
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)
        args = list(call_domain_fixture())
        args[0]["call_sites"].update(candidate_count=-1)
        args[0]["call_sites"]["candidates"] = []
        args[0]["call_sites"]["candidates_withheld"] = -1
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)
        # A real withheld remainder still balances.
        args = list(call_domain_fixture())
        args[0]["call_sites"].update(candidate_count=5, candidates_withheld=3)
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.PASS)

    def test_kin_refs_must_agree_with_find_references(self):
        payload = call_domain_fixture()[0]
        agreeing = {"lines": ["dangling_links  function  linkgraph/predicates.py"],
                    "call_sites": copy.deepcopy(payload["call_sites"])}
        self.assertEqual(freshness.grade_cli_agrees(payload, 0, json.dumps(agreeing))[0], freshness.PASS)
        for mutate in (
            lambda cli: cli["call_sites"].update(callers=8),
            lambda cli: cli["call_sites"].update(clauses=[]),
            lambda cli: cli["call_sites"].update(settled=True),
            lambda cli: cli.pop("call_sites"),
            lambda cli: cli.update(lines=["no references"]),
            lambda cli: cli.update(error="no entity named RESOLVE_PREDICATE"),
            lambda cli: cli.update(negative={"safe_to_conclude_absent": True}),
        ):
            cli = copy.deepcopy(agreeing); mutate(cli)
            self.assertEqual(freshness.grade_cli_agrees(payload, 0, json.dumps(cli))[0], freshness.FAIL)
        self.assertEqual(freshness.grade_cli_agrees(payload, 1, json.dumps(agreeing))[0], freshness.FAIL)
        self.assertEqual(freshness.grade_cli_agrees(payload, 0, "not json")[0], freshness.UNREADABLE)

    def test_candidates_do_not_substitute_for_proven_rows(self):
        args = list(fixture()); args[0]["candidates"] = args[0]["references"]; args[0]["references"] = []
        self.assertEqual(freshness.grade_admitted_reference(*args)[0], freshness.FAIL)

    def test_bad_positive_cannot_fall_through_to_named_gap(self):
        args = list(fixture()); args[0]["references"][0]["sites"][0]["line_in_entity"] = 0
        args[0]["negative"]["trust_reason"] = "graph_behind_working_tree"
        class Suite:
            def repo(self): return "/fixture/repo"
            def repository_id(self): return args[4]
            def mcp(self, calls):
                return {2: args[0]} if calls[0][0] == "find_references" else {2: args[1], 3: args[2]}
        self.assertEqual(freshness.grade_absence_names_the_gap_it_is_withheld_for(args[0])[0], freshness.PASS)
        self.assertEqual(freshness.check_absence(Suite()).status, freshness.FAIL)

    def test_original_negative_and_committed_controls_remain(self):
        for payload in (freshness.WITHHELD, freshness.WITHHELD_ENRICHMENT):
            self.assertEqual(freshness.grade_absence_names_the_gap_it_is_withheld_for(payload)[0], freshness.PASS)
        for payload in (freshness.CERTIFIED, freshness.WITHHELD_UNEXPLAINED,
                        {"negative": {"safe_to_conclude_absent": False, "trust_reason": "structural_authoritative"}}):
            self.assertEqual(freshness.grade_absence_names_the_gap_it_is_withheld_for(payload)[0], freshness.FAIL)
        self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(freshness.CERTIFIED)[0], freshness.PASS)
        self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(freshness.WITHHELD)[0], freshness.FAIL)


class AdmittedDurability(unittest.TestCase):
    """The admitted control, proved by the graph's own entity in its module."""

    def status(self):
        return {"_kin": {"runtime": "repo-daemon", "answered_by": {"repo_id": "fixture-repository"},
                         "freshness": {"state": "recorded", "at": "now", "age_seconds": 0},
                         "durability": {"state": "live_uncommitted", "live_entities": 10,
                                        "durable_entities": 6, "live_only_entities": 4}}}

    def grade(self, status, source):
        return freshness.grade_durability_after_admission(status, source, "linkgraph.predicates")

    def test_the_served_function_proves_admission(self):
        status, detail = self.grade(self.status(), fixture()[2])
        self.assertEqual(status, freshness.PASS, detail)

    def test_every_weakening_fails(self):
        for mutate in (
                lambda status, source: source.update(module="linkgraph.other"),
                lambda status, source: source.update(semantic_owners=[]),
                lambda status, source: source.update(body="def dangling_links(conn): pass"),
                lambda status, source: source.update(name="resolve_key"),
                lambda status, source: source["source_identity"].update(repository_id="foreign"),
                lambda status, source: source["source_base"].update(ticket="sb2.other.3." + "3f" * 16),
                lambda status, source: status["_kin"].pop("answered_by"),
                lambda status, source: status["_kin"].update(behind={"unadmitted_paths": 1}),
                lambda status, source: status["_kin"]["durability"].update(live_only_entities=0),
                lambda status, source: status["_kin"]["freshness"].update(state="unknown")):
            status, source = self.status(), copy.deepcopy(fixture()[2])
            mutate(status, source)
            self.assertEqual(self.grade(status, source)[0], freshness.FAIL)
        self.assertEqual(self.grade(self.status(), None)[0], freshness.UNREADABLE)


class MockRuntimeFixture(unittest.TestCase):
    def setUp(self):
        # These unit tests use a fictitious executable and mock its wire I/O.
        # Runtime installation has separate real-binary acceptance coverage.
        runtime = patch.object(freshness, "prepare_runtime_fixture",
                               side_effect=lambda kin, daemon, env, home: (kin, daemon))
        runtime.start()
        self.addCleanup(runtime.stop)


class RegistryRepository(MockRuntimeFixture):
    """The fixture's repository id comes from the run's own registry."""

    def test_the_fixture_entry_is_read_and_no_other(self):
        with tempfile.TemporaryDirectory() as root:
            suite = freshness.Suite("/fixture/kin", root)
            suite._repo = os.path.realpath(root)
            with open(suite.env["KIN_REGISTRY_PATH"], "w") as handle:
                handle.write('[[repos]]\nid = "elsewhere"\npath = "/other/repo"\n\n'
                             '[[repos]]\nid = "fixture-repository"\npath = "%s"\n'
                             'entities = 0\n' % root)
            self.assertEqual(suite.repository_id(), "fixture-repository")

    def test_no_registry_or_no_entry_reads_as_none(self):
        with tempfile.TemporaryDirectory() as root:
            suite = freshness.Suite("/fixture/kin", root)
            suite._repo = os.path.realpath(root)
            self.assertIsNone(suite.repository_id())
            with open(suite.env["KIN_REGISTRY_PATH"], "w") as handle:
                handle.write('[[repos]]\nid = "elsewhere"\npath = "/other/repo"\n')
            self.assertIsNone(suite.repository_id())


class CommittedAbsence(unittest.TestCase):
    def test_the_witnessed_negative_and_envelope_contradiction_fails(self):
        # Decisive fields from diagnostic-r1 wire/010-committed.parsed.json,
        # id 3. The negative-only grader accepted this real product defect.
        payload = copy.deepcopy(freshness.CERTIFIED)
        payload["negative"].update(kind="focal_not_resolved", interpretation="name_not_resolved",
                                   result_count=0)
        payload["_kin"].update(
            verdict={"state": "inconclusive", "absence_claim": "not_authoritative",
                     "safe_to_conclude_absent": False, "limiting_factor": "substrate_unknown"},
            self_check={"status": "contradicted", "disagreements": [
                "negative.safe_to_conclude_absent is true while _kin.verdict.safe_to_conclude_absent is false",
                "negative.trust reads authoritative under an inconclusive _kin.verdict"]})
        self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(payload)[0], freshness.FAIL)

    def test_each_public_verdict_must_independently_agree(self):
        self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(
            freshness.CERTIFIED)[0], freshness.PASS)
        mutations = [
            (["negative", "safe_to_conclude_absent"], False),
            (["negative", "safe_to_conclude_absent"], 1),
            (["negative", "trust"], "inconclusive"),
            (["_kin"], None), (["_kin"], {}),
            (["_kin", "verdict"], None), (["_kin", "verdict"], {}),
            (["_kin", "verdict", "state"], "inconclusive"),
            (["_kin", "verdict", "absence_claim"], "not_authoritative"),
            (["_kin", "verdict", "absence_claim"], "not_applicable"),
            (["_kin", "verdict", "safe_to_conclude_absent"], False),
            (["_kin", "verdict", "safe_to_conclude_absent"], 1),
            (["_kin", "self_check"], {"status": "contradicted"}),
        ]
        for path, value in mutations:
            with self.subTest(path=path, value=value):
                payload = copy.deepcopy(freshness.CERTIFIED); obj = payload
                for key in path[:-1]: obj = obj[key]
                obj[path[-1]] = value
                self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(payload)[0], freshness.FAIL)


class WireRetention(MockRuntimeFixture):
    def test_failed_and_malformed_exchanges_stay_retained_and_cannot_grade(self):
        good = {"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text", "text": json.dumps(fixture()[0])}]}}
        cases = [(7, json.dumps(good), "failure"), (0, "not-json", "bad wire"),
                 (0, json.dumps({"jsonrpc": "2.0", "id": 2, "error": {"message": "bad"}}), "rpc"),
                 (0, json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"isError": True, "content": good["result"]["content"]}}), "tool"),
                 (0, json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"text": "broken"}]}}), "content"),
                 (0, json.dumps({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"text": {"bad": "type"}}]}}), "content-type"),
                 (0, json.dumps(good) + "\n" + json.dumps(good), "duplicate"),
                 (0, "", "missing"),
                 (0, json.dumps({"jsonrpc": "2.0", "id": 1, "error": {"message": "init"}}) + "\n" + json.dumps(good), "init")]
        for rc, stdout, stderr in cases:
            with self.subTest(stderr=stderr), tempfile.TemporaryDirectory() as root:
                suite = freshness.Suite("/fixture/kin", root); suite._repo = root
                with patch.object(freshness, "run", return_value=(rc, stdout, stderr)):
                    with self.assertRaises(RuntimeError): suite.mcp([("find_references", {"query": freshness.SYMBOL})])
                directory = Path(suite.evidence_dir) / "001"
                self.assertEqual((directory / "stdout.jsonl").read_text(), stdout)
                self.assertEqual((directory / "stderr.txt").read_text(), stderr)
                self.assertIn(freshness.SYMBOL, (directory / "stdin.jsonl").read_text())
                self.assertTrue(json.loads((directory / "parsed.json").read_text())["errors"])

    def test_spawn_failure_and_valid_exchange_both_have_receipts(self):
        with tempfile.TemporaryDirectory() as root:
            suite = freshness.Suite("/fixture/kin", root); suite._repo = root
            with patch.object(freshness, "run", side_effect=OSError("no binary")):
                with self.assertRaises(RuntimeError): suite.mcp([("find_references", {})])
            first = json.loads((Path(suite.evidence_dir) / "001/parsed.json").read_text())
            self.assertIn("no binary", first["launch_error"])
            answer = {"id": 2, "result": {"content": [{"text": json.dumps(fixture()[0])}]}}
            with patch.object(freshness, "run", return_value=(0, json.dumps(answer), "")):
                self.assertEqual(suite.mcp([("find_references", {})])[2], fixture()[0])
            self.assertTrue((Path(suite.evidence_dir) / "002/parsed.json").exists())

    def test_only_the_requested_qualified_name_miss_can_be_an_error_result(self):
        payload = copy.deepcopy(freshness.CERTIFIED)
        payload["message"] = "Entity not found"
        payload["negative"].update(kind="focal_not_resolved", interpretation="name_not_resolved", result_count=0)
        with tempfile.TemporaryDirectory() as root:
            suite = freshness.Suite("/fixture/kin", root); suite._repo = root
            def reply(body):
                return (0, json.dumps({"id": 2, "result": {"isError": True,
                        "content": [{"text": json.dumps(body)}]}}), "")
            with patch.object(freshness, "run", return_value=reply(payload)):
                parsed = suite.mcp([("find_references", {"query": freshness.ABSENT_SYMBOL})])[2]
                self.assertEqual(freshness.grade_absence_stays_authoritative_over_a_committed_tree(parsed)[0], freshness.PASS)
                with self.assertRaises(RuntimeError): suite.mcp([("get_entity_source", {"entity_id": "x"})])
                with self.assertRaises(RuntimeError): suite.mcp([("find_references", {"query": "other"})])
            for field, value in (("result_count", True), ("kind", "qualified_answer")):
                invalid = copy.deepcopy(payload); invalid["negative"][field] = value
                with patch.object(freshness, "run", return_value=reply(invalid)):
                    with self.assertRaises(RuntimeError): suite.mcp([("find_references", {"query": freshness.ABSENT_SYMBOL})])


if __name__ == "__main__":
    unittest.main()
