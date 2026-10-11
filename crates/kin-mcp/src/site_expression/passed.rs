// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a value handed to a call as an argument goes.
//!
//! A census lists a use such as `runWithFiberInDEV(fiber, commitTextUpdate)`
//! as a value passed on: the function that holds the line hands the focal to
//! other code, and that code may call it. A reader then has two questions the
//! line alone does not answer when the call is written over several lines:
//! which code it was handed to, and whether that code calls it. This reads
//! both from text and records the answer already holds.
//!
//! The call is found in the passing function's own text: the innermost
//! argument list the value sits in, and the callee written before it. The
//! reading is checked against itself before it is used: the argument at the
//! position it found has to be the value, spelled as the site spells it.
//! Anything else (a keyword argument, a value inside a larger expression, a
//! list the scan cannot close) names the callee without a position, or names
//! nothing.
//!
//! When the passing function's call-site ledger proves which repository
//! declaration that call reaches, and that declaration's own ledger holds a
//! call through the parameter the value arrives as, the declaration calls
//! what it is given, and the passing function is a caller. A parameter the
//! declaration assigns to again proves nothing.

use std::ops::ControlFlow;

use kin_model::entity::Entity;
use kin_model::graph::GraphStore;
use kin_model::{CallSiteState, EntityId, EntityKind, LanguageId, ResolutionRecordId};
use kin_parser::site_syntax::SiteSyntax;

use super::{
    arguments_end, char_before, is_identifier_char, one_line, receiver_start, scan_code, syntax_of,
    whole_text, RECEIVER_MAX_CHARS,
};
use crate::handlers::external_symbols::SiteText;

/// The most text before a value that is read to find the call it sits in.
const ENCLOSING_SCAN_MAX_BYTES: usize = 64 * 1024;

/// Words a language writes before a parenthesized group that is no argument
/// list.
const NOT_A_CALLEE: [&str; 30] = [
    "and", "assert", "await", "case", "catch", "class", "def", "del", "delete", "do", "elif",
    "else", "except", "for", "function", "if", "in", "is", "lambda", "match", "not", "of", "or",
    "raise", "return", "switch", "throw", "typeof", "while", "yield",
];

/// What an answer can say about a value handed to a call as an argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassedValue {
    /// The callee the value is handed to, as written: `runWithFiberInDEV`,
    /// `app.config.from_file`.
    pub callee: String,
    /// The value's place among the call's arguments, from 0, when it is a
    /// whole positional argument and no spread sits before it.
    pub position: Option<usize>,
    /// The repository declaration the call is proven to reach.
    pub target: Option<(EntityId, String)>,
    /// The parameter of `target` the value arrives as, when `target`'s own
    /// ledger holds a call through that parameter: it calls what it is given.
    pub called_as: Option<String>,
}

/// Who hands the value at a listed site on, to what, and which of the two is
/// the function to count as a caller, in one sentence.
///
/// A reader told only that a value is passed "to other code, which may call
/// it" has to work out for itself whether the caller is the code that passes
/// it or the code that receives it, and one answer was wrong for reading it
/// the second way. The function that hands the value over is the one that
/// decides the focal runs: it is the caller, when the code it hands it to
/// calls what it is given. `site` is a listed residue site as
/// [`super::annotate_residue`] leaves it.
pub fn passed_note(site: &serde_json::Value) -> String {
    let text = |value: &serde_json::Value| value.as_str().map(str::to_string);
    let passer = site
        .get("inside")
        .and_then(text)
        .or_else(|| {
            site.get("in_module")
                .and_then(text)
                .map(|module| format!("{module}'s own code"))
        })
        .unwrap_or_else(|| "the code at this site".to_string());
    let to = site.get(super::PASSED_TO_KEY);
    let field = |key: &str| to.and_then(|to| to.get(key)).and_then(text);
    let receiver = field("target").or_else(|| field("callee"));
    match (receiver, field("calls_parameter")) {
        (Some(receiver), Some(parameter)) => format!(
            "{passer} passes it to {receiver}, and {receiver} calls what it is given (its \
             parameter {parameter}), so {passer} calls it through {receiver}. Count {passer}, \
             not {receiver}."
        ),
        (Some(receiver), None) => format!(
            "{passer} passes it to {receiver}; if {receiver} calls what it is given, the caller \
             to count is {passer}, not {receiver}."
        ),
        (None, _) => format!(
            "{passer} passes it to other code as an argument; if that code calls what it is \
             given, the caller to count is {passer}."
        ),
    }
}

/// The call an argument sits in, read from its caller's text.
struct EnclosingCall {
    /// The callee token, as a range of the caller's text.
    callee: (usize, usize),
    /// Where the receiver chain before the callee starts.
    callee_start: usize,
    position: Option<usize>,
}

/// What can be said about the value spelled at `token` in `body`, the exact
/// text of `holder`, the function that passes it on. `None` when the value
/// sits in no call this reading can place.
pub(super) fn read<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    text: &T,
    holder: &Entity,
    body: &str,
    token: (usize, usize),
) -> Option<PassedValue> {
    let syntax = syntax_of(holder);
    let call = enclosing_call(body, token, &syntax)?;
    let callee = one_line(
        &body[call.callee_start..call.callee.1],
        None,
        &syntax,
        RECEIVER_MAX_CHARS,
    );
    let mut passed = PassedValue {
        callee,
        position: call.position,
        target: None,
        called_as: None,
    };
    let Some(target) = proven_target(store, holder, call.callee) else {
        return Some(passed);
    };
    passed.called_as = call.position.and_then(|position| {
        let through_receiver = call.callee_start < call.callee.0;
        called_parameter(store, text, &target, position, through_receiver)
    });
    passed.target = Some((target.id, target.name.clone()));
    Some(passed)
}

/// The innermost call whose argument list holds `token`, checked against the
/// argument it names.
fn enclosing_call(body: &str, token: (usize, usize), syntax: &SiteSyntax) -> Option<EnclosingCall> {
    let (start, end) = token;
    if start >= end || end > body.len() || !body.is_char_boundary(start) {
        return None;
    }
    // Scan from the start of a line, far enough back to open every bracket
    // the value sits in, never from inside one.
    let floor = start.saturating_sub(ENCLOSING_SCAN_MAX_BYTES);
    let from = if floor == 0 {
        0
    } else {
        floor + body.get(floor..start)?.find('\n')? + 1
    };
    let mut open: Vec<(usize, char)> = Vec::new();
    // The scan steps over strings and comments, so it lands on the value's
    // first byte only when the value is code.
    let reached = scan_code(body, from, syntax, start - from, |at, character| {
        if at >= start {
            return ControlFlow::Break(Some(at == start));
        }
        match character {
            '(' | '[' | '{' => open.push((at, character)),
            ')' | ']' | '}' => {
                open.pop();
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
    if reached != Some(true) {
        return None;
    }
    let (paren, '(') = open.pop()? else {
        return None;
    };
    // The callee is the name written right before the list. A keyword there
    // (`if (`, `while (`) opens a group, not a call.
    let callee_end = body[..paren].trim_end_matches([' ', '\t']).len();
    let callee_end = body[..callee_end]
        .strip_suffix("?.")
        .map_or(callee_end, str::len);
    let callee_begin = body[..callee_end]
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_char(*character))
        .last()
        .map(|(at, _)| at)?;
    let callee = &body[callee_begin..callee_end];
    if callee
        .chars()
        .next()
        .is_none_or(|first| first.is_ascii_digit())
        || NOT_A_CALLEE.contains(&callee)
    {
        return None;
    }
    let close = arguments_end(body, callee_end, syntax)?;
    if close <= end || !body[..close].ends_with(')') {
        return None;
    }
    let arguments = split_arguments(body, paren + 1, close - 1, syntax)?;
    let index = arguments
        .iter()
        .position(|(from, to)| *from <= start && end <= *to)?;
    let written = |range: &(usize, usize)| body[range.0..range.1].trim();
    let value = written(&arguments[index]);
    // The value itself, bare or read off a receiver: `invoke`, or
    // `runner.invoke` for a site that spells `invoke`.
    let spelled = &body[start..end];
    let whole = value == spelled
        || value.strip_suffix(spelled).is_some_and(|receiver| {
            receiver.ends_with('.')
                && receiver.chars().all(|character| {
                    is_identifier_char(character) || matches!(character, '.' | '?')
                })
        });
    // An argument before it that may take several places, or that a comma
    // inside a lambda's parameters or a generic argument list cut in two,
    // leaves the value's own place unknown.
    let spread_before = arguments[..index].iter().any(|range| {
        let before = written(range);
        before.starts_with('*')
            || before.starts_with("...")
            || before.starts_with("lambda")
            || before.matches('<').count() != before.matches('>').count()
    });
    let named = value_of_keyword(value).is_some_and(|inner| inner.ends_with(spelled));
    if !whole && !named {
        return None;
    }
    Some(EnclosingCall {
        callee: (callee_begin, callee_end),
        callee_start: receiver_start(body, callee_begin, syntax),
        position: (whole && !spread_before).then_some(index),
    })
}

/// The value of an argument written `name=value` or `name: value`.
fn value_of_keyword(argument: &str) -> Option<&str> {
    let name_end = argument
        .char_indices()
        .find(|(_, character)| !is_identifier_char(*character))
        .map(|(at, _)| at)?;
    if name_end == 0 {
        return None;
    }
    let rest = argument[name_end..].trim_start();
    let value = rest
        .strip_prefix('=')
        .filter(|value| !value.starts_with('='))
        .or_else(|| rest.strip_prefix(':'))?;
    Some(value.trim())
}

/// The top-level arguments written in `body[from..to]`, each as a range, or
/// `None` when a bracket in it does not close. A trailing comma adds none.
fn split_arguments(
    body: &str,
    from: usize,
    to: usize,
    syntax: &SiteSyntax,
) -> Option<Vec<(usize, usize)>> {
    let mut ranges = Vec::new();
    let mut depth = 0usize;
    let mut begin = from;
    let mut broken = false;
    scan_code(
        body,
        from,
        syntax,
        to.saturating_sub(from),
        |at, character| {
            if at >= to {
                return ControlFlow::Break(Some(()));
            }
            match character {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => match depth.checked_sub(1) {
                    Some(less) => depth = less,
                    None => {
                        broken = true;
                        return ControlFlow::Break(Some(()));
                    }
                },
                ',' if depth == 0 => {
                    ranges.push((begin, at));
                    begin = at + 1;
                }
                _ => {}
            }
            ControlFlow::Continue(())
        },
    );
    if broken || depth != 0 {
        return None;
    }
    if !body[begin..to].trim().is_empty() {
        ranges.push((begin, to));
    }
    Some(ranges)
}

/// The repository declaration `holder`'s ledger proves the call at `callee`
/// reaches.
fn proven_target<G: GraphStore + ?Sized>(
    store: &G,
    holder: &Entity,
    callee: (usize, usize),
) -> Option<Entity> {
    let ledger = store
        .lookup_resolution_record(&ResolutionRecordId::call_sites(holder.id))
        .ok()
        .flatten()?;
    let site = ledger.as_call_sites()?.sites.iter().find(|site| {
        site.offset as usize == callee.0 && site.length as usize == callee.1 - callee.0
    })?;
    let target = match site.state {
        CallSiteState::ProvenTarget { target } | CallSiteState::ProvenStatic { target } => target,
        _ => return None,
    };
    store.get_entity(&target).ok().flatten()
}

/// The parameter of `target` an argument at `position` arrives as, when
/// `target` calls it: its ledger holds a call through a binding of that name,
/// and its body never assigns to the name again.
fn called_parameter<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    text: &T,
    target: &Entity,
    position: usize,
    through_receiver: bool,
) -> Option<String> {
    if !matches!(target.kind, EntityKind::Function | EntityKind::Method) {
        return None;
    }
    let body = whole_text(text, target).ok()?;
    let syntax = syntax_of(target);
    let (parameters, list_end) = parameters_of(&body, target, &syntax)?;
    // A Python method reached through an instance receives it first.
    let bound = target.language == LanguageId::Python
        && parameters
            .first()
            .is_some_and(|first| matches!(first.as_str(), "self" | "cls"));
    let index = match (bound, through_receiver) {
        (true, true) => position + 1,
        (true, false) => return None,
        (false, _) => position,
    };
    let name = parameters.get(index)?.clone();
    let ledger = store
        .lookup_resolution_record(&ResolutionRecordId::call_sites(target.id))
        .ok()
        .flatten()?;
    let called = ledger.as_call_sites()?.sites.iter().any(|site| {
        matches!(site.state, CallSiteState::Binding { .. })
            && kin_model::site_text(site, &body) == Some(name.as_str())
    });
    (called && !assigned_again(&body, list_end, &name, &syntax)).then_some(name)
}

/// The names `target` declares for its parameters, in order, and where the
/// list ends in `body`. `None` when a parameter is not one plain name.
fn parameters_of(body: &str, target: &Entity, syntax: &SiteSyntax) -> Option<(Vec<String>, usize)> {
    let name = target
        .name
        .rsplit(['.', ':'])
        .find(|segment| !segment.is_empty())?;
    let (open, close) = body.match_indices(name).find_map(|(at, _)| {
        let end = at + name.len();
        if char_before(body, at).is_some_and(is_identifier_char) {
            return None;
        }
        let close = arguments_end(body, end, syntax)?;
        let open = end + body[end..close].find('(')?;
        body[..close].ends_with(')').then_some((open, close))
    })?;
    let mut names = Vec::new();
    for (from, to) in split_arguments(body, open + 1, close - 1, syntax)? {
        let written = body[from..to].trim();
        // A rest or a starred parameter, and every one after it, takes no
        // fixed position.
        if written.starts_with('*') || written.starts_with("...") {
            break;
        }
        let end = written
            .char_indices()
            .find(|(_, character)| !is_identifier_char(*character))
            .map_or(written.len(), |(at, _)| at);
        let after = written[end..].trim_start();
        if end == 0
            || !(after.is_empty()
                || after.starts_with(':')
                || after.starts_with('?')
                || (after.starts_with('=') && !after.starts_with("==")))
        {
            return None;
        }
        names.push(written[..end].to_string());
    }
    Some((names, close))
}

/// Whether `body` assigns to `name` after its parameter list ends at `from`.
fn assigned_again(body: &str, from: usize, name: &str, syntax: &SiteSyntax) -> bool {
    let mut found = false;
    let mut at = from;
    while let Some(offset) = body[at..].find(name) {
        let start = at + offset;
        let end = start + name.len();
        at = end;
        if char_before(body, start)
            .is_some_and(|before| is_identifier_char(before) || before == '.')
            || body[end..].chars().next().is_some_and(is_identifier_char)
        {
            continue;
        }
        let rest = body[end..].trim_start_matches([' ', '\t']);
        let assigns = [
            "=", "+=", "-=", "*=", "/=", "|=", "&=", "??=", "||=", "&&=", ":=",
        ]
        .iter()
        .any(|operator| rest.starts_with(operator))
            && !rest.starts_with("==")
            && !rest.starts_with("=>");
        if assigns && code_at(body, from, start, syntax) {
            found = true;
            break;
        }
    }
    found
}

/// Whether `at` is code of `body`, not inside a string or a comment, read
/// from `from`.
fn code_at(body: &str, from: usize, at: usize, syntax: &SiteSyntax) -> bool {
    scan_code(body, from, syntax, at - from + 1, |offset, _| {
        if offset == at {
            ControlFlow::Break(Some(true))
        } else if offset > at {
            ControlFlow::Break(Some(false))
        } else {
            ControlFlow::Continue(())
        }
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_parser::site_syntax::{ECMASCRIPT, PYTHON};

    fn call_of(body: &str, name: &str, syntax: &SiteSyntax) -> Option<(String, Option<usize>)> {
        let start = super::super::spelled_at(body, name)
            .map(|(start, _)| start)
            .expect("the body spells the value");
        let call = enclosing_call(body, (start, start + name.len()), syntax)?;
        Some((
            body[call.callee_start..call.callee.1].to_string(),
            call.position,
        ))
    }

    /// React's `commitHostTextUpdate`, as the benchmark tree writes it: the
    /// value is the second argument of a call written over seven lines.
    const COMMIT_HOST_TEXT_UPDATE: &str = "export function commitHostTextUpdate(\n  \
        finishedWork: Fiber,\n  newText: string,\n  oldText: string,\n) {\n  \
        const textInstance: TextInstance = finishedWork.stateNode;\n  try {\n    \
        if (__DEV__) {\n      runWithFiberInDEV(\n        finishedWork,\n        \
        commitTextUpdate,\n        textInstance,\n        oldText,\n        newText,\n      \
        );\n    } else {\n      commitTextUpdate(textInstance, oldText, newText);\n    }\n  \
        } catch (error) {\n    captureCommitPhaseError(finishedWork, finishedWork.return, error);\n  \
        }\n}\n";

    #[test]
    fn a_value_on_its_own_line_of_a_long_call_names_the_call_and_its_place() {
        assert_eq!(
            call_of(COMMIT_HOST_TEXT_UPDATE, "commitTextUpdate", &ECMASCRIPT),
            Some(("runWithFiberInDEV".to_string(), Some(1)))
        );
    }

    #[test]
    fn flasks_passed_members_name_their_calls() {
        // tests/test_cli.py, TestRoutes.invoke.
        let body = "def invoke(self, app, runner):\n    \
                    cli = FlaskGroup(create_app=lambda: app)\n    \
                    return partial(runner.invoke, cli)\n";
        // The fixture's own name is spelled first; the value is the member.
        let start = body.find("runner.invoke").unwrap() + "runner.".len();
        let call = enclosing_call(body, (start, start + "invoke".len()), &PYTHON).unwrap();
        assert_eq!(&body[call.callee_start..call.callee.1], "partial");
        assert_eq!(call.position, Some(0));

        // tests/test_basic.py, test_missing_session.
        let body = "def test_missing_session(app):\n    with app.test_request_context():\n        \
                    expect_exception(flask.session.pop, \"foo\")\n";
        assert_eq!(
            call_of(body, "pop", &PYTHON),
            Some(("expect_exception".to_string(), Some(0)))
        );

        // tests/test_config.py: a call before it in the list, and a keyword
        // after it.
        let body = "def test_config_from_file_toml():\n    app.config.from_file(\n        \
                    os.path.join(current_dir, \"static\", \"config.toml\"), tomllib.load, \
                    text=False\n    )\n";
        assert_eq!(
            call_of(body, "load", &PYTHON),
            Some(("app.config.from_file".to_string(), Some(1)))
        );
    }

    #[test]
    fn a_keyword_argument_names_its_call_without_a_place() {
        let body = "def test_missing():\n    app.config.from_file(\"missing.json\", \
                    load=json.load)\n";
        let start = body.find("json.load").unwrap() + "json.".len();
        let call = enclosing_call(body, (start, start + 4), &PYTHON).unwrap();
        assert_eq!(
            &body[call.callee_start..call.callee.1],
            "app.config.from_file"
        );
        assert_eq!(call.position, None);
    }

    #[test]
    fn a_spread_before_the_value_leaves_its_place_unknown() {
        let body = "function run(args, handler) {\n  dispatch(...args, handler);\n}\n";
        let start = body.rfind("handler").unwrap();
        let call = enclosing_call(body, (start, start + 7), &ECMASCRIPT).unwrap();
        assert_eq!(&body[call.callee.0..call.callee.1], "dispatch");
        assert_eq!(call.position, None);
    }

    #[test]
    fn a_value_that_is_not_a_whole_argument_names_nothing() {
        // Inside a larger expression, the argument is not the value.
        let body = "function run(handler) {\n  dispatch(wrap ? handler : other);\n}\n";
        let start = body.rfind("handler").unwrap();
        assert!(enclosing_call(body, (start, start + 7), &ECMASCRIPT).is_none());
        // A condition is no call.
        let body = "function run(handler) {\n  if (handler) {\n    return 1;\n  }\n}\n";
        let start = body.rfind("handler").unwrap();
        assert!(enclosing_call(body, (start, start + 7), &ECMASCRIPT).is_none());
        // A bracket inside a string before the value opens nothing.
        let body = "function run(handler) {\n  log(\"(\", handler);\n}\n";
        assert_eq!(
            call_of(
                &body[body.find("log").unwrap() - 2..],
                "handler",
                &ECMASCRIPT
            ),
            Some(("log".to_string(), Some(1)))
        );
        // The innermost call takes the value, not the one around it.
        let body = "function run(handler) {\n  outer(first, inner(handler), last);\n}\n";
        let start = body.rfind("handler").unwrap();
        let call = enclosing_call(body, (start, start + 7), &ECMASCRIPT).unwrap();
        assert_eq!(&body[call.callee.0..call.callee.1], "inner");
        assert_eq!(call.position, Some(0));
    }

    #[test]
    fn parameters_read_as_plain_names_in_order() {
        let entity = |name: &str, language| {
            crate::call_sites::fixture::spanned_entity(name, "source", language, 0, "")
        };
        // React's `runWithFiberInDEV`: a function type among the parameters.
        let body = "export function runWithFiberInDEV<A0, A1, T>(\n  fiber: null | Fiber,\n  \
                    callback: (A0, A1) => T,\n  arg0: A0,\n  arg1: A1,\n): T {\n  \
                    return callback(arg0, arg1);\n}\n";
        let (names, _) = parameters_of(
            body,
            &entity("runWithFiberInDEV", LanguageId::JavaScript),
            &ECMASCRIPT,
        )
        .unwrap();
        assert_eq!(names, ["fiber", "callback", "arg0", "arg1"]);
        assert!(!assigned_again(
            body,
            body.find("): T").unwrap(),
            "callback",
            &ECMASCRIPT
        ));

        // A decorated Python method, with defaults and a starred tail.
        let body = "@setupmethod\ndef expect_exception(self, f, expected=RuntimeError, *args, \
                    **kwargs):\n    f = wrap(f)\n    return f(*args, **kwargs)\n";
        let (names, end) = parameters_of(
            body,
            &entity("Helper.expect_exception", LanguageId::Python),
            &PYTHON,
        )
        .unwrap();
        assert_eq!(names, ["self", "f", "expected"]);
        assert!(assigned_again(body, end, "f", &PYTHON));
        assert!(!assigned_again(body, end, "expected", &PYTHON));

        // A destructured parameter is not one plain name.
        let body = "function render({children}, ref) {\n  return ref(children);\n}\n";
        assert!(
            parameters_of(body, &entity("render", LanguageId::JavaScript), &ECMASCRIPT).is_none()
        );
    }

    use std::collections::HashMap;

    use crate::call_sites::fixture::{admit, id_of, ledger, proof_context, spanned_entity};
    use kin_db::InMemoryGraph;
    use kin_model::entity::SourceSpan;
    use kin_model::{CallSite, CallSiteLedger, Hash256, ResolutionRecord, UnresolvedReason};

    /// Each entity's own text, as an answer's body reader hands it over.
    struct Bodies(HashMap<EntityId, String>);

    impl SiteText for Bodies {
        fn quote(&self, caller: &Entity, site: &SourceSpan) -> Result<String, &'static str> {
            let span = caller.span.as_ref().ok_or("caller_has_no_span")?;
            self.0
                .get(&caller.id)
                .and_then(|body| {
                    body.get(site.start_byte - span.start_byte..site.end_byte - span.start_byte)
                })
                .map(str::to_string)
                .ok_or("caller_text_not_held")
        }
    }

    /// React's `runWithFiberInDEV`, cut to the call that matters.
    const RUN_WITH_FIBER: &str = "export function runWithFiberInDEV<A0, A1, A2, T>(\n  \
        fiber: null | Fiber,\n  callback: (A0, A1, A2) => T,\n  arg0: A0,\n  arg1: A1,\n  \
        arg2: A2,\n): T {\n  return callback(arg0, arg1, arg2);\n}\n";

    /// A ledger of `caller` with one site at the last place `body` spells
    /// `token`.
    fn ledger_at_last(
        caller: &Entity,
        body: &str,
        context: ResolutionRecordId,
        token: &str,
        state: CallSiteState,
    ) -> ResolutionRecord {
        ResolutionRecord::CallSites(CallSiteLedger {
            caller: caller.id,
            behavior_hash: caller.fingerprint.behavior_hash,
            body_hash: Hash256::from_bytes([0x43; 32]),
            context,
            census: 1,
            sites: vec![CallSite {
                offset: body.rfind(token).expect("the body spells the token") as u32,
                length: token.len() as u32,
                state,
            }],
        })
    }

    /// What is read for the value `commitTextUpdate` that
    /// `commitHostTextUpdate` hands to `runWithFiberInDEV`, under the state
    /// its own ledger gives that call and the state `runWithFiberInDEV`'s
    /// ledger gives the call through `callback` in `callee_body`.
    fn react_case(
        call_proven: bool,
        callback_state: CallSiteState,
        callee_body: &str,
    ) -> PassedValue {
        let store = InMemoryGraph::new();
        let language = LanguageId::JavaScript;
        let holder = spanned_entity(
            "commitHostTextUpdate",
            "packages/react-reconciler/src/ReactFiberCommitHostEffects.js",
            language,
            400,
            COMMIT_HOST_TEXT_UPDATE,
        );
        let target = spanned_entity(
            "runWithFiberInDEV",
            "packages/react-reconciler/src/ReactCurrentFiber.js",
            language,
            90,
            callee_body,
        );
        let context = proof_context(language, "5.7.3");
        let context_id = id_of(&context);
        let call_state = if call_proven {
            CallSiteState::ProvenStatic { target: target.id }
        } else {
            CallSiteState::Unresolved {
                reason: UnresolvedReason::NoAnswer,
            }
        };
        admit(
            &store,
            &[&holder, &target],
            vec![
                context,
                ledger(
                    &holder,
                    COMMIT_HOST_TEXT_UPDATE,
                    context_id,
                    vec![("runWithFiberInDEV", call_state)],
                ),
                ledger_at_last(&target, callee_body, context_id, "callback", callback_state),
            ],
        );
        let bodies = Bodies(HashMap::from([
            (holder.id, COMMIT_HOST_TEXT_UPDATE.to_string()),
            (target.id, callee_body.to_string()),
        ]));
        let start = COMMIT_HOST_TEXT_UPDATE.find("commitTextUpdate,").unwrap();
        read(
            &store,
            &bodies,
            &holder,
            COMMIT_HOST_TEXT_UPDATE,
            (start, start + "commitTextUpdate".len()),
        )
        .expect("the value sits in a call")
    }

    #[test]
    fn a_proven_callee_that_calls_its_parameter_settles_who_calls() {
        let binding = CallSiteState::Binding { may_call: None };
        let passed = react_case(true, binding.clone(), RUN_WITH_FIBER);
        assert_eq!(passed.callee, "runWithFiberInDEV");
        assert_eq!(passed.position, Some(1));
        assert_eq!(
            passed.target.as_ref().map(|(_, name)| name.as_str()),
            Some("runWithFiberInDEV")
        );
        assert_eq!(passed.called_as.as_deref(), Some("callback"));

        // The resolver said nothing about the call through `callback`: the
        // callee is named, and nothing is claimed about what it does.
        let silent = CallSiteState::Unresolved {
            reason: UnresolvedReason::NoAnswer,
        };
        let passed = react_case(true, silent, RUN_WITH_FIBER);
        assert!(passed.target.is_some());
        assert_eq!(passed.called_as, None);

        // The call itself is unproven: only what is written is known.
        let passed = react_case(false, binding.clone(), RUN_WITH_FIBER);
        assert_eq!(passed.callee, "runWithFiberInDEV");
        assert_eq!(passed.position, Some(1));
        assert_eq!((passed.target, passed.called_as), (None, None));

        // The callee binds the parameter's name again before it calls it.
        let rebound = RUN_WITH_FIBER.replace(
            "  return callback(",
            "  callback = wrap(callback);\n  return callback(",
        );
        let passed = react_case(true, binding, &rebound);
        assert!(passed.target.is_some());
        assert_eq!(passed.called_as, None);
    }

    /// A Python method reached through an instance takes the instance first,
    /// so the value arrives one parameter later than it is written.
    #[test]
    fn a_python_method_called_on_an_instance_counts_its_receiver() {
        let store = InMemoryGraph::new();
        let language = LanguageId::Python;
        let test = "def test_missing_session(app, checker):\n    \
                    checker.expect_exception(flask.session.pop, \"foo\")\n";
        let helper = "def expect_exception(self, f, *args, **kwargs):\n    \
                      e = pytest.raises(RuntimeError, f, *args, **kwargs)\n    \
                      return f(*args)\n";
        let holder = spanned_entity(
            "test_missing_session",
            "tests/test_basic.py",
            language,
            0,
            test,
        );
        let mut target = spanned_entity(
            "Checker.expect_exception",
            "tests/checker.py",
            language,
            0,
            helper,
        );
        target.kind = EntityKind::Method;
        let context = proof_context(language, "1.1.400");
        let context_id = id_of(&context);
        admit(
            &store,
            &[&holder, &target],
            vec![
                context,
                ledger(
                    &holder,
                    test,
                    context_id,
                    vec![(
                        "expect_exception",
                        CallSiteState::ProvenTarget { target: target.id },
                    )],
                ),
                ledger_at_last(
                    &target,
                    helper,
                    context_id,
                    "f",
                    CallSiteState::Binding { may_call: None },
                ),
            ],
        );
        let bodies = Bodies(HashMap::from([
            (holder.id, test.to_string()),
            (target.id, helper.to_string()),
        ]));
        let start = test.find(".pop").unwrap() + 1;
        let passed = read(&store, &bodies, &holder, test, (start, start + 3)).unwrap();
        assert_eq!(passed.callee, "checker.expect_exception");
        assert_eq!(passed.position, Some(0));
        assert_eq!(passed.called_as.as_deref(), Some("f"));
    }

    #[test]
    fn the_note_names_both_ends_and_says_which_one_to_count() {
        use serde_json::json;
        // What the answer about React's `commitTextUpdate` listed.
        let mut site = json!({
            "kind": "argument",
            "name": "commitTextUpdate",
            "inside": "commitHostTextUpdate",
            "expr": "commitTextUpdate,",
        });
        assert_eq!(
            passed_note(&site),
            "commitHostTextUpdate passes it to other code as an argument; if that code calls \
             what it is given, the caller to count is commitHostTextUpdate."
        );
        site[super::super::PASSED_TO_KEY] = json!({"callee": "runWithFiberInDEV", "position": 1});
        assert_eq!(
            passed_note(&site),
            "commitHostTextUpdate passes it to runWithFiberInDEV; if runWithFiberInDEV calls \
             what it is given, the caller to count is commitHostTextUpdate, not \
             runWithFiberInDEV."
        );
        site[super::super::PASSED_TO_KEY]["target"] = json!("runWithFiberInDEV");
        site[super::super::PASSED_TO_KEY]["calls_parameter"] = json!("callback");
        assert_eq!(
            passed_note(&site),
            "commitHostTextUpdate passes it to runWithFiberInDEV, and runWithFiberInDEV calls \
             what it is given (its parameter callback), so commitHostTextUpdate calls it \
             through runWithFiberInDEV. Count commitHostTextUpdate, not runWithFiberInDEV."
        );
        // A site at a module's top level names the module.
        let site = json!({"kind": "argument", "in_module": "flask.cli"});
        assert!(passed_note(&site).starts_with("flask.cli's own code passes it to other code"));
    }

    /// The whole path an answer takes: the census lists the value use by file
    /// and line, the residue reading finds the call it sits in, and the
    /// listed site carries it.
    #[test]
    fn a_listed_value_site_carries_the_call_it_is_handed_to() {
        use kin_model::{EscapeResidue, FocalEscape, ResidueSite};
        let store = InMemoryGraph::new();
        let language = LanguageId::JavaScript;
        let file = "packages/react-reconciler/src/ReactFiberCommitHostEffects.js";
        let holder = spanned_entity(
            "commitHostTextUpdate",
            file,
            language,
            400,
            COMMIT_HOST_TEXT_UPDATE,
        );
        let target = spanned_entity(
            "runWithFiberInDEV",
            "packages/react-reconciler/src/ReactCurrentFiber.js",
            language,
            90,
            RUN_WITH_FIBER,
        );
        let context = proof_context(language, "5.7.3");
        let context_id = id_of(&context);
        admit(
            &store,
            &[&holder, &target],
            vec![
                context,
                ledger(
                    &holder,
                    COMMIT_HOST_TEXT_UPDATE,
                    context_id,
                    vec![(
                        "runWithFiberInDEV",
                        CallSiteState::ProvenStatic { target: target.id },
                    )],
                ),
                ledger_at_last(
                    &target,
                    RUN_WITH_FIBER,
                    context_id,
                    "callback",
                    CallSiteState::Binding { may_call: None },
                ),
            ],
        );
        let bodies = Bodies(HashMap::from([
            (holder.id, COMMIT_HOST_TEXT_UPDATE.to_string()),
            (target.id, RUN_WITH_FIBER.to_string()),
        ]));
        // The census names the site by file and 1-based line.
        let line_in_body = COMMIT_HOST_TEXT_UPDATE
            .lines()
            .position(|line| line.trim() == "commitTextUpdate,")
            .unwrap() as u32;
        let line = holder.span.as_ref().unwrap().start_line + line_in_body + 1;
        let escape = FocalEscape::Escapes {
            reason: "a use of its name holds it as a value: see residue.values",
            residue: EscapeResidue {
                values: vec![ResidueSite {
                    file: file.to_string(),
                    line,
                    kind: super::super::PASSED_SITE_KIND,
                    name: Some("commitTextUpdate".to_string()),
                    detail: None,
                }],
                ..EscapeResidue::default()
            },
        };
        let lines = super::super::residue_lines(&store, &bodies, &escape, 10);
        let mut summary = escape.summary_json(10);
        super::super::annotate_residue(&mut summary, &lines);
        let site = &summary["residue"]["values"]["sites"][0];
        assert_eq!(site["inside"], "commitHostTextUpdate");
        assert_eq!(
            site[super::super::PASSED_TO_KEY],
            serde_json::json!({
                "callee": "runWithFiberInDEV",
                "position": 1,
                "target_id": target.id.to_string(),
                "target": "runWithFiberInDEV",
                "calls_parameter": "callback",
            })
        );
        assert!(
            passed_note(site).starts_with(
                "commitHostTextUpdate passes it to runWithFiberInDEV, and runWithFiberInDEV calls"
            ),
            "{}",
            passed_note(site)
        );
    }
}
