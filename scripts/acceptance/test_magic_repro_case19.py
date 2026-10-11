#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Grade how magic-repro reads reference rows that address sites inside callers.

A `find_references` row names its caller by entity id, places it by `module`,
the dotted module that contains it, and addresses each site as
`line_in_entity`, counted from 0 at the caller's first line, with the `callee`
text Kin cut from the caller's body there. It carries no line of a file, and
neither does `get_entity_source`.

Case 19 still has to grade every site against the source on disk, because that
is the only thing that tells a re-anchored site from a remembered one. It places
a site in the source this suite wrote through the caller's body from
`get_entity_source`: where that exact body sits is where the caller starts, and
the site is its `line_in_entity` below that. These cases hold that placement,
the rule that a body that sits nowhere in the written source is a remembered
caller, and the rule that a site Kin cannot place inside its caller is the
stale site itself rather than something to skip. The checks that ask where a
caller or focal lives read `module`, whether a caller is a test reads `role`,
and the last cases hold those readings.

No repo, no daemon: every input is a literal payload.
"""
import ast
import copy
import importlib.util
from pathlib import Path
import unittest


def _load(name):
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).with_name(name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


m = _load("magic_repro")

# The fixture file exactly as case 19 reads it back: the module docstring the
# comment-only commit prepended, then the original source.
LINES = (m.MIXIN_DOCSTRING + m.MIXIN_SESSIONS_PY).splitlines()
MODULE = m.MIXIN_CALLER_MODULE


def first_line(text, after=0):
    """The 0-based index of the first line at or after `after` that starts with `text`."""
    return next(index for index in range(after, len(LINES))
                if LINES[index].strip().startswith(text))


RESOLVE = first_line("def resolve_redirects")
REQUEST = first_line("def request")
# Session.send is the second `def send` in the file; the first is the mixin's.
SEND = first_line("def send", first_line("class Session("))


def offset(start, text):
    """A site's `line_in_entity`: its line counted from 0 at the caller's first line."""
    return first_line(text, start) - start


def row(name, entity_id, start, call, callee, module=MODULE):
    return {"entity_id": entity_id, "name": name, "kind": "Method", "role": "source",
            "module": module, "resolution": "type_resolved",
            "relation_kinds": ["calls"], "site_count": 1,
            "sites": [{"line_in_entity": offset(start, call), "callee": callee}],
            "sites_absent_reason": None, "sites_partial_reason": None}


def evidence():
    return {
        "error": None,
        "source_lines": LINES,
        "after_send": {"focal_entity": {"id": "session-send"}, "references": [
            row("SessionRedirectMixin.resolve_redirects", "resolve", RESOLVE,
                "resp = self.send(", "self.send"),
            row("Session.request", "request", REQUEST, "return self.send(", "self.send")]},
        "after_adapter": {"focal_entity": {"id": "adapter-send"}, "references": [
            row("Session.send", "send", SEND, "return adapter.send(", "adapter.send")]},
        "after_caller_bodies": {"resolve": entity_source(RESOLVE),
                                "request": entity_source(REQUEST),
                                "send": entity_source(SEND)},
    }


def body_text(start, shift=0):
    """The caller's body as Kin serves it: from its keyword, with no line in a file."""
    node = next(node for node in ast.walk(ast.parse("\n".join(LINES)))
                if isinstance(node, ast.FunctionDef) and node.lineno == start + 1)
    lines = LINES[start - shift:node.end_lineno - shift]
    return "\n".join([lines[0].lstrip()] + lines[1:])


def entity_source(start):
    return {"body": body_text(start)}


class Suite:
    def __init__(self, recorded):
        self.recorded = recorded

    def comment_only_commit_evidence(self):
        return self.recorded


def grade(recorded):
    return m.check_19(Suite(recorded))


def rows(recorded, key):
    return recorded[key]["references"]


class Check19(unittest.TestCase):
    """Every reported site, placed in the edited file, must still carry the call."""

    def assert_status(self, recorded, expected):
        result = grade(recorded)
        self.assertEqual(result.status, expected, result.asserts)
        return result

    def test_the_fixture_places_each_call_where_the_file_has_it(self):
        # Guards the fixture itself: the offsets above must name the real calls.
        for start, call in ((RESOLVE, "resp = self.send("), (REQUEST, "return self.send("),
                            (SEND, "return adapter.send(")):
            self.assertGreater(offset(start, call), 0)
            self.assertIn(call, LINES[start + offset(start, call)])

    def test_sites_that_carry_their_calls_pass(self):
        result = self.assert_status(evidence(), m.PASS)
        self.assertIn("all 3 reported reference site(s)", result.detail)

    def test_a_site_remembered_from_before_the_commit_fails(self):
        # The defect this check exists for: the caller moved down 26 lines with
        # its file and one of its sites did not, so Kin can place that site
        # inside no caller.
        recorded = evidence()
        rows(recorded, "after_adapter")[0]["sites"].append(
            {"line_in_entity": None, "callee": None, "callee_unavailable": "site_outside_caller"})
        rows(recorded, "after_adapter")[0]["site_count"] = 2
        self.assert_status(recorded, m.FAIL)

    def test_a_site_one_line_off_fails(self):
        for key, index in (("after_send", 0), ("after_send", 1), ("after_adapter", 0)):
            for delta in (-1, 1):
                with self.subTest(key=key, index=index, delta=delta):
                    recorded = evidence()
                    rows(recorded, key)[index]["sites"][0]["line_in_entity"] += delta
                    self.assert_status(recorded, m.FAIL)

    def test_a_caller_body_cut_at_its_old_place_fails(self):
        # A caller the graph never re-anchored is cut from where it sat before
        # the 26-line docstring, so its body is other text from the module and
        # even a correct offset lands on a line that does not carry the call.
        recorded = evidence()
        recorded["after_caller_bodies"]["send"]["body"] = body_text(SEND, shift=26)
        self.assert_status(recorded, m.FAIL)

    def test_a_remembered_body_that_is_not_in_the_edited_source_fails(self):
        # The body still carries the call at the reported offset, so only its
        # absence from the written source can show it is a remembered caller.
        recorded = evidence()
        body = recorded["after_caller_bodies"]["send"]["body"]
        recorded["after_caller_bodies"]["send"]["body"] = body + "\n        # remembered"
        result = self.assert_status(recorded, m.FAIL)
        self.assertIn("not in the edited source", result.detail)

    def test_a_body_that_sits_in_two_places_is_unreadable_not_a_pass(self):
        recorded = evidence()
        end = REQUEST + len(recorded["after_caller_bodies"]["request"]["body"].splitlines())
        recorded["source_lines"] = LINES + [""] + LINES[REQUEST:end]
        self.assert_status(recorded, m.UNREADABLE)

    def test_the_fixture_bodies_place_where_the_source_has_them(self):
        for start in (RESOLVE, REQUEST, SEND):
            self.assertEqual(m.locate_body(LINES, body_text(start)), (start + 1, None))
        self.assertEqual(m.locate_body(LINES, ""), (None, "absent"))
        self.assertEqual(m.locate_body(LINES, "def nowhere():\n    pass"), (None, "absent"))

    def test_the_right_line_quoting_another_call_fails(self):
        recorded = evidence()
        rows(recorded, "after_send")[0]["sites"][0]["callee"] = "self.get_redirect_target"
        self.assert_status(recorded, m.FAIL)

    def test_text_kin_says_lies_outside_the_caller_fails(self):
        recorded = evidence()
        site = rows(recorded, "after_adapter")[0]["sites"][0]
        site.update(callee=None, callee_unavailable="site_outside_caller")
        self.assert_status(recorded, m.FAIL)

    def test_a_quote_that_is_only_unavailable_is_not_staleness(self):
        recorded = evidence()
        site = rows(recorded, "after_adapter")[0]["sites"][0]
        site.update(callee=None, callee_unavailable="caller_source_unavailable")
        self.assert_status(recorded, m.PASS)

    def test_a_bare_name_quote_names_the_method(self):
        recorded = evidence()
        rows(recorded, "after_adapter")[0]["sites"][0]["callee"] = "send"
        self.assert_status(recorded, m.PASS)

    def test_a_caller_that_cannot_be_placed_is_unreadable_not_a_pass(self):
        recorded = evidence()
        recorded["after_caller_bodies"]["request"] = {"error": "get_entity unreadable"}
        self.assert_status(recorded, m.UNREADABLE)
        recorded = evidence()
        del recorded["after_caller_bodies"]["send"]
        self.assert_status(recorded, m.UNREADABLE)

    def test_a_correct_file_line_cannot_substitute_for_missing_entity_source(self):
        for body in (None, 17):
            with self.subTest(body=body):
                recorded = evidence()
                recorded["after_caller_bodies"]["send"]["body"] = body
                self.assert_status(recorded, m.UNREADABLE)

    def test_a_correct_file_line_cannot_substitute_for_a_wrong_entity_body(self):
        for body in ("", "def send():\n    return unrelated()"):
            with self.subTest(body=body):
                recorded = evidence()
                recorded["after_caller_bodies"]["send"]["body"] = body
                self.assert_status(recorded, m.FAIL)

    def test_a_stale_site_outranks_an_unplaced_caller(self):
        recorded = evidence()
        recorded["after_caller_bodies"]["request"] = {"error": "get_entity unreadable"}
        rows(recorded, "after_adapter")[0]["sites"][0]["line_in_entity"] = None
        self.assert_status(recorded, m.FAIL)

    def test_malformed_sites_are_unreadable(self):
        recorded = evidence()
        rows(recorded, "after_adapter")[0]["sites"] = "+3"
        self.assert_status(recorded, m.UNREADABLE)

    def test_rows_with_no_site_are_not_failed(self):
        # An absent site is a different answer from a wrong one.
        recorded = evidence()
        row = rows(recorded, "after_send")[1]
        row.update(sites=[], site_count=0, sites_absent_reason="no_evidence_span")
        self.assert_status(recorded, m.PASS)

    def test_nothing_graded_is_unreadable(self):
        recorded = evidence()
        for key in ("after_send", "after_adapter"):
            for each in rows(recorded, key):
                each.update(sites=[], site_count=0, sites_absent_reason="no_evidence_span")
        self.assert_status(recorded, m.UNREADABLE)

    def test_only_callers_placed_in_the_edited_module_are_graded(self):
        recorded = evidence()
        for key in ("after_send", "after_adapter"):
            for each in rows(recorded, key):
                each["module"] = "pkg.adapters"
                each["sites"][0]["line_in_entity"] = None
        self.assert_status(recorded, m.UNREADABLE)

    def test_a_row_placed_only_by_a_projected_path_is_not_read_as_placed(self):
        recorded = evidence()
        for key in ("after_send", "after_adapter"):
            for each in rows(recorded, key):
                del each["module"]
                each["projection"] = {"path": "pkg/sessions.py"}
        self.assert_status(recorded, m.UNREADABLE)


class CallerBodies(unittest.TestCase):
    """The caller bodies case 19 places sites by, read once per caller."""

    class Mcp:
        def __init__(self, records):
            self.records = records
            self.calls = []

        def mcp(self, repo, tool, args):
            self.calls.append((repo, tool, args["entity_id"]))
            record = self.records[args["entity_id"]]
            if isinstance(record, Exception):
                raise record
            return record, 0

    def test_each_caller_is_read_once_through_get_entity_source(self):
        records = {"a": {"body": "def a(): pass"}, "b": {"body": "def b(): pass"}}
        fake = self.Mcp(records)
        payloads = ({"references": [{"entity_id": "a"}, {"entity_id": "b"}]},
                    {"references": [{"entity_id": "a"}, {"name": "federated"}, None]})
        bodies = m.Suite.caller_bodies(fake, "repo", payloads)
        self.assertEqual(bodies, records)
        self.assertEqual(fake.calls, [("repo", "get_entity_source", "a"),
                                      ("repo", "get_entity_source", "b")])

    def test_an_unreadable_record_is_named_rather_than_guessed(self):
        fake = self.Mcp({"refused": m.McpError("mcp get_entity_source isError"),
                         "bodyless": {"id": "bodyless"},
                         "numeric": {"body": 17},
                         "null": {"body": None}})
        payloads = ({"references": [{"entity_id": key} for key in fake.records]},)
        bodies = m.Suite.caller_bodies(fake, "repo", payloads)
        for key in fake.records:
            self.assertNotIn("body", bodies[key], key)
            self.assertTrue(bodies[key]["error"], key)

    def test_a_record_is_kept_without_any_line_in_a_file(self):
        # Kin serves no start line, and the reading must not need one.
        fake = self.Mcp({"a": {"id": "a", "body": "def a(): pass", "module": "pkg.a"}})
        bodies = m.Suite.caller_bodies(fake, "repo", ({"references": [{"entity_id": "a"}]},))
        self.assertEqual(bodies, {"a": {"body": "def a(): pass"}})

    def test_whole_module_callers_do_not_request_source(self):
        fake = self.Mcp({})
        payloads = ({"references": [{"entity_id": "m", "kind": "Module"},
                                     {"entity_id": "f", "kind": "File"}]},)
        bodies = m.Suite.caller_bodies(fake, "repo", payloads)
        self.assertEqual(fake.calls, [])
        self.assertTrue(all("error" in source for source in bodies.values()))


class RowModule(unittest.TestCase):
    """Where a caller or focal is placed, read from its `module` only."""

    def test_the_module_is_read(self):
        self.assertEqual(m.row_module({"module": "pkg.cli"}), "pkg.cli")

    def test_anything_else_reads_as_unplaced(self):
        for value in ({}, {"projection": {"path": "pkg/cli.py"}}, {"file_path": "pkg/cli.py"},
                      {"module": None}, {"module": ""}, {"module": 3}, None, []):
            with self.subTest(row=value):
                self.assertIsNone(m.row_module(value))


class IsTestCaller(unittest.TestCase):
    """Whether a caller is test code, read from the role the graph gives it."""

    def test_roles_are_read(self):
        self.assertIs(m.is_test_caller({"role": "test"}), True)
        for role in ("source", "vendored", "generated"):
            self.assertIs(m.is_test_caller({"role": role}), False)

    def test_a_row_with_no_role_is_unclassified(self):
        for value in ({}, {"role": None}, {"role": ""},
                      {"module": "tests.test_client"}, None, []):
            with self.subTest(row=value):
                self.assertIsNone(m.is_test_caller(value))


class Check5(unittest.TestCase):
    """No test caller may be claimed for Adapter.send, read from each row's role."""

    def setUp(self):
        self.payload = {"focal_entity": {"id": "adapter-send"}, "references": [
            {"entity_id": "s", "name": "Session.send", "role": "source",
             "module": "app.session"}]}

    def fixture(self, name):
        return "converted"

    def inspect(self, repo, name):
        return {"raw": "", "relations": []}

    def references(self, repo, query):
        return copy.deepcopy(self.payload)

    def status(self):
        return m.check_5(self).status

    def test_production_callers_pass(self):
        self.assertEqual(self.status(), m.PASS)

    def test_a_test_caller_fails(self):
        self.payload["references"].append(
            {"entity_id": "t", "name": "test_session_send", "role": "test",
             "module": "tests.test_session"})
        self.assertEqual(self.status(), m.FAIL)

    def test_a_test_caller_fails_wherever_it_is_placed(self):
        # The role is the graph's classification; the module does not overrule it.
        self.payload["references"].append(
            {"entity_id": "t", "name": "test_session_send", "role": "test",
             "module": "app.session"})
        self.assertEqual(self.status(), m.FAIL)

    def test_a_caller_with_no_role_is_unreadable_not_a_pass(self):
        self.payload["references"].append(
            {"entity_id": "t", "name": "test_session_send",
             "projection": {"path": "tests/test_session.py"}})
        self.assertEqual(self.status(), m.UNREADABLE)


class Check1(unittest.TestCase):
    """parse_note's callers must reach the storage module, read from each row's module."""

    def setUp(self):
        self.payload = {"focal_entity": {"id": "parse-note", "module": "pkg.parsing"},
                        "references": [
                            {"entity_id": "i", "name": "Database.ingest_dir",
                             "module": m.INCREMENTAL_STORAGE_MODULE}]}

    def fixture(self, name):
        return "incremental"

    def graph_status(self, repo):
        return {"relation_kinds": {"Imports": 1}, "cross_file": (3, 9)}

    def references(self, repo, query):
        return copy.deepcopy(self.payload)

    def status(self):
        return m.check_1(self).status

    def test_a_caller_in_the_storage_module_passes(self):
        self.assertEqual(self.status(), m.PASS)

    def test_callers_only_in_the_declaring_module_fail(self):
        self.payload["references"] = [
            {"entity_id": "e", "name": "extract_links", "module": "pkg.parsing"}]
        self.assertEqual(self.status(), m.FAIL)

    def test_a_caller_placed_only_by_a_projected_path_fails(self):
        self.payload["references"] = [
            {"entity_id": "i", "name": "Database.ingest_dir",
             "projection": {"path": "pkg/storage.py"}}]
        self.assertEqual(self.status(), m.FAIL)

    def test_a_module_that_only_ends_like_storage_fails(self):
        self.payload["references"][0]["module"] = "pkg.legacy_storage"
        self.assertEqual(self.status(), m.FAIL)


class Check3(unittest.TestCase):
    """normalize_title's three calling modules, read from each row's module."""

    def setUp(self):
        self.payload = {"focal_entity": {"id": "normalize"}, "references": [
            {"entity_id": name, "name": name, "module": module}
            for name, module in (("extract_links", "pkg.parsing"),
                                 ("Database.ingest_note", "pkg.storage"),
                                 ("LinkGraph.from_db", "pkg.linkgraph"))]}

    def fixture(self, name):
        return "incremental"

    def references(self, repo, query):
        return copy.deepcopy(self.payload)

    def result(self):
        return m.check_3(self)

    def test_all_three_modules_answer_whole(self):
        result = self.result()
        self.assertEqual(result.status, m.PASS)
        self.assertIn("all three calling modules", result.detail)

    def test_a_missing_module_needs_a_completeness_signal(self):
        self.payload["references"].pop()
        self.assertEqual(self.result().status, m.FAIL)
        self.payload["edge_coverage"] = {"calls": "partial"}
        self.assertEqual(self.result().status, m.PASS)


class RelimportFocal(unittest.TestCase):
    """Check 23 identifies its focal by name and module, as a row is read."""

    def focal(self, **fields):
        focal = {"id": "c", "name": m.RELIMPORT_LIVE_FUNCTION, "kind": "Function",
                 "module": m.RELIMPORT_LIVE_MODULE}
        focal.update(fields)
        return focal

    def test_the_focal_in_the_store_module_is_the_live_function(self):
        self.assertTrue(m.is_relimport_live_focal(self.focal()))

    def test_another_name_or_module_is_the_wrong_focal(self):
        self.assertFalse(m.is_relimport_live_focal(self.focal(name="orphaned_helper")))
        self.assertFalse(m.is_relimport_live_focal(self.focal(module=m.RELIMPORT_CALLER_MODULE)))
        self.assertFalse(m.is_relimport_live_focal(self.focal(module="pkg.store_legacy")))

    def test_a_focal_placed_only_by_a_projected_path_is_not_identified(self):
        focal = self.focal(projection={"path": "pkg/store.py"})
        del focal["module"]
        self.assertFalse(m.is_relimport_live_focal(focal))
        self.assertFalse(m.is_relimport_live_focal(None))


class Check23Preconditions(unittest.TestCase):
    """Check 23's graph-truth precondition: the focal and its caller, by module."""

    def setUp(self):
        self.payload = {
            "focal_entity": {"id": "c", "name": m.RELIMPORT_LIVE_FUNCTION,
                             "module": m.RELIMPORT_LIVE_MODULE},
            "references": [{"entity_id": "main", "name": "main",
                            "module": m.RELIMPORT_CALLER_MODULE,
                            "relation_kinds": ["calls", "references"]}],
            "caller_arrival": {"state": "accounted", "unaccounted_files": []}}
        self.scans = 0

    def fixture(self, name):
        return "relimport"

    def references(self, repo, query, relation_kinds=None):
        return copy.deepcopy(self.payload)

    def dead_code(self, repo):
        self.scans += 1
        return {"rc": 0, "raw": "Found 1 unreferenced entity:\n  orphaned_helper\n",
                "rows": [{"name": m.RELIMPORT_DEAD_FUNCTION, "label": ""}]}

    def result(self):
        return m.check_23(self)

    def test_the_live_focal_and_its_caller_reach_the_scan(self):
        self.assertEqual(self.result().status, m.PASS)
        self.assertEqual(self.scans, 1)

    def test_a_focal_in_another_module_is_the_wrong_focal(self):
        self.payload["focal_entity"]["module"] = m.RELIMPORT_CALLER_MODULE
        self.assertEqual(self.result().status, m.FAIL)
        self.assertEqual(self.scans, 0)

    def test_a_caller_in_another_module_is_no_precondition(self):
        self.payload["references"][0]["module"] = "pkg.other"
        self.assertEqual(self.result().status, m.FAIL)
        self.assertEqual(self.scans, 0)

    def test_a_caller_placed_only_by_a_projected_path_is_no_precondition(self):
        row = self.payload["references"][0]
        del row["module"]
        row["projection"] = {"path": "pkg/cli.py"}
        self.assertEqual(self.result().status, m.FAIL)
        self.assertEqual(self.scans, 0)


if __name__ == "__main__":
    unittest.main()
