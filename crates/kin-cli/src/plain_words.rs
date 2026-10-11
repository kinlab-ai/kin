//! Plain wording for the lines a person reads at a terminal.
//!
//! Some sentences reach the terminal from crates that also serve machines,
//! and those carry internal labels: a verdict code before a clause, a
//! call-site state spelled the way a payload spells it, or a term the
//! project uses inside its own pipeline. The machine fields keep those labels
//! because scripts and tests parse them. This module rewrites them at the
//! moment a line is printed, so `--json` output and every payload stay exactly
//! as they were.

use regex::Regex;
use std::sync::LazyLock;

/// Phrases rewritten wherever they appear, longest first.
const PHRASES: &[(&str, &str)] = &[
    (
        "the escape census found it is not, so only a name computed at run time could reach it",
        "Kin found it is not, so only a name built while the program runs could reach it",
    ),
    (
        "the escape census could not rule that out",
        "Kin could not rule that out",
    ),
    ("the escape census found it may be", "Kin found it may be"),
    ("the escape census", "Kin's check for names used as values"),
    (
        "held as a value or looked up by a computed name",
        "stored in a variable or looked up by a name built while the program runs",
    ),
    (
        "looked up by a computed name",
        "looked up by a name built while the program runs",
    ),
    (": see residue.values", ""),
    (": see residue.implicit_calls", ""),
    (": see residue.reflective", ""),
    (
        ", so they are counted in focal_escape.residue.unkeyed_lookups rather than listed as sites to check",
        ", so they are counted but not listed as places to check",
    ),
    (
        "lookups by a name computed at run time",
        "lookups by a name built while the program runs",
    ),
    (", each listed in focal_escape.residue", ""),
    (
        ", so they are counted under `call_sites.value_calls` rather than listed",
        ", so they are counted but not listed",
    ),
    ("every one in the call_sites block", "every one in the call sites section"),
    ("name_unique reference", "reference to the only definition that name can reach"),
    ("Unproven call sites that could call ", "Call sites not confirmed, that could call "),
    (" proven_external -> ", ": calls a library function, "),
    (" proven_target -> ", ": confirmed, calls "),
    (" proven_static -> ", ": confirmed by the code alone, calls "),
    (" proven_declaration -> ", ": confirmed, calls the declaration "),
    (" proven_outside", ": calls code outside this repository"),
    (" unresolved (outside_the_graph)", ": not resolved (the target is outside this repository)"),
    ("; unresolved)", "; not resolved)"),
    (" (outside_the_graph)", " (the target is outside this repository)"),
    (" (no_answer)", " (the language server gave no answer)"),
    (" (server_refused)", " (the language server refused)"),
    (" (answers_disagree)", " (the language server's answers disagreed)"),
    (" (callee_not_placeable)", " (Kin could not place the called name)"),
    (" (reflective)", " (looked up by name at run time)"),
    ("the focal's own body", "this entity's own body"),
    ("  not settled: ", "  Not confirmed: "),
    ("name_unique", "only definition with that name"),
    (
        "so its parse coverage was not read",
        "so which files it can read was not checked",
    ),
    (
        "so whether its opens fold history was not read",
        "so whether it can reopen quickly was not checked",
    ),
    (
        "semantic_locate routing: fused locate pipeline; ",
        "search combines several signals; ",
    ),
    // The verdict's clause names the MCP tool it was computed for, and a
    // person at a shell ran `kin refs`.
    (
        "find_references did not report cross-repo authority",
        "`kin refs` did not report whether it checked other repositories",
    ),
    ("entity fusion: ", "name matching: "),
    ("lexical parity floor: ", "word matching floor: "),
    ("cross-encoder rerank: ", "second-model re-ranking: "),
    (
        "installs unattended through the gated executor",
        "installs unattended after its safety checks",
    ),
    ("private authority file(s)", "private registry file(s)"),
    (
        "language-server enrichment is not suspended for this store",
        "linking references across files is not paused for this store",
    ),
    ("the relation census cannot be read", "relationship counts cannot be read"),
    ("graph embedding coverage", "search-by-meaning coverage"),
    ("relation-graph completeness", "cross-file link completeness"),
    (
        "reach the graph through admission",
        "reach Kin's graph when Kin next reads the working files",
    ),
    ("no download is owed", "no download is needed"),
    (
        "this store records hydration semantics version ",
        "this store was created with format version ",
    ),
    (
        " at creation, matching the version this build derives",
        ", which matches this Kin build",
    ),
    (
        "cross-file reference and override edges",
        "references and overrides between files",
    ),
    ("cross-file reference edges", "references between files"),
    ("the sweep enriched ", "the sweep checked "),
    ("enriched none of them", "checked none of them"),
    ("Code graph: absent (", "Code graph: empty ("),
    ("Code graph: partial (", "Code graph: partly linked ("),
    ("Code graph: present (", "Code graph: built ("),
    ("completion not attested", "not confirmed that background analysis finished"),
    ("Semantic coverage: ", "Search by meaning: "),
    (" entities embedded", " entities can be searched by meaning"),
    (
        "the ranking ran on lexical and graph signals alone",
        "results were ranked by word matches and code structure only",
    ),
    ("; owed_enrichment)", "; cross-file links not finished)"),
    ("; owed_derivation)", "; not analyzed yet)"),
    ("; unproven_no_resolver)", "; no language server can link it here)"),
    ("Pending: ", "Still linking: "),
    ("  Limit: ", "  Not complete: "),
    (
        " more pending entities; use --json for the bounded list.",
        " more entities are still being linked; --json lists them.",
    ),
    ("Call-site enrichment is incomplete.", "Some call sites are not fully linked yet."),
    (
        "A row names its entity by id; the path after `projection:` is the file that entity is projected into, not an address.",
        "Each row gives the entity's id and, after `projection:`, the file it is written to.",
    ),
];

/// Codes that lead a clause, as `code: text`.
static CODE_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    let mut codes: Vec<&str> = kin_mcp::verdict::CLAUSE_CODES
        .iter()
        .map(|entry| entry.code)
        .collect();
    codes.extend([
        kin_mcp::verdict::CERTIFIED_EXCEPT,
        kin_model::call_site_reading::NAMED_LEFTOVERS,
        kin_model::call_site_reading::VALUE_CALLS_UNMEASURED,
    ]);
    // Longest first so a code that begins another one is not cut short.
    codes.sort_by_key(|code| std::cmp::Reverse(code.len()));
    let alternatives = codes
        .iter()
        .map(|code| regex::escape(code))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"\b(?:{alternatives}): ")).expect("clause code pattern")
});

/// The sentence impact and review attach for each kind of call site that is
/// not linked, one per verdict code, joined by `; `.
static UNSETTLED_EVIDENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(\d+) entit(?:y|ies) in this impact's reach or among its possible inbound callers across the selected graph (?:has|have|holds|hold) (.+?)\. Impact counts are a lower bound and review risk may change\. This conservatively bounds the answer\. It does not prove that every listed entity reaches the changed code\.(?:; )?",
    )
    .expect("unsettled evidence pattern")
});

/// What one unsettled-evidence clause says its entities hold, in plain words.
fn unsettled_evidence_words(held: &str) -> &'static str {
    if held.contains("still owed") || held.starts_with("unsettled call-site evidence") {
        "still have call sites Kin has not finished linking"
    } else if held.contains("value binding") {
        "have calls that go through a variable whose value Kin could not pin down"
    } else if held.contains("no build of the repository compiles") {
        "have calls in files no build compiles, so no language server checked them"
    } else if held.contains("timed out") {
        "have calls the language server failed on"
    } else if held.contains("without proving a target") {
        "have calls that could not be matched to what they call"
    } else if held.contains("no resolver can prove") {
        "have calls no language server can check on this machine"
    } else if held.contains("proof context") {
        "have calls that were checked against an older build setup"
    } else {
        "have calls Kin could not link"
    }
}

/// The sentence an empty search carries when no kind of code was measured.
static COVERAGE_UNMEASURED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"no coverage class was measured for ([^,]+), so nothing established what the extractor admitted for it, and an empty result cannot be separated from a declaration the extractor never admitted as an entity or from a file the graph does not hold yet",
    )
    .expect("coverage pattern")
});

/// Sentences `kin init` and `kin daemon sweep` print, each with the plain
/// sentence that replaces it.
static SWEEP_RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    let rules: &[(&str, &str)] = &[
        (
            r"^(\s*)cross-file enrichment complete \((\d+)/(\d+) files\)",
            "${1}Linked references across files: all $3 files checked.",
        ),
        (
            r"^(\s*)cross-file enrichment covered (\d+) of (\d+) files:",
            "${1}Linked references across files for $2 of $3 files. The rest could not be checked:",
        ),
        (
            r"^(\s*)cross-file enrichment reached (\d+) of (\d+) files and still owes (\d+) files?:",
            "${1}Linked references across files for $2 of $3 files. $4 file(s) still need another try:",
        ),
        (
            r"^(\s*)cross-file enrichment ended having enriched (\d+) of (\d+) files, leaving (\d+) languages? unserved:",
            "${1}Linked references across files for $2 of $3 files. $4 could not be checked:",
        ),
        (
            r"^(\s*)(\S.*): (\d+) (files?) not enriched, because ",
            "${1}${2}: $3 $4 not checked, because ",
        ),
        (
            r"^note: cross-file enrichment finished without enriching any of the (\d+) files it walked \((\d+) blocked\); reference and import edges will be missing until it can run, and `.kin/daemon.log` records what stopped it",
            "note: Kin could not link references across files. None of the $1 files were checked ($2 blocked), so calls and imports between files will be missing until it can. `.kin/daemon.log` records what stopped it",
        ),
        (
            r"^(\s*)(\d+) files? (?:is|are) owed: (?:its|their) language server left questions about (?:it|them) unanswered, so the next sweep after a backoff asks again, and `kin daemon sweep` asks at once",
            "${1}$2 file(s) still need another try: the language server did not answer every question about them. Kin retries after a short wait, or run `kin daemon sweep` to retry now",
        ),
        (
            r"^Enriching cross-file references \(language server\)\.\.\.",
            "Linking references across files, using the language server...",
        ),
        (
            r"^(\s*)Initialized Kin repository authority at ",
            "${1}Set up Kin in ",
        ),
        (r"^(\s*)Authority: repository-v6 \(graph-owned\)", "${1}Storage: Kin's own store (format v6)"),
        (
            r"^(\s*)History: unborn \(no synthetic commit\)",
            "${1}History: none yet (no commits)",
        ),
        (r"^(\s*)Workspace: empty exact tree", "${1}Workspace: empty"),
        (
            r"^(\s*)Graph reopen: Persisted the workspace base graph section at \w+ \(authority generation (\d+)\)\.",
            "${1}Fast reopen: saved a copy of the code graph so the next open is quick (store revision $2).",
        ),
        (
            r"^First embedding pass: (\d+) still queued and automatic indexing is off; `kin embed` runs it, so this command did not wait for them\.",
            "Search by meaning: $1 entities are not indexed yet and automatic indexing is off. Run `kin embed` to index them; this command did not wait.",
        ),
        (r"^(\s*)Default ref: ", "${1}Default branch: "),
        (r"^(\s*)Authority generation: ", "${1}Store revision: "),
        (r"^(\s*)Workspace generation: ", "${1}Workspace revision: "),
        (r"^(\s*)Workspace head: symbolic ", "${1}Workspace is on: "),
        (
            r"^(\s*)Graph reopen: No workspace base graph section exists yet because this workspace is unborn \(authority generation (\d+)\)\.",
            "${1}Fast reopen: not available yet, because this workspace has no commits (store revision $2).",
        ),
        (
            r"^(\s*)Imported: exact reachable Git history, refs, raw objects, workspace, and admission policy",
            "${1}Imported: the full Git history, branches and tags, Git objects, working files and file rules",
        ),
        (
            r"^(\s*)Semantic enrichment: (absent|partial|present) \((\d+) entities, (\d+) relations, (\d+) changes in durable authority generation (\d+); ([^)]*)\)",
            "${1}Code graph: $2 ($3 entities, $4 relationships, $5 changes saved at store revision $6; $7)",
        ),
        (
            r"^(\s*)No semantic entities were extracted\. If this repository's languages are not ones Kin parses, that is expected, and `kin languages` lists the ones it does; content and history are still under repository authority either way\.",
            "${1}Kin found no functions, classes or other code entities. If this repository's languages are not ones Kin reads, that is expected; `kin languages` lists the ones it does. Your files and history are stored either way.",
        ),
        (r"^(\s*)Embedding model: ", "${1}Search-by-meaning model: "),
        (
            r"^(\s*)Uncommitted worktree state: (\d+) path\(s\) differ from the committed state that was admitted",
            "${1}Uncommitted changes: $2 path(s) differ from the last commit Kin saved",
        ),
    ];
    rules
        .iter()
        .map(|(pattern, plain)| (Regex::new(pattern).expect("sweep rule"), *plain))
        .collect()
});

/// A call-site row whose target could not be proven, for the reasons the
/// phrase table does not name.
static UNRESOLVED_ROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*\+\d+ `[^`]*`) unresolved\b").expect("row pattern"));

/// Bracketed code tags, `[code:detail]`, that a payload adds to a sentence.
static CODE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r" ?\[[a-z]+(?:_[a-z]+)+(?::[a-z_]+)?\]").expect("tag pattern"));

/// One line of terminal text with internal labels replaced by plain words.
///
/// Lines that carry none of them come back unchanged.
pub fn line(text: &str) -> String {
    let mut out = CODE_PREFIX.replace_all(text, "").into_owned();
    for (pattern, plain) in SWEEP_RULES.iter() {
        if pattern.is_match(&out) {
            out = pattern.replace(&out, *plain).into_owned();
            break;
        }
    }
    out = UNRESOLVED_ROW
        .replace(&out, "$1: not resolved")
        .into_owned();
    out = COVERAGE_UNMEASURED
        .replace_all(
            &out,
            "Kin has not checked which $1 declarations it can read, so an empty result could \
             mean nothing matches, that Kin could not read a declaration, or that a file is not \
             in the graph yet",
        )
        .into_owned();
    if UNSETTLED_EVIDENCE.is_match(&out) {
        let mut owed = false;
        out = UNSETTLED_EVIDENCE
            .replace_all(&out, |found: &regex::Captures<'_>| {
                let words = unsettled_evidence_words(&found[2]);
                owed |= words.contains("not finished");
                format!(
                    "{} entities this change could reach, or that could call into it, {words}. ",
                    &found[1]
                )
            })
            .into_owned();
        out = out.trim_end().to_string();
        out.push_str(if owed {
            " Treat these counts as a minimum; the risk may change once linking finishes. The \
             list can include entities that do not actually reach the changed code."
        } else {
            " Treat these counts as a minimum. The list can include entities that do not \
             actually reach the changed code."
        });
    }
    for (from, to) in PHRASES {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    CODE_TAG.replace_all(&out, "").into_owned()
}

/// The name of a `kin doctor` check, in words a person knows.
pub fn doctor_label(label: &str) -> String {
    match label {
        "Relation census" => "Relationship counts",
        "Hydration semantics" => "Store format version",
        "Registry authority" => "Registry files",
        "Enrichment sweeps" => "Cross-file linking",
        "Reference edge coverage" => "Cross-file references",
        "Semantic query readiness" => "Search readiness",
        "Graph section" => "Fast reopen data",
        "Parse coverage" => "Files read",
        "Supervisor protocol" => "Daemon supervisor",
        "Projection in force" => "File view in use",
        "VFS projection" => "Virtual file view",
        "Retrieval quality profile" => "Search quality settings",
        other => other,
    }
    .to_string()
}

/// The lines of an answer that also carries source code, with only the named
/// sections (and the text before the first section) reworded. A code body is
/// the repository's own text, so no rewrite may touch it.
pub fn in_sections(lines: &[String], sections: &[&str]) -> Vec<String> {
    let mut current = String::new();
    let mut out = Vec::with_capacity(lines.len());
    for entry in lines {
        // A section can open in the middle of a multi-line entry.
        let mut pieces = Vec::new();
        for piece in entry.split('\n') {
            let trimmed = piece.trim();
            if trimmed.len() > 8 && trimmed.starts_with("--- ") && trimmed.ends_with(" ---") {
                current = trimmed[4..trimmed.len() - 4].to_string();
            }
            let reword = current.is_empty() || sections.iter().any(|name| *name == current);
            pieces.push(if reword {
                line(piece)
            } else {
                piece.to_string()
            });
        }
        out.push(pieces.join("\n"));
    }
    out
}

/// The lines of a trace, whose bodies are source code: the notes, the section
/// lead-ins and the closing tally are reworded, the code is not.
pub fn trace_lines(lines: &[String]) -> Vec<String> {
    static COUNTS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^Counts: contracts=(\d+) tests=(\d+) work_items=(\d+) annotations=(\d+)$")
            .expect("counts pattern")
    });
    static LEAD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^\[(<- )?(Calls|References|Imports)( ->)?\]").expect("lead"));
    static HOPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+)-hop\] ").expect("hops"));
    let reworded = in_sections(lines, &["Follow-ups"]);
    reworded
        .iter()
        .map(|entry| {
            entry
                .split('\n')
                .map(|piece| {
                    if let Some(found) = COUNTS.captures(piece) {
                        format!(
                            "Also linked to this entity: {} contracts, {} tests, {} work items, {} annotations.",
                            &found[1], &found[2], &found[3], &found[4]
                        )
                    } else if let Some(found) = LEAD.captures(piece) {
                        let verb = match (&found[2], found.get(1).is_some()) {
                            ("Calls", false) => "Calls:",
                            ("Calls", true) => "Called by:",
                            ("References", false) => "Refers to:",
                            ("References", true) => "Referred to by:",
                            ("Imports", false) => "Imports:",
                            _ => "Imported by:",
                        };
                        format!("{verb}{}", &piece[found[0].len()..])
                    } else if let Some(found) = HOPS.captures(piece) {
                        let steps: u32 = found[1].parse().unwrap_or(0);
                        format!("[{steps} steps away] {}", &piece[found[0].len()..])
                    } else {
                        piece.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

/// Every line of a block of terminal text.
pub fn text(block: &str) -> String {
    let mut out = block.lines().map(line).collect::<Vec<_>>().join("\n");
    if block.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verdict_code_before_a_clause_is_dropped() {
        assert_eq!(
            line("  not settled: call_sites_unresolved: 1 of the 3 call sites got no answer"),
            "  Not confirmed: 1 of the 3 call sites got no answer"
        );
    }

    #[test]
    fn the_impact_limit_reads_as_what_is_missing() {
        let said = line(
            "  Limit: enrichment_incomplete: 652 entities in this impact's reach or among its \
             possible inbound callers across the selected graph have call-site evidence the \
             graph has not settled yet because derivation or enrichment is still owed, and a \
             sweep will still reach it. Impact counts are a lower bound and review risk may \
             change. This conservatively bounds the answer. It does not prove that every listed \
             entity reaches the changed code.",
        );
        assert!(
            said.starts_with("  Not complete: 652 entities this change could reach"),
            "{said}"
        );
        assert!(said.contains("not finished linking"), "{said}");
        assert!(
            !said.contains("enrichment") && !said.contains("unsettled"),
            "{said}"
        );
        // Calls a resolver already answered without a target are not owed
        // work, so their line never says linking is unfinished.
        let answered = line(
            "  Limit: call_sites_unresolved: 20 entities in this impact's reach or among its \
             possible inbound callers across the selected graph hold call sites the resolver \
             answered without proving a target. Impact counts are a lower bound and review risk \
             may change. This conservatively bounds the answer. It does not prove that every \
             listed entity reaches the changed code.",
        );
        assert_eq!(
            answered,
            "  Not complete: 20 entities this change could reach, or that could call into it, \
             have calls that could not be matched to what they call. Treat these counts as a \
             minimum. The list can include entities that do not actually reach the changed code."
        );
        assert_eq!(
            line("    Pending: setup [id] (projection: docs/conf.py; owed_enrichment)"),
            "    Still linking: setup [id] (projection: docs/conf.py; cross-file links not finished)"
        );
    }

    #[test]
    fn doctor_names_read_plainly() {
        assert_eq!(doctor_label("Relation census"), "Relationship counts");
        assert_eq!(doctor_label("Memory floor"), "Memory floor");
        assert_eq!(
            line("this store records hydration semantics version 32 at creation, matching the version this build derives"),
            "this store was created with format version 32, which matches this Kin build"
        );
    }

    #[test]
    fn init_summary_lines_read_plainly() {
        assert_eq!(
            line("  Semantic enrichment: absent (0 entities, 0 relations, 0 changes in durable authority generation 1; completion not attested)"),
            "  Code graph: empty (0 entities, 0 relationships, 0 changes saved at store revision 1; not confirmed that background analysis finished)"
        );
        assert_eq!(
            line("  cross-file enrichment complete (83/83 files)"),
            "  Linked references across files: all 83 files checked."
        );
    }

    #[test]
    fn the_reopen_and_index_lines_read_plainly() {
        assert_eq!(
            line("  Graph reopen: Persisted the workspace base graph section at abc123 (authority generation 1)."),
            "  Fast reopen: saved a copy of the code graph so the next open is quick (store revision 1)."
        );
        assert!(line("First embedding pass: 365 still queued and automatic indexing is off; `kin embed` runs it, so this command did not wait for them.")
            .starts_with("Search by meaning: 365 entities are not indexed yet"));
    }

    #[test]
    fn only_the_named_sections_are_reworded() {
        let lines: Vec<String> = [
            "Header not_here",
            "--- Context Pack ---",
            "x = 1  # unresolved",
            "--- Call sites ---",
            "  +1 `f` unresolved (outside_the_graph)",
        ]
        .iter()
        .map(|entry| entry.to_string())
        .collect();
        let said = in_sections(&lines, &["Call sites"]);
        assert_eq!(said[2], "x = 1  # unresolved");
        assert!(said[4].contains("not resolved"), "{}", said[4]);
    }

    #[test]
    fn a_plain_line_is_unchanged() {
        let plain = "Found 6 results: render_template (Function, python)";
        assert_eq!(line(plain), plain);
    }

    #[test]
    fn code_like_source_text_is_not_touched() {
        let source = "    call_sites_unresolved_count: int = 0";
        assert_eq!(line(source), source);
    }

    #[test]
    fn a_call_site_row_reads_in_words() {
        assert_eq!(
            line("  +10 `_get` unresolved (outside_the_graph)"),
            "  +10 `_get`: not resolved (the target is outside this repository)"
        );
        assert_eq!(
            line("  src/other.rs still has an unresolved prior binding"),
            "  src/other.rs still has an unresolved prior binding"
        );
    }

    #[test]
    fn no_internal_word_survives_the_unkeyed_lookup_clause() {
        let said = line(&format!(
            "{}: 14 lookups by a name computed at run time could reach the focal as they could \
             reach anything their receiver holds, and none spells a call name, so they are \
             counted in focal_escape.residue.unkeyed_lookups rather than listed as sites to check",
            kin_model::call_site_reading::UNKEYED_LOOKUPS_COUNTED
        ));
        for internal in ["unkeyed", "residue", "focal_escape"] {
            assert!(!said.contains(internal), "{internal}: {said}");
        }
        assert!(
            said.starts_with("14 lookups by a name built while the program runs"),
            "{said}"
        );
        assert!(
            said.ends_with("counted but not listed as places to check"),
            "{said}"
        );
    }

    #[test]
    fn no_internal_word_survives_the_value_call_sentence() {
        let said = line(
            "  978 more could reach it only if url_for is held as a value or looked up by a \
             computed name, and the escape census found it may be (a use of its name holds it \
             as a value: see residue.values).",
        );
        assert!(!said.contains("census"), "{said}");
        assert!(!said.contains("residue"), "{said}");
        assert!(said.contains("stored in a variable"), "{said}");
    }
}
