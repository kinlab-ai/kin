#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Firelock, LLC
"""NON-CITABLE acceptance suite for the one-verdict contract (FIR-2672).

Its output is a regression gate, never proof, never investor-facing and never a
released claim. It shares the CHECK line format, the exit codes and the
`--self-test` discipline of its siblings in this directory.

What it is for
--------------
The rc0552s green stranger asked `find_references` for a Python function on
0.5.52, got a verdict reading `state: certified`, `edge_coverage: certified`,
`limiting_factor: null`, `completeness.status: complete`, `bound: exact`, and a
note saying the counts were the whole set, in the same `_kin` block that
recorded `classes.imports: "absent"` and `limits: ["edge_coverage:imports_absent"]`.
It renamed the sites Kin certified and the code broke on the import sites Kin
had never been able to read. Every release from 0.5.43, the first to carry the
one verdict, certified the same way; the rule was born certifying over a
recorded limit.

The contract the MCP instructions state is one verdict per response, computed
from every block that qualifies the answer, with the most pessimistic input
winning. These checks hold the envelope to it on a live store:

  invariant   on the default query (calls, imports, references), no requested
              class in `completeness.classes` may read anything but `present`
              while the verdict certifies; when one does, the verdict is
              inconclusive, its limiting factor names the class, the
              completeness status is not `complete`, the bound is a floor, and
              `limits` names the class. Present non-call classes do not prove
              their own completion. Either the separately recorded gap, a
              settled Call census and an explicit lower bound qualify the
              positive, or a settled Call census whose mention reading matched
              every non-call read against a requested row certifies it. The
              shipped certification over absent classes fails.
  inverse     the control that keeps the invariant from being satisfied by
              refusing everything. The same focal over `calls` alone must
              return a real call witness and a settled Call census, certify
              with `limiting_factor: null`, and carry an exact count.
  unproduced  the import class is never `absent` on a source whose files
              import one another: it reads `unproduced` on a build whose
              linker emits no entity-level import edge, and `present` on one
              whose linker does. `absent` on this source is the 0.5.52 wording
              this ticket is about.
  two_reasons the same query under the server's smallest response budget.
              Every partial page refuses absence with its own paging reason,
              and reassembly restores the original verdict. Every input that
              verdict records as inconclusive keeps its clause in
              `limiting_factor`, each label once: the budget's clause beside
              the class gap on a build that cannot produce the import class,
              the budget's clause alone on one that can. The shape this guards
              against kept one clause and dropped the rest, which one level up
              is a CLI line naming the edge gap and not the dead embedding
              worker beside it.

    CHECK <id> <ticket> PASS|FAIL|UNREADABLE <detail>

UNREADABLE is a distinct outcome from FAIL and is never reported as a pass.
Exit status is 1 when any check FAILs, 2 when none fail but some are
UNREADABLE, 0 only when every check passes, 3 on a setup error.

The binary under test
---------------------
    cargo build --release --locked --bin kin --bin kin-daemon
    python3 scripts/acceptance/verdict_limits_repro.py --kin target/release/kin

`--kin` may also come from KIN_BIN. The kin-daemon beside it is used when one
exists. The fixture is Python and the `references` class needs a Python
language server (pyright) reachable by the daemon, as the acceptance workflow
installs; without one, `references` reads short and the checks say so.
"""

import argparse
import json
import re
import os
import shutil
import subprocess
import sys
import tempfile
import time

# An agent profile serves each answer in plain words and leaves out the verdict
# codes and evidence blocks this suite reads. Ask every kin it starts for the
# machine replies instead; the full profile always serves them.
os.environ.setdefault("KIN_MCP_MACHINE_REPLIES", "1")

from trace_pages import mcp_references
from runtime_fixture import prepare_runtime_fixture

PASS = "PASS"
FAIL = "FAIL"
UNREADABLE = "UNREADABLE"
TICKET = "FIR-2672"
CLASSES = ("calls", "imports", "references")

# The `_kin` block the rc0552s stranger received on 0.5.52, verbatim in every
# field the graders read. It must fail the invariant grader or this suite
# guards nothing.
SHIPPED_0552 = {
    "_kin": {
        "completeness": {
            "bound": "exact",
            "classes": {"calls": "present", "imports": "absent", "references": "present"},
            "counted": {"exact": True, "reported": 5, "unit": "referencing_entities"},
            "decided_by": ["calls", "references"],
            "limits": ["edge_coverage:imports_absent"],
            "note": "This answer rested on the calls, references edge class(es), and each was "
                    "observed present, so the counts here are the whole set.",
            "status": "complete",
            "substrate": "edges",
        },
        "verdict": {
            "inputs": {"absence_gate": "certified", "completeness": "certified",
                       "degradations": "not_applicable", "edge_coverage": "certified",
                       "withheld_candidates": "certified"},
            "limiting_factor": None,
            "note": "Every input that could qualify this answer agreed, so the counts here are "
                    "the whole set and an absence in it is authoritative.",
            "safe_to_conclude_absent": False,
            "state": "certified",
        },
    },
    "references": [{}] * 5,
    "relation_kinds": ["calls", "imports", "references"],
}


# BEGIN failure evidence excerpt
# Every suite carries this block byte for byte, because a suite is also copied
# out and run as a single file. test_failure_excerpt.py keeps the copies equal.
EVIDENCE_LIMIT = 4000
EVIDENCE_LINE_LIMIT = 600
EVIDENCE_PANICS = ("panicked at", "has overflowed its stack")
EVIDENCE_ESCAPES = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
EVIDENCE_ERROR = re.compile(r"^(?:[\w./-]+:\s*)?(?:error|fatal)\b", re.IGNORECASE)
EVIDENCE_LOG_ERROR = re.compile(r"\sERROR\s")


def failure_excerpt(text, limit=EVIDENCE_LIMIT):
    """Bounded evidence from a command's output that still says why it failed.

    Output that fits is returned whole. Longer output keeps its opening and its
    end, and between them every line that carries a Rust panic, with the
    message line under it, and the last error line, wherever they fall. A
    warning printed around the error cannot push it out, and a long log cannot
    cut the panic out. `limit` only ever raises the bound, never lowers it.
    """
    text = (text or "").strip()
    limit = max(int(limit), EVIDENCE_LIMIT)
    if len(text) <= limit:
        return text
    head_end = limit // 4
    tail_start = len(text) - limit // 2
    lines = text.split("\n")
    starts, offset = [], 0
    panics, errors = [], []
    for index, line in enumerate(lines):
        starts.append(offset)
        offset += len(line) + 1
        plain = EVIDENCE_ESCAPES.sub("", line).strip()
        if any(marker in plain for marker in EVIDENCE_PANICS):
            panics.extend((index, index + 1))
        if EVIDENCE_ERROR.match(plain) or EVIDENCE_LOG_ERROR.search(plain):
            errors.append(index)
    # The first panic and its message, then the last error line, then any
    # later panics, for as long as the middle's share of the bound lasts.
    order = panics[:2] + errors[-1:] + panics[2:]
    budget, kept = limit // 4, set()
    for index in order:
        if index >= len(lines) or index in kept:
            continue
        start, end = starts[index], starts[index] + len(lines[index])
        if end <= head_end or start >= tail_start:
            continue
        cost = min(len(lines[index]), EVIDENCE_LINE_LIMIT) + 1
        if cost > budget:
            continue
        kept.add(index)
        budget -= cost
    middle = [lines[index][:EVIDENCE_LINE_LIMIT] for index in sorted(kept)]
    parts = [text[:head_end], "[...]"] + middle + (["[...]"] if middle else [])
    return "\n".join(parts + [text[tail_start:]])
# END failure evidence excerpt


def tail(text, limit=400):
    """Quote a command's output as evidence through failure_excerpt."""
    return failure_excerpt(text, limit)


def run(cmd, cwd=None, env=None, timeout=600):
    proc = subprocess.run(cmd, cwd=cwd, env=env, timeout=timeout,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return proc.returncode, proc.stdout


class Result(object):
    def __init__(self, check_id, title):
        self.id = check_id
        self.title = title
        self.asserts = []

    def ok(self, detail):
        self.asserts.append({"status": PASS, "detail": detail})

    def bad(self, detail):
        self.asserts.append({"status": FAIL, "detail": detail})

    def unknown(self, detail):
        self.asserts.append({"status": UNREADABLE, "detail": detail})

    @property
    def status(self):
        graded = [a for a in self.asserts if a["status"] in (PASS, FAIL, UNREADABLE)]
        if any(a["status"] == FAIL for a in graded):
            return FAIL
        if any(a["status"] == UNREADABLE for a in graded):
            return UNREADABLE
        if not graded:
            return UNREADABLE
        return PASS

    @property
    def detail(self):
        for wanted in (FAIL, UNREADABLE):
            for a in self.asserts:
                if a["status"] == wanted:
                    return a["detail"]
        graded = [a["detail"] for a in self.asserts if a["status"] == PASS]
        return "; ".join(graded) if graded else "no assertion was reached"


# ------------------------------------------------------------------- graders

def reference_completion_qualification(payload, negative, verdict):
    """A settled Call census does not certify requested non-call completion.

    Accept only this recorded gap, with a lower bound and no absence claim.
    This qualifies the returned positives; it never certifies their whole set.
    """
    code = "reference_completion_unmeasured"
    kinds = payload.get("relation_kinds")
    inputs = verdict.get("inputs")
    comp = (payload.get("_kin") or {}).get("completeness") or {}
    block = payload.get("call_sites")
    if (not isinstance(kinds, list)
            or not any(isinstance(kind, str) and kind.lower() != "calls" for kind in kinds)):
        return False, "no requested non-call relation backs the completion gap"
    if (verdict.get("state") != "inconclusive" or negative.get("trust") != "inconclusive"
            or not isinstance(inputs, dict)
            or inputs.get("reference_completion") != "inconclusive"
            or inputs.get("absence_gate") != "inconclusive"
            or inputs.get("call_sites") != "certified"):
        return False, "the verdict does not separately attribute the non-call completion gap"
    if any(reading not in ("certified", "not_applicable") for name, reading in inputs.items()
           if name not in ("reference_completion", "absence_gate")):
        return False, "another verdict input limits the positive beyond non-call completion"
    codes = [part.strip() for part in str(verdict.get("limiting_factor") or "").split(";")
             if part.strip()]
    reason = negative.get("trust_reason")
    if (codes != [code]
            or reason != code + ": non-call completion is unmeasured"
            or code not in str(negative.get("advice") or "")):
        return False, "non-call completion provenance does not match its exact limiting code"
    if (verdict.get("absence_claim") != "not_applicable"
            or verdict.get("safe_to_conclude_absent") is not False
            or negative.get("safe_to_conclude_absent") is not False
            or comp.get("status") == "complete" or comp.get("bound") != "at_least"
            or (comp.get("counted") or {}).get("exact") is not False
            or "verdict_inconclusive" not in (comp.get("limits") or [])):
        return False, "non-call uncertainty claims an exact count or an absence"
    if (not isinstance(block, dict) or block.get("settled") is not True
            or block.get("clauses") != [] or block.get("candidates") != []
            or type(block.get("candidate_count")) is not int or block["candidate_count"] != 0
            or block.get("candidates_withheld", 0) != 0):
        return False, "no settled, residue-free Call census backs the separate completion gap"
    return True, "bounded by non-call completion, with the Call census settled"


def measured_non_call_completion(payload, verdict):
    """A certification of non-call completion that the census actually measured.

    The server certifies imports and references only when the Call census is
    settled and its mention reading matched every non-call read that could
    reach the focal against a stored row of a requested kind. Accept exactly
    that evidence: a measured reading with nothing unmatched, no lookup by a
    computed name, no member-held references left unmeasured, and no read
    carried only by a kind the query did not ask for.
    """
    requested = [kind.lower() for kind in payload.get("relation_kinds") or []
                 if isinstance(kind, str)]
    if (verdict.get("inputs") or {}).get("reference_completion") != "certified":
        return False, "the verdict does not attribute the certification to reference completion"
    block = payload.get("call_sites")
    if (not isinstance(block, dict) or block.get("settled") is not True
            or block.get("clauses") != []):
        return False, "no settled, clause-free Call census backs the certification"
    residue = ((block.get("focal_escape") or {}).get("residue")) or {}
    mentions = residue.get("mentions")
    if not isinstance(mentions, dict) or mentions.get("measured") is not True:
        return False, "the Call census took no mention reading"
    if "references" in requested and mentions.get("references_unmeasured") is not None:
        return False, "the mention reading left member-held references unmeasured"
    if ((residue.get("unkeyed_lookups") or {}).get("count") or 0) != 0:
        return False, "lookups by a computed name could reach the focal"
    if ((mentions.get("unmatched") or {}).get("count") or 0) != 0:
        return False, "the mention reading lists non-call reads with no stored row"
    matched = mentions.get("matched") or {}
    if (not isinstance(matched, dict)
            or any(type(count) is not int or count < 0 for count in matched.values())):
        return False, "the mention reading's matched counts are not whole numbers"
    elsewhere = sorted(kind for kind, count in matched.items()
                       if count > 0 and kind.lower() not in requested)
    if elsewhere:
        return False, "reads are carried only by unrequested %s rows" % ", ".join(elsewhere)
    # `count` is every read the reading took, matched or not. With nothing
    # listed unmatched it must equal the matched total, or a read went unaccounted.
    total = mentions.get("count")
    if type(total) is not int or total != sum(matched.values()):
        return False, ("the mention reading counts %r reads and matched %d"
                       % (total, sum(matched.values())))
    return True, "every non-call read was matched by the mention census"


def verdict_honours_classes(payload):
    """Every unread requested class makes the verdict inconclusive and named.

    Presence alone does not prove non-call completion. A fully present answer
    either retains the separately attributed, explicitly bounded completion
    gap, or certifies on a measured mention census that matched every non-call
    read; an unexplained refusal or a certification without that census fails.

    Returns (verdict, problems). `verdict` is None when the payload does not
    carry the fields, which is UNREADABLE rather than a finding.
    """
    kin = payload.get("_kin") or {}
    comp = kin.get("completeness") or {}
    verdict = kin.get("verdict") or {}
    classes = comp.get("classes")
    state = verdict.get("state")
    if not isinstance(classes, dict) or state not in ("certified", "inconclusive"):
        return None, ["no completeness.classes map or no verdict state"]
    requested = [c for c in (payload.get("relation_kinds") or list(classes)) if c in classes]
    if not requested:
        return None, ["no requested class is in completeness.classes"]
    short = [c for c in requested if classes.get(c) != "present"]
    problems = []
    limits = comp.get("limits") or []
    counted = comp.get("counted") or {}
    if short:
        if state != "inconclusive":
            problems.append("verdict is %r while %s read %s"
                            % (state, ", ".join(short), [classes[c] for c in short]))
        factor = verdict.get("limiting_factor") or ""
        if not any(c in factor for c in short):
            problems.append("limiting_factor %r names none of %s" % (factor[:120], short))
        if comp.get("status") == "complete":
            problems.append("completeness.status is complete over %s" % short)
        if comp.get("bound") == "exact" or counted.get("exact") is True:
            problems.append("bound/counted.exact claim an exact count over %s" % short)
        if not any(any(l.startswith("edge_coverage:%s_" % c) for c in short) for l in limits):
            problems.append("limits %s name none of %s" % (limits, short))
        if (verdict.get("inputs") or {}).get("edge_coverage") == "certified":
            problems.append("inputs.edge_coverage reads certified over %s" % short)
    elif state == "inconclusive":
        qualified, reason = reference_completion_qualification(
            payload, payload.get("negative") or {}, verdict)
        if not qualified:
            problems.append("every class is present but the refusal is unproven: %s" % reason)
    elif any(c != "calls" for c in requested):
        measured, reason = measured_non_call_completion(payload, verdict)
        if not measured:
            problems.append("the verdict certifies non-call completion from edge-class "
                            "presence: %s" % reason)
    return state, problems


def import_class_never_absent_on_importing_source(payload):
    """On a source whose files import one another the import class is a fact
    about the build (`unproduced`) or a witnessed edge (`present`), never a
    completed observation about the code (`absent`)."""
    classes = ((payload.get("_kin") or {}).get("completeness") or {}).get("classes")
    if not isinstance(classes, dict) or "imports" not in classes:
        return None
    return classes["imports"] in ("unproduced", "present"), classes["imports"]


# Which clause of `limiting_factor` each verdict input is entitled to, by the
# label its reading writes. The absence gate composes its own clauses out of
# every gap it pushed and is not held to one label.
INPUT_CLAUSE_LABELS = {
    "caller_arrival": ("caller_arrival_",),
    "reference_completion": ("reference_completion_unmeasured",),
    "edge_coverage": ("cross_file_edges_", "edge_coverage_unknown"),
    "withheld_candidates": ("withheld_candidates",),
    "degradations": ("retrieval_degraded",),
    "completeness": ("substrate_", "counts_are_a_floor"),
    "response_budget": ("response_bounded",),
    # absence_gate is deliberately NOT here, and the reason took three attempts
    # to get right, so it is written out.
    #
    # Its clause set is not its own. It has TWO sources. With no negative block
    # it emits `absence_coverage_clauses`, whose twelve labels look like a
    # sensible entry. With a negative block it emits `negative.trust_reason`,
    # which `push_gap` composes from whatever refused anywhere: on the live
    # payload this suite grades, that was `response_bounded`, `answer_truncated`
    # and `retrieval_degraded`, and NONE of the twelve appeared.
    #
    # So a prefix list for this input is either incomplete, which reports a
    # finding every time it refuses through the negative path, or broad enough
    # to be satisfied by any clause at all, which is a check that cannot fail.
    # Both were tried. `factor_carries_the_absence_gates_own_clauses` below
    # checks the mechanism instead: it reads the trust_reason the payload
    # actually carries and requires each of ITS labels in the factor, which
    # needs no vocabulary guess.
}# The smallest response budget the server serves (`RESPONSE_MIN_MAX_CHARS`).
# Asking for it on a populated answer makes the transport withhold rows or
# split them across pages, without changing the underlying graph evidence.
BUDGET_FLOOR_CHARS = 2000


def readiness_pending(payload):
    """Whether the answer says its language-server readiness observation is pending.

    A daemon that has just started establishes readiness with its own server
    handshake, and until that observation completes its answers read
    `reference_enrichment: unknown`, which the verdict rightly refuses to
    certify. The fixture waits on that observation, read from the coverage the
    answer carries, rather than grading the window. A completed finding of any
    kind (available, no language server, unusable, switched off) ends the wait,
    so the checks still grade whatever the host can actually do.
    """
    coverage = payload.get("edge_coverage") if isinstance(payload, dict) else None
    return isinstance(coverage, dict) and coverage.get("reference_enrichment") == "unknown"


def factor_labels(factor):
    """Every clause label the factor names, in order.

    Splitting on the separator is CORRECT here, and it was not always. The
    server used to emit a clause whose own prose contained "; ", so this split
    cut one gap into a labelled clause plus an unlabelled fragment and then
    graded the fragment as though it were a label. That is fixed at the source:
    clauses are carried as a list to a single join, and
    `no_clause_carries_the_separator_that_divides_clauses` in `verdict.rs`
    asserts no clause contains the separator, driving the real producer over
    five payload shapes rather than restating its strings.

    So this parser is a second copy, and that coupling is real, but replacing it
    is worse than keeping it. The obvious replacement, a regex for `word:` at a
    clause start, finds a THIRD label in today's real factor: the absence gate's
    clause contains the parenthetical "...do not stand in for imports: an entity
    other files reach only through imports...", and a mid-sentence colon is
    indistinguishable from a label start without knowing where clauses end.
    Splitting on the separator knows exactly that, and now it is true.

    The dependency is therefore stated rather than removed: this function is
    correct only while the server's invariant holds, and that invariant has its
    own falsified check on the other side.
    """
    clauses = [clause.strip() for clause in (factor or "").split("; ") if clause.strip()]
    return [clause.split(":", 1)[0].strip() for clause in clauses]


def factor_carries_the_absence_gates_own_clauses(payload):
    """The absence gate's clauses reach the factor, checked at the mechanism.

    A prefix list cannot do this. The gate's vocabulary is not its own: with a
    negative block it emits `negative.trust_reason`, which the negative block
    composes from whatever refused anywhere, so its labels are other inputs'
    labels. Reading the trust_reason the payload actually carries needs no guess.

    Returns (checked, problems). `checked` is False when there is nothing to
    check, which is UNREADABLE rather than a pass.
    """
    verdict = ((payload.get("_kin") or {}).get("verdict") or {})
    inputs = verdict.get("inputs")
    negative = payload.get("negative") or {}
    reason = negative.get("trust_reason")
    if not isinstance(inputs, dict) or inputs.get("absence_gate") != "inconclusive":
        return False, []
    if not isinstance(reason, str) or not reason.strip():
        return False, []
    factor = verdict.get("limiting_factor") or ""
    problems = []
    if not factor:
        return True, ["absence_gate refuses with a trust_reason and the factor is empty"]
    for label in factor_labels(reason):
        # A clause the gate carried that another input also carried is
        # deduplicated to one, which still leaves its label in the factor. So a
        # MISSING label is a dropped clause rather than a deduplicated one.
        if label not in factor_labels(factor):
            problems.append(
                "absence_gate's trust_reason names %r and the factor does not carry it "
                "(factor labels %s)" % (label, factor_labels(factor)))
    return True, problems


def factor_carries_every_refusing_input(payload):
    """Every input the verdict records as inconclusive has its clause in
    `limiting_factor`, each label once, and a verdict no input refuses carries
    no clause at all.

    Returns (labels, problems). `labels` is None when the payload carries no
    verdict inputs, which is UNREADABLE rather than a finding.
    """
    verdict = ((payload.get("_kin") or {}).get("verdict") or {})
    inputs = verdict.get("inputs")
    if not isinstance(inputs, dict) or verdict.get("state") not in ("certified", "inconclusive"):
        return None, ["no verdict inputs"]
    factor = verdict.get("limiting_factor") or ""
    labels = factor_labels(factor)
    refusing = [name for name, state in inputs.items() if state == "inconclusive"]
    problems = []
    if refusing and verdict.get("state") != "inconclusive":
        problems.append("inputs %s refuse while the verdict is %r" % (refusing, verdict.get("state")))
    if refusing and not labels:
        problems.append("inputs %s refuse and limiting_factor is empty" % refusing)
    for name in refusing:
        prefixes = INPUT_CLAUSE_LABELS.get(name)
        # Substring against the whole factor, not against parsed boundaries.
        # A refusing input's clause is present or it is not, and answering that
        # needs no opinion about where the clause ends.
        if prefixes and not any(("%s" % pre) in factor for pre in prefixes):
            problems.append("input %s refuses and no clause of the factor is its (labels %s)"
                            % (name, labels))
    dupes = sorted({label for label in labels if labels.count(label) > 1})
    if dupes:
        problems.append("a label is said twice: %s" % dupes)
    if not refusing and labels:
        problems.append("no input refuses yet the factor reads %r" % labels)
    return labels, problems


GRADERS = {
    "verdict_honours_classes": verdict_honours_classes,
    "import_class_never_absent_on_importing_source": import_class_never_absent_on_importing_source,
    "factor_carries_every_refusing_input": factor_carries_every_refusing_input,
}


# ------------------------------------------------------------------- fixtures

# Three files that reach one another through every class the verdict reads:
# `search.py` and the test import from `parsing.py` (imports), call
# `blank_code` (calls), and use the `Note` class as an annotation and read its
# attributes (references, which a language server resolves). A fixture with
# calls alone reads `references: absent` honestly. The invariant reads every
# class; the inverse certifies only the fixture's settled Calls-only domain.
FILES = {
    "pkg/__init__.py": "",
    "pkg/parsing.py": (
        '"""Parsing helpers."""\n\n\n'
        "class Note:\n"
        '    """One parsed note: a title and the body under it."""\n\n'
        "    def __init__(self, title: str, body: str) -> None:\n"
        "        self.title = title\n"
        "        self.body = body\n\n"
        "    def word_count(self) -> int:\n"
        "        return len(self.body.split())\n\n\n"
        "def blank_code(text: str) -> str:\n"
        '    """Mask fenced code so tags inside it are not markup."""\n'
        "    out = []\n"
        "    for line in text.splitlines():\n"
        "        out.append('' if line.startswith('```') else line)\n"
        "    return '\\n'.join(out)\n\n\n"
        "def extract_tags(text: str) -> list:\n"
        "    masked = blank_code(text)\n"
        "    return [w[1:] for w in masked.split() if w.startswith('#')]\n\n\n"
        "def parse_note(text: str) -> Note:\n"
        "    title, _, body = text.partition('\\n')\n"
        "    return Note(title.strip('# '), blank_code(body))\n"
    ),
    "pkg/search.py": (
        "from pkg.parsing import Note, blank_code, extract_tags, parse_note\n\n\n"
        "def index_note(text: str) -> dict:\n"
        "    note: Note = parse_note(text)\n"
        "    body = blank_code(text)\n"
        "    return {'title': note.title, 'tags': extract_tags(text),\n"
        "            'words': note.word_count(), 'raw_words': len(body.split())}\n"
    ),
    "tests/__init__.py": "",
    # The test's name does not contain the focal's name on purpose: the query
    # resolves its focal by name pattern, and a second match would make the
    # focal ambiguous, which is a refusal of its own and not the one under test.
    "tests/test_parsing.py": (
        "from pkg.parsing import Note, blank_code, parse_note\n\n\n"
        "def test_fenced_tags_are_masked():\n"
        "    assert '#nope' not in blank_code('real #tag\\n```\\n#nope\\n```')\n\n\n"
        "def test_parsed_title_is_kept():\n"
        "    note: Note = parse_note('# Alpha\\nbody')\n"
        "    assert note.title == 'Alpha'\n"
    ),
}


class Suite(object):
    def __init__(self, kin, workdir, daemon=None, verbose=False):
        self.kin = kin
        self.workdir = workdir
        self.verbose = verbose
        self.kin_home = os.path.join(workdir, "kin-home-%d" % os.getpid())
        os.makedirs(self.kin_home, exist_ok=True)
        self.env = dict(os.environ)
        self.env["KIN_HOME"] = self.kin_home
        self.env["KIN_DAEMON_AUTO_EMBED"] = "0"
        self.env["KIN_EMBED_BACKEND"] = "cpu"
        self.env["KIN_VFS_DISABLE"] = "1"
        # Written against a store that holds its complete history.
        self.env["KIN_INIT_HISTORY"] = "full"
        self.env.pop("KIN_MCP_REPO", None)
        self.env.pop("KIN_DIR", None)
        if daemon:
            self.env["KIN_DAEMON_BIN"] = daemon
        self.repo = None
        self.payloads = {}
        self.kin, self.daemon = prepare_runtime_fixture(
            self.kin, daemon, self.env, self.env["KIN_HOME"])

    def log(self, line):
        if self.verbose:
            print("  " + line, flush=True)

    def git(self, args, cwd):
        base = ["git", "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"]
        return run(base + args, cwd=cwd, env=self.env)

    def kin_run(self, args, repo, timeout=600):
        rc, out = run([self.kin] + args, cwd=repo, env=self.env, timeout=timeout)
        self.log("kin %s -> %d" % (" ".join(args), rc))
        return rc, out

    def mcp(self, repo, tool, args, timeout=300):
        if tool == "find_references":
            return mcp_references(self.kin, repo, self.env, args, timeout)
        env = dict(self.env)
        env["KIN_MCP_REPO"] = repo
        proc = subprocess.Popen([self.kin, "mcp", "start", "--repo", repo],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, cwd=repo, env=env, text=True)
        msgs = [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize",
             "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": {"name": "kin-verdict-limits-repro", "version": "1"}}},
            {"jsonrpc": "2.0", "method": "notifications/initialized"},
            {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
             "params": {"name": tool, "arguments": args}},
        ]
        try:
            out, err = proc.communicate("".join(json.dumps(m) + "\n" for m in msgs),
                                        timeout=timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate()
            raise RuntimeError("mcp %s timed out after %ss" % (tool, timeout))
        resp = None
        for line in out.splitlines():
            line = line.strip()
            if line.startswith("{"):
                try:
                    obj = json.loads(line)
                except ValueError:
                    continue
                if obj.get("id") == 2:
                    resp = obj
        if resp is None:
            raise RuntimeError("mcp %s returned no id=2 frame (stderr tail: %s)"
                               % (tool, failure_excerpt(err).replace("\n", " ")))
        if "error" in resp:
            raise RuntimeError("mcp %s error: %s" % (tool, json.dumps(resp["error"])[:200]))
        content = (resp.get("result") or {}).get("content") or []
        if not content or "text" not in content[0]:
            raise RuntimeError("mcp %s returned no text content" % tool)
        return json.loads(content[0]["text"])

    def fixture(self):
        """A three-file Python package whose files import and call one another,
        admitted through `kin init`, with its enrichment sweep given time."""
        if self.repo:
            return self.repo
        repo = os.path.join(self.workdir, "importing")
        for rel, body in FILES.items():
            path = os.path.join(repo, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as handle:
                handle.write(body)
        rc, out = self.git(["init", "-q", "--initial-branch=main"], repo)
        if rc != 0:
            raise RuntimeError("git init failed: %s" % tail(out))
        self.git(["config", "user.email", "repro@example.invalid"], repo)
        self.git(["config", "user.name", "kin-verdict-limits-repro"], repo)
        self.git(["add", "--all"], repo)
        rc, out = self.git(["commit", "-q", "-m", "a package whose files import one another"], repo)
        if rc != 0:
            raise RuntimeError("git commit failed: %s" % tail(out))
        rc, out = self.kin_run(["init"], repo, timeout=900)
        if rc != 0:
            raise RuntimeError("kin init failed: %s" % tail(out))
        self.repo = repo
        return repo

    def references(self, kinds=None, attempts=10, extra=None):
        """find_references(blank_code), retried while the reference sweep
        settles, because `references` reads short until the language server has
        run and that is a fact about timing rather than about the verdict.
        Retried too while the answer says its language-server readiness
        observation is still pending (see `readiness_pending`).
        `extra` adds arguments to the call, such as a response budget."""
        repo = self.fixture()
        key = (tuple(kinds or ()), tuple(sorted((extra or {}).items())))
        if key in self.payloads:
            return self.payloads[key]
        args = {"query": "blank_code", "answer_only": False}
        if kinds:
            args["relation_kinds"] = list(kinds)
        args.update(extra or {})
        payload = None
        for attempt in range(attempts):
            self.kin_run(["graph", "status"], repo)
            try:
                payload = self.mcp(repo, "find_references", args)
            except (ValueError, RuntimeError):
                if attempt + 1 >= attempts:
                    raise
                time.sleep(4)
                continue
            classes = ((payload.get("_kin") or {}).get("completeness") or {}).get("classes") or {}
            if (all(classes.get(c) == "present" for c in classes if c != "imports")
                    and not readiness_pending(payload)):
                break
            time.sleep(4)
        self.payloads[key] = payload
        return payload


# --------------------------------------------------------------------- checks

def check_invariant(suite):
    result = Result("invariant", "the verdict never certifies over a requested class it could "
                                 "not read, and retains the separate non-call completion gap")
    try:
        payload = suite.references()
    except Exception as error:  # noqa: BLE001
        result.unknown("find_references(blank_code) unreadable: %s" % error)
        return result
    refs = payload.get("references") or []
    if not refs:
        result.unknown("find_references(blank_code) returned no rows, so the populated-answer "
                       "verdict is not exercised: %s" % tail(json.dumps(payload), 300))
        return result
    state, problems = verdict_honours_classes(payload)
    classes = payload["_kin"]["completeness"].get("classes")
    if state is None:
        result.unknown("; ".join(problems))
    elif problems:
        result.bad("classes=%s verdict=%s: %s" % (json.dumps(classes), state, "; ".join(problems)))
    else:
        result.ok("%d rows, classes=%s, verdict=%s, limiting_factor=%s"
                  % (len(refs), json.dumps(classes), state,
                     (payload["_kin"]["verdict"].get("limiting_factor") or "null")[:90]))
    return result


def check_inverse(suite):
    result = Result("inverse", "a settled Calls-only positive certifies cleanly, "
                               "with no limiting factor and an exact count")
    # Edge-class presence is not a census of non-call uses. Exercise the
    # certifiable domain directly, regardless of which other classes exist.
    try:
        payload = suite.references(kinds=("calls",))
    except Exception as error:  # noqa: BLE001
        result.unknown("find_references(blank_code, calls) unreadable: %s" % error)
        return result
    classes = ((payload.get("_kin") or {}).get("completeness") or {}).get("classes") or {}
    if classes.get("calls") != "present":
        result.unknown("the calls class reads %r on the fixture, so no certified control is "
                       "available" % classes.get("calls"))
        return result
    return grade_inverse(result, payload, "control: settled calls alone")


def grade_inverse(result, payload, arm):
    refs = payload.get("references") or []
    state, problems = verdict_honours_classes(payload)
    kin = payload.get("_kin") or {}
    comp = kin.get("completeness") or {}
    verdict = kin.get("verdict") or {}
    factor = verdict.get("limiting_factor")
    block = payload.get("call_sites") or {}
    witness = any(isinstance(row, dict) and row.get("entity_id")
                  and "calls" in (row.get("relation_kinds") or [])
                  and any(isinstance(site, dict)
                          and type(site.get("line_in_entity")) is int
                          and site["line_in_entity"] >= 0
                          and "blank_code" in str(site.get("callee") or "")
                          for site in row.get("sites") or []) for row in refs)
    if payload.get("relation_kinds") != ["calls"] or not witness:
        problems.append("the control has no Calls-only positive witness for blank_code")
    if (block.get("settled") is not True or block.get("clauses") != []
            or (verdict.get("inputs") or {}).get("call_sites") != "certified"
            # The producer omits this input when it is silent on Calls-only answers.
            or (verdict.get("inputs") or {}).get("reference_completion", "not_applicable")
            != "not_applicable"):
        problems.append("the control does not record a settled Call census without a non-call gap")
    if ((comp.get("counted") or {}).get("exact") is not True
            or (comp.get("counted") or {}).get("reported") != len(refs)):
        problems.append("the control's exact count does not match its positive rows")
    if state == "certified" and not problems and comp.get("bound") == "exact" and factor is None:
        result.ok("%s: %d rows, certified, limiting_factor null, bound exact" % (arm, len(refs)))
    else:
        result.bad("%s: did not certify cleanly: state=%s limiting_factor=%s bound=%s %s"
                   % (arm, state, (factor or "null")[:120], comp.get("bound"),
                      "; ".join(problems)))
    return result


def check_unproduced(suite):
    result = Result("unproduced", "the import class on an importing source reads unproduced or "
                                  "present, never absent")
    try:
        payload = suite.references()
    except Exception as error:  # noqa: BLE001
        result.unknown("find_references(blank_code) unreadable: %s" % error)
        return result
    graded = import_class_never_absent_on_importing_source(payload)
    if graded is None:
        result.unknown("this build's completeness block carries no imports class")
        return result
    honest, state = graded
    if honest:
        result.ok("imports reads %r on a source whose files import one another" % state)
    else:
        result.bad("imports reads %r on a source whose files import one another; the class was "
                   "never readable on this build, and absent says the code has no such site"
                   % state)
    return result


def check_two_reasons(suite):
    result = Result("two_reasons", "a verdict with more than one reason to refuse names every "
                                   "one of them in its limiting factor, each once")
    # At the floor, withheld rows or partial pages qualify the transport
    # independently of what the graph holds. A paged answer must say on every
    # page that the page is partial, carry the answer's own verdict on it, and
    # recover the original graph verdict afterward. Beside an import class this build cannot produce that is two
    # reasons; on a build whose linker produces the class it is one, and the
    # check says which world it graded. Either way every refusing input must
    # keep its clause: the shape this guards against kept the budget's clause
    # and dropped the rest, and one level up the CLI kept the class gap and
    # dropped a dead embedding worker.
    try:
        payload = suite.references(extra={"max_chars": BUDGET_FLOOR_CHARS})
    except Exception as error:  # noqa: BLE001
        result.unknown("find_references(blank_code, max_chars=%d) unreadable: %s"
                       % (BUDGET_FLOOR_CHARS, error))
        return result
    inputs = (((payload.get("_kin") or {}).get("verdict") or {}).get("inputs") or {})
    pages = getattr(payload, "page_observations", ())
    partial = [page for page in pages if page["page"].get("complete") is False]
    page_problems = []
    if partial:
        if len(partial) != len(pages) or len(pages) < 2:
            page_problems.append("partial reference evidence did not traverse a complete continuation sequence")
        original_verdict = (payload.get("_kin") or {}).get("verdict") or {}
        for index, page in enumerate(pages):
            verdict = page["verdict"]
            limits = (page.get("completeness") or {}).get("limits") or []
            if (page["page"].get("kind") != "references"
                    or "reference_page_partial" not in limits
                    or verdict.get("safe_to_conclude_absent") is not False
                    or page["negative"].get("safe_to_conclude_absent") is not False):
                page_problems.append("reference page %d lost its explicit partial-answer qualification" % index)
            if (verdict.get("state") != original_verdict.get("state")
                    or factor_labels(verdict.get("limiting_factor"))
                    != factor_labels(original_verdict.get("limiting_factor"))):
                page_problems.append("reference page %d does not carry the answer's own verdict" % index)
        if "reference_page_partial" in factor_labels(
                ((payload.get("_kin") or {}).get("verdict") or {}).get("limiting_factor")):
            page_problems.append("the assembled original verdict still carries a transport-only partial clause")
        if not payload.get("references"):
            page_problems.append("the page sequence reconstructed no populated reference answer")
        if inputs.get("call_sites") == "inconclusive":
            block = payload.get("call_sites") or {}
            clauses = block.get("clauses")
            original_labels = factor_labels(((payload.get("_kin") or {}).get("verdict") or {})
                                            .get("limiting_factor"))
            if (block.get("settled") is not False or not isinstance(clauses, list) or not clauses
                    or any(not isinstance(clause, str) or ": " not in clause
                           or clause.split(":", 1)[0] not in original_labels for clause in clauses)):
                page_problems.append("reassembly lost a recorded call-site limitation")
    elif inputs.get("response_budget") != "inconclusive":
        result.unknown("the response budget neither withheld nor paged rows at max_chars=%d (inputs %s), "
                       "so no response limit was exercised"
                       % (BUDGET_FLOOR_CHARS, inputs))
        return result
    labels, problems = factor_carries_every_refusing_input(payload)
    if labels is None:
        result.unknown("the bounded answer carries no verdict inputs")
        return result
    refusing = sorted(name for name, state in inputs.items() if state == "inconclusive")
    # The absence gate is checked at its mechanism rather than through the label
    # map, because its clauses are `trust_reason`'s and therefore other inputs'
    # labels. See the note above INPUT_CLAUSE_LABELS.
    gate_checked, gate_problems = factor_carries_the_absence_gates_own_clauses(payload)
    problems = list(problems) + list(gate_problems) + page_problems
    if problems:
        result.bad("%s (refusing inputs %s, factor labels %s)"
                   % ("; ".join(problems), refusing, labels))
    else:
        result.ok("%d explicitly limited page(s); %d original refusing input(s) %s and the factor carries %s%s"
                  % (len(partial), len(refusing), refusing, labels,
                     "" if gate_checked
                     else "; the absence gate carried no trust_reason, so its clauses were not "
                          "checked"))
    return result


CHECKS = [check_invariant, check_inverse, check_unproduced, check_two_reasons]
DECLARED = ("invariant", "inverse", "unproduced", "two_reasons")


# ------------------------------------------------------------------ self-test

def self_test():
    failures = []
    counted = [0]

    def expect(label, got, want):
        counted[0] += 1
        if got != want:
            failures.append("%s: got %r, wanted %r" % (label, got, want))

    # The shipped 0.5.52 shape must fail, at every field a reader acts on.
    state, problems = verdict_honours_classes(SHIPPED_0552)
    expect("the shipped shape is graded", state, "certified")
    expect("the shipped shape fails", len(problems) >= 5, True)
    expect("the shipped shape's verdict is named", any("verdict is" in p for p in problems), True)
    expect("the shipped shape's exact count is named",
           any("exact" in p for p in problems), True)

    def honest(imports="present", state="certified", factor=None, status="complete",
               bound="exact", limits=None, edge_in="certified"):
        return {
            "_kin": {
                "completeness": {"bound": bound, "status": status,
                                 "classes": {"calls": "present", "imports": imports,
                                             "references": "present"},
                                 "counted": {"exact": bound == "exact", "reported": 5},
                                 "limits": limits or []},
                "verdict": {"state": state, "limiting_factor": factor,
                            "inputs": {"edge_coverage": edge_in}},
            },
            "references": [{}] * 5,
            "relation_kinds": ["calls", "imports", "references"],
        }
    expect("Calls-only presence and certification passes",
           verdict_honours_classes(dict(honest(), relation_kinds=["calls"]))[1], [])
    fixed = honest(imports="unproduced", state="inconclusive",
                   factor="cross_file_edges_unproduced: this build produced no entity-level "
                          "imports edge for Python", status="partial", bound="at_least",
                   limits=["edge_coverage:imports_unproduced", "verdict_inconclusive"],
                   edge_in="inconclusive")
    expect("the fixed shape passes", verdict_honours_classes(fixed)[1], [])
    expect("a fixed shape whose factor names another class fails",
           len(verdict_honours_classes(dict(fixed, _kin={
               "completeness": fixed["_kin"]["completeness"],
               "verdict": dict(fixed["_kin"]["verdict"], limiting_factor="retrieval_degraded")}))[1]) >= 1,
           True)
    expect("all present but inconclusive fails (no false inconclusive)",
           len(verdict_honours_classes(honest(state="inconclusive", factor="x"))[1]), 1)
    expect("a payload with no classes is unreadable",
           verdict_honours_classes({"_kin": {"verdict": {"state": "certified"}}})[0], None)
    expect("calls-only query reads only calls",
           verdict_honours_classes(dict(honest(imports="absent"),
                                        relation_kinds=["calls"]))[1], [])

    import copy

    completion = honest(state="inconclusive", factor="reference_completion_unmeasured",
                        status="unknown", bound="at_least", limits=["verdict_inconclusive"])
    completion["_kin"]["verdict"].update({
        "absence_claim": "not_applicable", "safe_to_conclude_absent": False,
        "inputs": {"edge_coverage": "certified", "call_sites": "certified",
                   "reference_completion": "inconclusive", "absence_gate": "inconclusive"}})
    completion["call_sites"] = {"settled": True, "clauses": [], "candidates": [],
                                "candidate_count": 0}
    completion["negative"] = {
        "trust": "inconclusive", "safe_to_conclude_absent": False,
        "trust_reason": "reference_completion_unmeasured: non-call completion is unmeasured",
        "advice": "reference_completion_unmeasured"}
    expect("present non-call classes retain their exact completion gap",
           verdict_honours_classes(completion)[1], [])
    expect("present non-call classes never certify from presence alone",
           bool(verdict_honours_classes(honest())[1]), True)
    certified_noncall = copy.deepcopy(completion)
    certified_noncall["_kin"]["verdict"].update(state="certified")
    expect("certification over a recorded non-call gap fails",
           bool(verdict_honours_classes(certified_noncall)[1]), True)
    for label, mutate in (
            ("missing completion provenance", lambda p: p["_kin"]["verdict"]["inputs"].pop("reference_completion")),
            ("another limiting input", lambda p: p["_kin"]["verdict"]["inputs"].update(degradations="inconclusive")),
            ("an exact count over uncertainty", lambda p: p["_kin"]["completeness"]["counted"].update(exact=True)),
            ("an absence claim", lambda p: p["_kin"]["verdict"].update(safe_to_conclude_absent=True)),
            ("an unsettled Call census", lambda p: p["call_sites"].update(settled=False)),
            ("a Calls-only completion excuse", lambda p: p.update(relation_kinds=["calls"])),
            ("an arbitrary completion reason", lambda p: p["negative"].update(trust_reason="some gap"))):
        broken = copy.deepcopy(completion)
        mutate(broken)
        expect(label + " fails", bool(verdict_honours_classes(broken)[1]), True)

    # The shape a measured census certifies on: both imports of the focal
    # matched, nothing unmatched, no computed-name lookup.
    measured = honest()
    measured["_kin"]["verdict"]["inputs"].update(
        call_sites="certified", reference_completion="certified")
    measured["call_sites"] = {
        "settled": True, "clauses": [],
        "focal_escape": {"escape": "contained", "residue": {
            "mentions": {"measured": True, "count": 2, "matched": {"imports": 2}}}}}
    expect("a measured census that matched every read certifies",
           verdict_honours_classes(measured)[1], [])
    for label, mutate in (
            ("an unmeasured mention reading",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].update(measured=False)),
            ("no mention reading",
             lambda p: p["call_sites"]["focal_escape"]["residue"].pop("mentions")),
            ("an unmatched non-call read",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].update(
                 unmatched={"count": 1, "sites": [{"file": "pkg/a.py", "line": 3}]})),
            ("a computed-name lookup",
             lambda p: p["call_sites"]["focal_escape"]["residue"].update(
                 unkeyed_lookups={"count": 1})),
            ("member-held references left unmeasured",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].update(
                 references_unmeasured="the focal holds members")),
            ("a read carried only by an unrequested kind",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].update(
                 matched={"imports": 1, "decorates": 1})),
            ("a read neither matched nor listed unmatched",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].update(
                 count=2, matched={"imports": 1})),
            ("no total of the reads taken",
             lambda p: p["call_sites"]["focal_escape"]["residue"]["mentions"].pop("count")),
            ("an unsettled Call census", lambda p: p["call_sites"].update(settled=False)),
            ("certification not attributed to reference completion",
             lambda p: p["_kin"]["verdict"]["inputs"].update(reference_completion="not_applicable"))):
        broken = copy.deepcopy(measured)
        mutate(broken)
        expect("a certification with " + label + " fails",
               bool(verdict_honours_classes(broken)[1]), True)

    calls = honest()
    calls["relation_kinds"] = ["calls"]
    calls["references"] = [{"entity_id": "caller", "relation_kinds": ["calls"],
                            "sites": [{"line_in_entity": 1, "callee": "blank_code"}]}]
    calls["_kin"]["completeness"]["counted"]["reported"] = 1
    calls["_kin"]["verdict"]["inputs"].update(
        call_sites="certified", reference_completion="not_applicable")
    calls["call_sites"] = {"settled": True, "clauses": []}
    expect("a settled Calls-only positive certifies",
           grade_inverse(Result("t", "t"), calls, "control").status, PASS)
    silent_calls = copy.deepcopy(calls)
    silent_calls["_kin"]["verdict"]["inputs"].pop("reference_completion")
    expect("a silent non-call input is omitted on the settled Calls-only control",
           grade_inverse(Result("t", "t"), silent_calls, "control").status, PASS)
    for label, mutate in (
            ("active non-call gap on the inverse", lambda p: p["_kin"]["verdict"]["inputs"].update(reference_completion="inconclusive")),
            ("empty inverse", lambda p: p.update(references=[])),
            ("no call witness", lambda p: p["references"][0].update(sites=[])),
            ("unsettled inverse", lambda p: p["call_sites"].update(settled=False)),
            ("non-call inverse", lambda p: p.update(relation_kinds=list(CLASSES))),
            ("inexact inverse", lambda p: p["_kin"]["completeness"]["counted"].update(exact=False))):
        broken = copy.deepcopy(calls)
        mutate(broken)
        expect(label + " fails", grade_inverse(Result("t", "t"), broken, "control").status, FAIL)

    expect("unproduced is honest",
           import_class_never_absent_on_importing_source(honest(imports="unproduced")),
           (True, "unproduced"))
    expect("present is honest",
           import_class_never_absent_on_importing_source(honest()), (True, "present"))
    expect("absent is the shipped wording",
           import_class_never_absent_on_importing_source(SHIPPED_0552), (False, "absent"))
    expect("no imports class is unreadable",
           import_class_never_absent_on_importing_source({"_kin": {}}), None)

    def bounded(inputs, factor):
        return {"_kin": {"verdict": {"state": "inconclusive", "limiting_factor": factor,
                                     "inputs": inputs}}}
    two = {"absence_gate": "inconclusive", "edge_coverage": "inconclusive",
           "withheld_candidates": "certified", "degradations": "certified",
           "completeness": "inconclusive", "response_budget": "inconclusive"}
    full = ("response_bounded: the response budget withheld part of this answer; "
            "cross_file_edges_unproduced: this build produced no entity-level imports edge for "
            "Python; substrate_partial: the coverage classes this answer depended on were not "
            "all observed present (calls, imports, references)")
    expect("every refusing input has its clause",
           factor_carries_every_refusing_input(bounded(two, full))[1], [])
    # Two, not three: `absence_gate` is deliberately absent from the map, for
    # the reason written above it. Its clauses come from `trust_reason` on the
    # live path and are other inputs' labels, so a prefix entry is either
    # incomplete or unfailable. The mechanism check below covers it instead.
    expect("the factor a budget cut used to replace outright fails twice",
           len(factor_carries_every_refusing_input(bounded(
               two, "response_bounded: the response budget withheld part of this answer"))[1]), 2)

    # The absence gate, checked at its mechanism rather than by a vocabulary
    # guess. This is the shape the live suite failed on twice: a trust_reason
    # whose labels are response_bounded, answer_truncated and retrieval_degraded,
    # none of which any absence_gate prefix list contained.
    def gated(factor, reason):
        return {"negative": {"trust_reason": reason},
                "_kin": {"verdict": {"state": "inconclusive", "limiting_factor": factor,
                                     "inputs": {"absence_gate": "inconclusive"}}}}

    live_reason = ("response_bounded: the response budget withheld part of this answer; "
                   "answer_truncated: this answer stopped early; "
                   "retrieval_degraded: this query reported degradations")
    expect("the live shape passes when every trust_reason clause reaches the factor",
           factor_carries_the_absence_gates_own_clauses(
               gated(live_reason + "; counts_are_a_floor: lower bound", live_reason))[1], [])
    expect("and it is checked rather than skipped",
           factor_carries_the_absence_gates_own_clauses(
               gated(live_reason, live_reason))[0], True)
    # Envelope v2 sends the factor as codes alone, while the gate's trust_reason
    # still carries `label: text` clauses until negative.rs moves. So a label is
    # matched as a label, never as a `label:` substring of the factor.
    live_codes = "response_bounded; answer_truncated; retrieval_degraded; counts_are_a_floor"
    expect("the v2 factor, codes alone, carries every trust_reason clause",
           factor_carries_the_absence_gates_own_clauses(gated(live_codes, live_reason))[1], [])
    expect("and a v2 factor that dropped a code is still named",
           len(factor_carries_the_absence_gates_own_clauses(
               gated("response_bounded; retrieval_degraded", live_reason))[1]), 1)
    expect("a dropped trust_reason clause is named",
           [problem.split(" names ")[1].split(" and")[0]
            for problem in factor_carries_the_absence_gates_own_clauses(
                gated("response_bounded: x; retrieval_degraded: y", live_reason))[1]],
           ["'answer_truncated'"])
    expect("a clause deduplicated against another input still counts as carried",
           factor_carries_the_absence_gates_own_clauses(
               gated("retrieval_degraded: said once for two inputs",
                     "retrieval_degraded: said once for two inputs"))[1], [])
    expect("no trust_reason is UNREADABLE, not a pass",
           factor_carries_the_absence_gates_own_clauses(
               gated("response_bounded: x", None))[0], False)
    expect("an absence_gate that does not refuse is not checked",
           factor_carries_the_absence_gates_own_clauses(
               {"negative": {"trust_reason": live_reason},
                "_kin": {"verdict": {"state": "certified", "limiting_factor": None,
                                     "inputs": {"absence_gate": "certified"}}}})[0], False)

    expect("a label said twice fails",
           len(factor_carries_every_refusing_input(bounded(two, full + "; substrate_partial: again"))[1]),
           1)
    expect("a certified verdict with no clause passes",
           factor_carries_every_refusing_input({"_kin": {"verdict": {
               "state": "certified", "limiting_factor": None,
               "inputs": {"edge_coverage": "certified"}}}})[1], [])
    expect("a certified verdict carrying a clause fails",
           len(factor_carries_every_refusing_input({"_kin": {"verdict": {
               "state": "certified", "limiting_factor": "retrieval_degraded: x",
               "inputs": {"edge_coverage": "certified"}}}})[1]), 1)
    expect("no verdict inputs is unreadable", factor_carries_every_refusing_input({})[0], None)

    # The fixture waits only while readiness is pending. Every completed
    # finding, and an answer that carries no coverage, ends the wait.
    expect("pending readiness waits",
           readiness_pending({"edge_coverage": {"reference_enrichment": "unknown"}}), True)
    for finding in ("available", "no_language_server", "language_server_unusable",
                    "enrichment_disabled", "unsupported"):
        expect("completed readiness %s does not wait" % finding,
               readiness_pending({"edge_coverage": {"reference_enrichment": finding}}), False)
    expect("no coverage does not wait", readiness_pending({}), False)

    expect("every declared id has a check",
           tuple(c.__name__.replace("check_", "") for c in CHECKS), DECLARED)

    grade_cases = [(PASS, [(PASS, "a")]), (FAIL, [(PASS, "a"), (FAIL, "b")]),
                   (UNREADABLE, [(PASS, "a"), (UNREADABLE, "b")]), (UNREADABLE, [])]
    for want, entries in grade_cases:
        r = Result("t", "t")
        for status, detail in entries:
            r.asserts.append({"status": status, "detail": detail})
        if r.status != want:
            failures.append("Result.status(%s) = %s, wanted %s" % (entries, r.status, want))

    for failure in failures:
        print("SELFTEST FAIL %s" % failure)
    total = counted[0] + len(grade_cases)
    print("kin-verdict-limits-repro: self-test %d/%d cases" % (total - len(failures), total))
    return 1 if failures else 0


# ----------------------------------------------------------------------- main

def main(argv):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--kin", default=os.environ.get("KIN_BIN"))
    parser.add_argument("--daemon", default=os.environ.get("KIN_DAEMON_BIN"))
    parser.add_argument("--json", dest="json_path", default=None)
    parser.add_argument("--label", default=os.environ.get("KIN_ACCEPTANCE_LABEL"))
    parser.add_argument("--keep", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()
    if not args.kin:
        print("kin-verdict-limits-repro: no kin binary. Pass --kin or set KIN_BIN.")
        return 3
    kin = os.path.abspath(os.path.expanduser(args.kin))
    if not os.path.isfile(kin) or not os.access(kin, os.X_OK):
        print("kin-verdict-limits-repro: %s is not an executable file" % kin)
        return 3
    daemon = args.daemon and os.path.abspath(os.path.expanduser(args.daemon))
    if not daemon:
        beside = os.path.join(os.path.dirname(kin), "kin-daemon")
        daemon = beside if os.path.isfile(beside) else None

    workdir = tempfile.mkdtemp(prefix="kin-verdict-limits-repro-")
    suite = None
    try:
        suite = Suite(kin, workdir, daemon=daemon, verbose=args.verbose)
        results = []
        for check in CHECKS:
            try:
                results.append(check(suite))
            except Exception as error:  # noqa: BLE001
                result = Result(check.__name__.replace("check_", ""), "probe crashed")
                result.unknown("%s: %s" % (type(error).__name__, error))
                results.append(result)
        for result in results:
            print("CHECK %s %s %s %s" % (result.id, TICKET, result.status, result.detail))
        answered = tuple(r.id for r in results)
        if answered != DECLARED:
            print("kin-verdict-limits-repro: declared %s but %s answered" % (DECLARED, answered))
            return 3
        print("kin-verdict-limits-repro: %d of %d declared checks answered"
              % (len(answered), len(DECLARED)))
        failed = [r for r in results if r.status == FAIL]
        unreadable = [r for r in results if r.status == UNREADABLE]
        print("kin-verdict-limits-repro: %d checks, %d pass, %d FAIL, %d UNREADABLE"
              % (len(results), len(results) - len(failed) - len(unreadable),
                 len(failed), len(unreadable)))
        if args.json_path:
            payload = {"suite": "verdict_limits_repro", "ticket": TICKET, "label": args.label,
                       "kin": kin,
                       "results": [{"id": r.id, "ticket": TICKET, "title": r.title,
                                    "status": r.status, "detail": r.detail, "asserts": r.asserts}
                                   for r in results]}
            directory = os.path.dirname(os.path.abspath(args.json_path))
            if directory:
                os.makedirs(directory, exist_ok=True)
            with open(args.json_path, "w") as handle:
                json.dump(payload, handle, indent=2, sort_keys=True)
        if failed:
            return 1
        if unreadable:
            return 2
        return 0
    finally:
        if suite is not None and suite.repo:
            suite.kin_run(["daemon", "stop"], suite.repo)
        if not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
