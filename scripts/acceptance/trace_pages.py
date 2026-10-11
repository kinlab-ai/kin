#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""Strict trace-page transport for acceptance probes, preserving semantic fields."""
import json
import copy
import os
import selectors
import subprocess
import tempfile
import time

# An agent profile serves each answer in plain words and leaves out the verdict
# codes and evidence blocks this suite reads. Ask every kin it starts for the
# machine replies instead; the full profile always serves them.
os.environ.setdefault("KIN_MCP_MACHINE_REPLIES", "1")


class TraceAnswer(dict):
    """Reconstructed evidence with out-of-band observations of the raw pages."""
    page_bytes = ()
    page_ceilings = ()
    page_observations = ()
    page_wires = ()


def drain_trace(fetch, requested=None):
    """fetch(cursor) returns (decoded page, exact emitted JSON text)."""
    return _drain_pages(fetch, requested, "chain")


def drain_references(fetch, requested=None):
    """Reconstruct reference records and their original safety readings."""
    return _drain_pages(fetch, requested, "references")


# Every bucket an impact page carries rows in, in the server's page order
# (`PageKind::Impact` in crates/kin-mcp/src/trace_pages.rs). A bucket missing
# here makes `returned_rows` disagree with the rows this reader counts.
IMPACT_COLLECTIONS = (
    "affected_callers", "affected_value_uses", "affected_overrides",
    "affected_override_callers", "override_caller_candidates",
    "same_name_implementations", "affected_dependents",
    "affected_contract_consumers", "affected_tests", "entity_impacts",
    "affected_work_items", "affected_annotations", "cross_repo_impact",
)


def drain_impact(fetch, requested=None):
    """Reconstruct every impact bucket and its original safety readings."""
    return _drain_pages(fetch, requested, "affected_callers")


# A ranking's rows, on whichever arm ranked them.
LOCATE_COLLECTIONS = ("entities", "results")


def drain_locate(fetch, requested=None):
    """Reconstruct a locate answer that paged, or return one that fit whole.

    A locate answer that fits its budget is served as it always was, with no
    page contract. One that does not is frozen and paged, and its ranking's own
    `next_cursor` comes back among the readings.
    """
    return _drain_pages(fetch, requested, "entities")


def restore_compacted(answer, entries):
    """Restore only repetitions the page explicitly points to in its own answer."""
    if not isinstance(entries, list):
        raise ValueError("page compacted metadata is not an array")

    def address(path):
        if not isinstance(path, str) or not path or any(not part for part in path.split(".")):
            raise ValueError("page has an invalid compacted field path")
        parent = answer
        parts = path.split(".")
        for part in parts[:-1]:
            if not isinstance(parent, dict) or part not in parent:
                raise ValueError("missing compacted reference: %s" % path)
            parent = parent[part]
        if not isinstance(parent, dict):
            raise ValueError("missing compacted reference: %s" % path)
        return parent, parts[-1]

    def value(path):
        parent, key = address(path)
        if key not in parent:
            raise ValueError("missing compacted reference: %s" % path)
        return parent[key]

    seen = set()
    for entry in entries:
        if not isinstance(entry, dict) or not isinstance(entry.get("path"), str):
            raise ValueError("page has malformed compacted metadata")
        path = entry["path"]
        if path in seen:
            raise ValueError("page compacted one field twice")
        seen.add(path)
        if set(entry) == {"path", "same_as"}:
            parent, key = address(path)
            if key in parent:
                raise ValueError("compacted duplicate was not omitted: %s" % path)
            parent[key] = copy.deepcopy(value(entry["same_as"]))
        elif set(entry) != {"path", "restates"}:
            raise ValueError("page has unknown compacted metadata")
    for entry in entries:
        if "restates" not in entry:
            continue
        path, references = entry["path"], entry["restates"]
        if path != "negative.advice" or not isinstance(references, list) or not references:
            raise ValueError("page has an invalid compacted restatement")
        parent, key = address(path)
        text = value(path)
        if not isinstance(text, str):
            raise ValueError("compacted advice is not text")
        used = set()
        for reference in references:
            if reference not in ("negative.subject", "_kin.graph_as_of", "negative.trust_reason"):
                raise ValueError("page advice names an unknown compacted reference")
            marker = "{%s}" % reference
            if reference in used or marker not in text:
                raise ValueError("compacted advice does not carry its advertised reference")
            used.add(reference)
            original = value(reference)
            replacement = (original if isinstance(original, str) else
                           json.dumps(original, separators=(",", ":"), ensure_ascii=False,
                                      sort_keys=True))
            text = text.replace(marker, replacement)
        parent[key] = text
    return answer


def _drain_pages(fetch, requested, primary):
    result, rows, fields, fragments, seen, sizes, ceilings = {}, {}, {}, {}, set(), [], []
    cursor = None
    observations, wires = [], []
    collections = (("chain", "candidates", "more_candidates", "target_candidates")
                   if primary == "chain" else
                   IMPACT_COLLECTIONS + ("target_candidates",)
                   if primary == "affected_callers" else
                   LOCATE_COLLECTIONS
                   if primary == "entities" else
                   ("references", "candidates", "interface_dispatch.candidates",
                    "call_sites.candidates", "candidates_by_owner", "target_candidates"))

    def record(collection, key, value):
        if collection == "readings":
            if key in result:
                raise ValueError("trace reading appeared twice: %s" % key)
            result[key] = value
        else:
            if collection not in collections:
                raise ValueError("unknown semantic collection: %s" % collection)
            rows.setdefault(collection, []).append(value)

    for _ in range(10000):
        page, wire = fetch(cursor)
        metadata = (page.get("_kin") or {}).get("page") or {}
        if primary == "entities" and cursor is None and metadata.get("version") is None:
            # A locate answer that fit its budget ships whole, unpaged.
            answer = TraceAnswer(page)
            answer.page_bytes = (len(wire.encode("utf-8")),)
            answer.page_wires = (wire,)
            return answer
        if metadata.get("version") != 1:
            raise ValueError("trace response is missing its page contract")
        compacted = metadata.get("compacted", [])
        if observations and compacted != observations[0]["page"].get("compacted", []):
            raise ValueError("page compacted metadata changed during continuation")
        kind = ("impact" if primary == "affected_callers" else
                "references" if primary == "references" else
                "locate" if primary == "entities" else None)
        if kind is not None and metadata.get("kind") != kind:
            raise ValueError("%s response has another page kind" % kind)
        if kind in ("impact", "locate"):
            row_collections = IMPACT_COLLECTIONS if kind == "impact" else LOCATE_COLLECTIONS
            total_rows = metadata.get("total_rows")
            if type(total_rows) is not int or total_rows < 0:
                raise ValueError("%s page reports no valid total_rows" % kind)
            if observations and total_rows != observations[0]["page"].get("total_rows"):
                raise ValueError("%s page row total changed during continuation" % kind)
            returned = sum(len(page.get(key) or []) for key in row_collections)
            if metadata.get("returned_rows") != returned:
                raise ValueError("%s page returned_rows does not match its bucket rows" % kind)
            if metadata.get("complete") is True and returned != total_rows:
                raise ValueError("complete %s page lost bucket rows" % kind)
        accounting = page["_kin"].get("response") or {}
        ceiling = accounting.get("max_chars")
        size = len(wire.encode("utf-8"))
        if not isinstance(ceiling, int) or not 2000 <= ceiling <= 60000:
            raise ValueError("trace page reports an invalid byte ceiling")
        if requested is not None and ceiling != max(2000, min(60000, requested)):
            raise ValueError("trace page did not honor the requested byte ceiling")
        if size > ceiling or accounting.get("chars_after_budget") != size:
            raise ValueError("trace page wire bytes disagree with its bound/accounting")
        sizes.append(size)
        wires.append(wire)
        ceilings.append(ceiling)
        observations.append({"page": dict(metadata),
                             "verdict": dict(page["_kin"].get("verdict") or {}),
                             "completeness": dict(page["_kin"].get("completeness") or {}),
                             "negative": dict(page.get("negative") or {})})
        if metadata.get("complete") is True:
            # A whole locate page keeps its ranking's cursor, which pages the
            # ranking rather than this answer.
            if cursor is not None or (page.get("next_cursor") is not None and kind != "locate"):
                raise ValueError("complete trace cannot occur inside a continuation")
            factors = ((page["_kin"].get("verdict") or {}).get("limiting_factor") or "")
            labels = {clause.strip().split(":", 1)[0] for clause in factors.split(";")}
            if labels.intersection(("trace_page_partial", "reference_page_partial",
                                    "impact_page_partial", "locate_page_partial")):
                raise ValueError("complete response carries a partial-page verdict")
            answer = restore_compacted(TraceAnswer(page), compacted)
            answer.page_bytes = tuple(sizes)
            answer.page_ceilings = tuple(ceilings)
            answer.page_observations = tuple(observations)
            answer.page_wires = tuple(wires)
            return answer
        if (page.get("negative") or {}).get("safe_to_conclude_absent") is not False:
            raise ValueError("an individual trace page certifies absence")
        # A partial page carries the answer's own verdict state and limiting
        # factor, and says it is partial in its page-scoped completeness.
        verdict = page["_kin"].get("verdict") or {}
        if (verdict.get("safe_to_conclude_absent") is not False
                or verdict.get("state") not in ("certified", "inconclusive")):
            raise ValueError("an individual trace page has an unqualified verdict")
        completeness = page["_kin"].get("completeness") or {}
        required_limit = ("impact_page_partial" if kind == "impact" else
                          "reference_page_partial" if kind == "references" else
                          "locate_page_partial" if kind == "locate" else
                          "trace_page_partial")
        if (completeness.get("status") != "partial"
                or required_limit not in (completeness.get("limits") or ())):
            raise ValueError("an individual trace page does not say it is partial")
        for collection in collections:
            values = page
            for part in collection.split("."):
                values = values.get(part) if isinstance(values, dict) else None
            for value in values or []:
                record(collection, None, value)
        for reading in page.get("readings") or []:
            record("readings", reading["key"], reading["value"])
        fragment = page.get("record_fragment")
        if fragment:
            address = (fragment["collection"], fragment["index"])
            field = fragment.get("field")
            key = address + (field,)
            accumulated = fragments.get(key, "")
            if len(accumulated.encode("utf-8")) != fragment["byte_offset"]:
                raise ValueError("trace fragment offset has a gap or overlap")
            accumulated += fragment["text"]
            fragments[key] = accumulated
            if fragment["field_complete"]:
                if len(accumulated.encode("utf-8")) != fragment["total_bytes"]:
                    raise ValueError("trace field completed at the wrong byte count")
                encoding = fragment["encoding"]
                if encoding not in ("utf8", "json_utf8"):
                    raise ValueError("unknown trace fragment encoding")
                value = accumulated if encoding == "utf8" else json.loads(accumulated)
                del fragments[key]
                if field is not None:
                    assembled = fields.setdefault(address, {})
                    if field in assembled:
                        raise ValueError("trace field appeared twice")
                    assembled[field] = value
                    if fragment["record_complete"]:
                        value = fields.pop(address)
                if field is None or fragment["record_complete"]:
                    record(address[0], fragment.get("key"), value)
        cursor = page.get("next_cursor")
        if cursor is None:
            if fields or fragments:
                raise ValueError("trace ended with unfinished semantic fields")
            target = rows.pop("target_candidates", None)
            for collection, values in rows.items():
                destination = result
                path = collection.split(".")
                for part in path[:-1]:
                    destination = destination.setdefault(part, {})
                if path[-1] in destination:
                    raise ValueError("semantic collection appeared in both readings and rows")
                destination[path[-1]] = values
            result.setdefault(primary, [])
            if target is not None:
                result.setdefault("target_ambiguity", {})["candidates"] = target
            if primary == "affected_callers":
                count = sum(len(result.get(key) or []) for key in IMPACT_COLLECTIONS)
                if count != metadata.get("total_rows"):
                    raise ValueError("impact continuation lost bucket rows")
            elif primary == "entities":
                count = sum(len(result.get(key) or []) for key in LOCATE_COLLECTIONS)
                if count != metadata.get("total_rows"):
                    raise ValueError("locate continuation lost ranked rows")
            else:
                count = "total_steps" if primary == "chain" else "total_references"
                if len(result[primary]) != metadata.get(count):
                    raise ValueError("trace continuation lost chain steps" if primary == "chain"
                                     else "reference continuation lost caller rows")
            answer = restore_compacted(TraceAnswer(result), compacted)
            answer.page_bytes = tuple(sizes)
            answer.page_ceilings = tuple(ceilings)
            answer.page_observations = tuple(observations)
            answer.page_wires = tuple(wires)
            return answer
        if not isinstance(cursor, str) or len(cursor) > 256 or cursor in seen:
            raise ValueError("trace cursor is malformed or did not advance")
        seen.add(cursor)
    raise ValueError("trace page count exceeded the probe limit")


# The detail a reference probe can ask for, and the value that asks for it. The
# agent profiles, `agent-default` among them, which a bare `kin mcp start`
# serves, always answer references in the answer-only shape and do not take
# these, so a probe that reads the detailed diagnostic blocks is served by the
# `full` profile, which does.
REFERENCE_DETAIL = (("answer_only", False), ("explain", True), ("compact", False))


def profile_env(env, tool, arguments):
    """The environment one call is spawned with.

    Unchanged unless the call asks a reference tool for detail, and then the
    same environment serving `full`.
    """
    if tool not in ("find_references", "refs"):
        return env
    if not any(arguments.get(key) is value for key, value in REFERENCE_DETAIL):
        return env
    env = dict(env or os.environ)
    env["KIN_MCP_TOOL_PROFILE"] = "full"
    return env


def mcp_trace(kin, repo, env, arguments, timeout=600):
    """Keep one stdio process/session for the complete continuation sequence."""
    return _mcp_pages(kin, repo, env, arguments, timeout, "trace_data_flow", drain_trace)


def mcp_references(kin, repo, env, arguments, timeout=600):
    """A reference cursor belongs to the same live stdio session as its first page."""
    return _mcp_pages(kin, repo, env, arguments, timeout, "find_references", drain_references)


def mcp_locate(kin, repo, env, arguments, timeout=600):
    """A locate page cursor stays in the stdio session owning its frozen answer."""
    return _mcp_pages(kin, repo, env, arguments, timeout, "semantic_locate", drain_locate)


def mcp_impact(kin, repo, env, arguments, timeout=600, transcript_path=None):
    """An impact cursor stays in the stdio session owning its frozen answer."""
    return _mcp_pages(kin, repo, env, arguments, timeout, "impact_analysis", drain_impact,
                      transcript_path=transcript_path)


def _mcp_pages(kin, repo, env, arguments, timeout, tool, drain, transcript_path=None):
    env = profile_env(env, tool, arguments)
    with tempfile.TemporaryFile(mode="w+") as errors:
        proc = subprocess.Popen([kin, "mcp", "start", "--repo", repo], cwd=repo,
                                env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=errors, text=True, bufsize=1)
        selector = selectors.DefaultSelector()
        selector.register(proc.stdout, selectors.EVENT_READ)
        sequence = [0]
        pending = bytearray()

        def request(method, params=None, notification=False):
            sequence[0] += 1
            frame = {"jsonrpc": "2.0", "method": method}
            if params is not None:
                frame["params"] = params
            if not notification:
                frame["id"] = sequence[0]
            proc.stdin.write(json.dumps(frame) + "\n")
            proc.stdin.flush()
            if notification:
                return None
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if b"\n" not in pending:
                    if not selector.select(max(0, deadline - time.monotonic())):
                        break
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    if not chunk:
                        raise ValueError("trace MCP process exited before replying")
                    pending.extend(chunk)
                    continue
                line, _, rest = pending.partition(b"\n")
                pending[:] = rest
                try:
                    response = json.loads(line)
                except ValueError:
                    continue
                if response.get("id") != sequence[0]:
                    continue
                if "error" in response:
                    raise ValueError("trace MCP error: %s" % response["error"])
                return response.get("result") or {}
            raise ValueError("trace MCP request timed out")

        try:
            request("initialize", {"protocolVersion":"2024-11-05", "capabilities":{},
                    "clientInfo":{"name":"kin-trace-pages-acceptance", "version":"1"}})
            request("notifications/initialized", notification=True)

            def fetch(cursor):
                args = dict(arguments)
                if cursor is not None:
                    args["cursor"] = cursor
                result = request("tools/call", {"name":tool, "arguments":args})
                if transcript_path is not None:
                    with open(transcript_path, "a") as transcript:
                        json.dump({"tool": tool, "arguments": args, "result": result}, transcript)
                        transcript.write("\n")
                content = result.get("content") or []
                if (result.get("isError") and tool in ("trace_data_flow", "impact_analysis", "semantic_locate")) or not content or "text" not in content[0]:
                    raise ValueError("trace MCP refused the page: %s" % result)
                wire = content[0]["text"]
                return json.loads(wire), wire

            return drain(fetch, arguments.get("max_chars", arguments.get("max_response_chars")))
        finally:
            selector.close()
            proc.stdin.close()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
            proc.stdout.close()
