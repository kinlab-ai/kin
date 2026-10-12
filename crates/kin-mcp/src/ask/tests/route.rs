// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;

#[derive(Deserialize)]
struct DevelopmentCorpus {
    routes: Vec<DevelopmentRow>,
}

#[derive(Deserialize)]
struct DevelopmentRow {
    source: String,
    question: String,
    intent: Intent,
    target: String,
    include_tests: bool,
    candidates: Vec<String>,
    targets: Vec<String>,
    qualifier: Option<String>,
    paths: Vec<String>,
    scope_words: Option<String>,
    declares: Option<DevelopmentDeclaration>,
    /// For a follow-up, the intent a reader labelled it with, and the ones
    /// also accepted.
    #[serde(default)]
    labelled_intent: Option<Intent>,
    #[serde(default)]
    labelled_alternates: Vec<Intent>,
}

#[derive(Deserialize)]
struct DevelopmentDeclaration {
    function: String,
    modules: Vec<String>,
    known: Vec<String>,
    asked_other: bool,
}

fn development_rows() -> Vec<DevelopmentRow> {
    serde_json::from_str::<DevelopmentCorpus>(include_str!("routing-development.json"))
        .expect("the preserved development corpus has the expected shape")
        .routes
}

#[test]
fn rules_match_every_preserved_development_route_given_its_historical_candidates() {
    // This corpus was used to develop the rules. It tests deterministic port
    // parity, never fresh accuracy. Hold extraction fixed here so a routing
    // change cannot hide behind a changed candidate set or silently retune it.
    let rows = development_rows();
    assert_eq!(rows.len(), 121);
    for (index, row) in rows.iter().enumerate() {
        let held = Extracted {
            candidates: row.candidates.clone(),
            qualifier: row.qualifier.clone(),
            // The router forwards this already-extracted list verbatim; its
            // source/directory classification belongs to extractor tests.
            file_path: row.paths.first().cloned(),
            directory_path: row.paths.get(1).cloned(),
        };
        let got = plan_with_extracted(&row.question, held);
        let label = format!("row {index} ({}): {}", row.source, row.question);
        assert_eq!(got.intent, row.intent, "{label}");
        assert_eq!(got.target.as_deref(), Some(row.target.as_str()), "{label}");
        assert_eq!(got.targets, row.targets, "{label}");
        assert_eq!(got.include_tests, row.include_tests, "{label}");
        assert_eq!(got.scope_words, row.scope_words, "{label}");
        assert_eq!(got.paths_not_applied, row.paths, "{label}");
        match (&got.declares, &row.declares) {
            (Some(got), Some(want)) => {
                assert_eq!(got.function, want.function, "{label}");
                assert_eq!(got.modules, want.modules, "{label}");
                assert_eq!(got.known, want.known, "{label}");
                assert_eq!(got.asked_other, want.asked_other, "{label}");
            }
            (None, None) => {}
            _ => panic!("declaration metadata differs: {label}"),
        }
    }
}

#[test]
fn current_extraction_keeps_development_intents_and_test_flags() {
    // Separate from target parity: the adapter-driven extractor intentionally
    // accepts more language spellings and preserves textual candidate order.
    for (index, row) in development_rows().iter().enumerate() {
        let got = plan(&row.question);
        assert_eq!(got.intent, row.intent, "row {index}: {}", row.question);
        assert_eq!(
            got.include_tests, row.include_tests,
            "row {index}: {}",
            row.question
        );
    }
}

#[test]
fn current_extraction_keeps_every_first_question_target() {
    // The first questions are the ones whose answers were measured, so the
    // adapter-driven extractor must pick the same targets for them, in the
    // same order, as the rules they were measured under.
    let rows = development_rows();
    let first: Vec<&DevelopmentRow> = rows
        .iter()
        .filter(|row| row.source == "first question")
        .collect();
    assert_eq!(first.len(), 24);
    for row in first {
        let got = plan(&row.question);
        assert_eq!(
            got.target.as_deref(),
            Some(row.target.as_str()),
            "{}",
            row.question
        );
        assert_eq!(got.targets, row.targets, "{}", row.question);
        assert_eq!(got.qualifier, row.qualifier, "{}", row.question);
        assert_eq!(got.scope_words, row.scope_words, "{}", row.question);
    }
}

#[test]
#[ignore = "measurement, run by hand"]
fn measure_current_extraction_target_parity() {
    let rows = development_rows();
    let mut differ = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let got = plan(&row.question);
        if got.target.as_deref() != Some(row.target.as_str()) || got.targets != row.targets {
            differ.push(format!(
                "row {index} ({}): want {:?} {:?}, got {:?} {:?}: {}",
                row.source, row.target, row.targets, got.target, got.targets, row.question
            ));
        }
    }
    println!(
        "target parity with the current extractor: {} of {}",
        rows.len() - differ.len(),
        rows.len()
    );
    for line in differ {
        println!("{line}");
    }
}

#[test]
fn follow_up_routing_holds_its_measured_baseline() {
    // 67 follow-ups agents asked, each labelled by a reader. The keyword
    // rules these were ported from route 58 to their label and 63 to their
    // label or an accepted alternate; the port must not do worse.
    let rows = development_rows();
    let labelled: Vec<&DevelopmentRow> = rows
        .iter()
        .filter(|row| row.labelled_intent.is_some())
        .collect();
    assert_eq!(labelled.len(), 67);
    let mut strict = 0;
    let mut accepted = 0;
    for row in &labelled {
        let got = plan(&row.question).intent;
        let label = row.labelled_intent.unwrap();
        strict += usize::from(got == label);
        accepted += usize::from(got == label || row.labelled_alternates.contains(&got));
    }
    println!("follow-ups routed to their label: {strict} of 67; with alternates: {accepted} of 67");
    assert!(strict >= 58, "{strict} of 67 strict");
    assert!(accepted >= 63, "{accepted} of 67 with alternates");
}

#[test]
fn cue_words_inside_a_name_never_route() {
    for question in [
        "Where is `unused_helper` declared?",
        "Where is unused_helper declared?",
    ] {
        let got = plan(question);
        assert_eq!(got.intent, Intent::WhereIsIt, "{question}");
        assert_eq!(got.target.as_deref(), Some("unused_helper"), "{question}");
        assert_eq!(got.cue.as_deref(), Some("Where"), "{question}");
    }
    // A cue inside a name does not route, and the same word as a word does.
    assert_eq!(
        plan("Who calls `chain_handlers`?").intent,
        Intent::CallersDirect
    );
    assert_eq!(
        plan("Who calls chainHandlers?").intent,
        Intent::CallersDirect
    );
    assert_eq!(
        plan("What can end up calling `chain_handlers`?").intent,
        Intent::CallersTransitive
    );
    assert_eq!(
        plan("What can reach render?").intent,
        Intent::CallersTransitive
    );
    assert_eq!(
        plan("Is `delete_user` used anywhere?").cue.as_deref(),
        Some("used anywhere")
    );
}

#[test]
#[ignore = "measurement, run by hand"]
fn measure_routes_that_differ_from_the_fixture() {
    for (index, row) in development_rows().iter().enumerate() {
        let got = plan(&row.question).intent;
        if got != row.intent {
            println!(
                "row {index} ({}): fixture {:?}, now {:?}, label {:?}: {}",
                row.source, row.intent, got, row.labelled_intent, row.question
            );
        }
    }
}

#[test]
fn cue_quotes_the_winning_rule_and_preserves_case() {
    let direct = plan("WHO CALLS `save_value`?");
    assert_eq!(direct.intent, Intent::CallersDirect);
    assert_eq!(direct.cue.as_deref(), Some("CALLS"));
    let transitive = plan("Who calls `save_value` indirectly, and does Store declare it?");
    assert_eq!(transitive.intent, Intent::CallersTransitive);
    assert_eq!(transitive.cue.as_deref(), Some("indirect"));
}

#[test]
fn asking_for_more_implementations_does_not_override_an_explicit_source_request() {
    let more = plan("Is there another implementation of `load_value`?");
    assert_eq!(more.intent, Intent::Declares);
    assert_eq!(more.target.as_deref(), Some("load_value"));
    assert_eq!(more.cue.as_deref(), Some("implementation"));
    for question in [
        "Show another implementation of `load_value`.",
        "Is there another implementation of `load_value`? Print its code.",
        "Show the body of `load_value`.",
    ] {
        assert_eq!(plan(question).intent, Intent::ShowSource, "{question}");
    }
}

#[test]
fn declaration_names_follow_adapter_separators_and_are_only_hints() {
    let got = plan("Does graph-store define `Store::get` besides OtherStore?");
    assert_eq!(got.intent, Intent::Declares);
    assert_eq!(got.target.as_deref(), Some("get"));
    let declares = got.declares.unwrap();
    assert!(declares.modules.contains(&"Store".to_string()));
    assert_eq!(declares.packages, ["graph-store"]);
    assert!(declares.asked_other);
}

#[test]
fn test_exclusion_preserves_boundaries_and_the_exact_character_window() {
    for question in [
        "skip tests",
        "excluding __tests__",
        "not from tests",
        "non-test code",
        "leave out the tests",
    ] {
        assert!(test_exclusion(question).is_some(), "{question}");
    }
    for question in [
        "tests included",
        "skip contestants",
        "skip testable code",
        "excluding__tests__",
        "skip. tests",
    ] {
        assert!(test_exclusion(question).is_none(), "{question}");
    }
    assert!(test_exclusion(&format!("skip{}tests", " ".repeat(40))).is_some());
    assert!(test_exclusion(&format!("skip{}tests", " ".repeat(41))).is_none());
    assert!(test_exclusion(&format!("skip {} tests", "界".repeat(38))).is_some());
    let got = plan("Who calls `load_value`, excluding __tests__?");
    assert!(!got.include_tests);
    assert_eq!(got.test_cue.as_deref(), Some("excluding __tests"));
}

#[test]
fn enumerated_targets_keep_order_without_pulling_in_unrelated_names() {
    let got = plan("Who calls `Store.get`, `Cache.get`, or `Queue.get`?");
    assert_eq!(got.targets, ["Store.get", "Cache.get", "Queue.get"]);
    let separate = plan("Who calls `Store.get`? Is `Queue.get` its replacement?");
    assert_eq!(separate.targets, ["Store.get"]);
}

#[test]
fn paths_stay_unapplied_even_when_they_contain_package_or_scope_words() {
    let got = plan("Who calls `Store.get` inside packages/graph-store/src?");
    assert_eq!(got.paths_not_applied, ["packages/graph-store/src"]);
    assert_eq!(got.scope_words, None);
    assert_eq!(got.qualifier, None);
    let declared =
        plan("Does `load_value` exist in packages/graph-store/src, or is it defined elsewhere?");
    assert_eq!(declared.intent, Intent::Declares);
    assert!(declared.declares.unwrap().packages.is_empty());
}

#[test]
fn an_explicit_module_scope_is_reported_without_being_resolved() {
    let got = plan("Who calls `Store.get` inside the storage package?");
    assert_eq!(got.scope_words.as_deref(), Some("storage"));
    assert!(got.paths_not_applied.is_empty());
}

#[test]
fn unknown_or_nameless_questions_do_not_invent_a_target_or_cue() {
    let empty = plan("");
    assert_eq!(empty.intent, Intent::Other);
    assert_eq!(empty.target, None);
    assert_eq!(empty.cue, None);
    assert!(empty.targets.is_empty());
    let no_name = plan("Who calls?");
    assert_eq!(no_name.intent, Intent::CallersDirect);
    assert_eq!(no_name.target, None);
    assert!(no_name.targets.is_empty());
    assert_eq!(first_name_at("abc", ""), None);
}

#[test]
fn test_exclusion_keeps_python_case_insensitive_ascii_range_boundaries() {
    // The preserved source uses re.I with [A-Za-z] lookarounds. Its four
    // non-ASCII case equivalents are letters at either side of "tests".
    for letter in ['K', 'ſ', 'İ', 'ı', 'A', 'z'] {
        for question in [format!("skip {letter}tests"), format!("skip tests{letter}")] {
            assert!(test_exclusion(&question).is_none(), "{question}");
        }
    }
    for nonletter in ['_', '7', '界', '-'] {
        for question in [
            format!("skip {nonletter}tests"),
            format!("skip tests{nonletter}"),
        ] {
            assert!(test_exclusion(&question).is_some(), "{question}");
        }
    }
}

#[test]
fn scope_hints_distinguish_sentence_punctuation_from_qualified_continuations() {
    for punctuation in ["", ".", "?", "!", ", please."] {
        let question = format!("Who calls `load_value` inside storage{punctuation}");
        assert_eq!(
            plan(&question).scope_words.as_deref(),
            Some("storage"),
            "{question}"
        );
    }
    for continuation in [".layer", ".Δelta", "::layer", "/src", "\\src"] {
        let question = format!("Who calls `load_value` inside storage{continuation}?");
        assert_eq!(plan(&question).scope_words, None, "{question}");
    }
}

#[test]
fn scope_hints_do_not_reinterpret_paths_or_cross_token_boundaries() {
    for path in ["storage.@cache/src", "storage../src", "storage.-cache/src"] {
        let question = format!("Who calls `load_value` inside {path}?");
        let got = plan(&question);
        assert_eq!(got.scope_words, None, "{question}");
        assert_eq!(got.paths_not_applied, [path], "{question}");
    }
    for question in [
        "Who calls `load_value` inside storage. See pkg/src for context.",
        "Who calls `load_value` inside storage (see pkg/src).",
        "Who calls `load_value` inside `storage`. See pkg/src for context.",
    ] {
        assert_eq!(
            plan(question).scope_words.as_deref(),
            Some("storage"),
            "{question}"
        );
    }
    assert_eq!(
        scope_words("Who calls `load_value` inside storage.@cache\\src?"),
        None
    );
}
