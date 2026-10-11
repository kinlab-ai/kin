// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source that does not parse is never committed.
//!
//! Every write that changes source is read once more as its language before
//! anything is published: an update, a patch, a creation, a removal, a patch
//! to a module's or a class's own code. A change that would leave the source
//! with a syntax problem it did not have is refused with one sentence that
//! names the problem and its line in what the caller sent.
//!
//! Reconciliation used to be the only reader, and it refuses an incomplete
//! parse only when a declaration would vanish with it. A method whose new
//! body dropped a closing bracket, or stood at its own header's indentation,
//! kept every declaration and was committed.
//!
//! The comparison is with the source as it stood
//! ([`kin_parser::syntax_reading::SyntaxReading::introduced_since`]), so a
//! problem the source already had beside the change, which can be valid code
//! newer than the grammar, never refuses a change that did not touch it.

use kin_model::LanguageId;
use kin_parser::syntax_reading::{read_syntax, Replaced, SyntaxProblem, SyntaxProblemKind};

/// The code one operation sent, where it went and what a refusal calls it.
#[derive(Debug, Clone)]
pub struct Sent {
    /// What the refusal calls it: `the code sent for Circle.area`,
    /// `Circle.area as patched`.
    pub what: String,
    /// The bytes it replaced in the source as it stood, and its length now.
    pub replaced: Replaced,
    /// How many bytes at the start of what stands there now are space ahead
    /// of the code: the blank lines and indentation a created declaration is
    /// placed behind. Lines are counted from the code.
    pub lead: usize,
    /// How the refusal places a problem in it.
    pub lines: Lines,
}

/// How a problem is placed in what an operation sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lines {
    /// By its line counted from the start of the run: the run is code the
    /// caller holds line by line, an entity's whole code.
    Numbered,
    /// By the line of code alone: the run is what a patch changed in a
    /// module's or a class's own code, which is served with its entities
    /// elided, so a count from its start is no line the caller has.
    Quoted,
    /// As code the change left behind: the operation sent no code and took
    /// some away.
    Removed,
}

impl Sent {
    /// The one run in which `now` differs from `before`, for a change whose
    /// place in the source the caller did not name: everything between the
    /// bytes the two share at their start and at their end.
    pub fn changed_run(what: String, before: &[u8], now: &[u8]) -> Self {
        let shared = before.len().min(now.len());
        let head = before
            .iter()
            .zip(now)
            .take_while(|(old, new)| old == new)
            .count();
        let tail = before
            .iter()
            .rev()
            .zip(now.iter().rev())
            .take(shared - head)
            .take_while(|(old, new)| old == new)
            .count();
        Self {
            what,
            replaced: Replaced {
                before: head..before.len() - tail,
                now_len: now.len() - head - tail,
            },
            lead: 0,
            lines: Lines::Quoted,
        }
    }

    /// [`Self::changed_run`] for an operation that put in code the caller
    /// sent whole, so a line counted from the run's first line of code is a
    /// line of what was sent.
    pub fn placed(what: String, before: &[u8], now: &[u8]) -> Self {
        let mut run = Self::changed_run(what, before, now);
        // The run opens with the blank lines and indentation it was placed
        // behind, which are not lines of the code.
        let start = Replaced::now(std::slice::from_ref(&run.replaced))[0].start;
        let lead = now[start..start + run.replaced.now_len]
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        run.lead = lead;
        run.lines = Lines::Numbered;
        run
    }

    /// [`Self::changed_run`] for an operation that took code away.
    pub fn removed(what: String, before: &[u8], now: &[u8]) -> Self {
        Self {
            lines: Lines::Removed,
            ..Self::changed_run(what, before, now)
        }
    }
}

/// A language as a sentence names it.
pub fn language_name(language: LanguageId) -> &'static str {
    language.profile().display_name
}

/// The longest stretch of one line of code a refusal quotes.
const QUOTED_MAX_CHARS: usize = 80;

/// What is wrong, in the words a refusal ends with.
fn problem_words(problem: &SyntaxProblem) -> String {
    match &problem.kind {
        SyntaxProblemKind::Missing(token) => {
            if token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                // A named construct: `identifier`, `expression`.
                let name = token.replace('_', " ");
                let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) {
                    "an"
                } else {
                    "a"
                };
                format!("{article} {name} is missing")
            } else {
                format!("`{token}` is missing")
            }
        }
        SyntaxProblemKind::Unexpected => "this does not parse".to_string(),
        SyntaxProblemKind::Layout(what) => (*what).to_string(),
    }
}

/// The line of `source` that holds `byte`, without the whitespace around it,
/// cut to what a sentence can carry.
fn quoted_line(source: &[u8], byte: usize) -> String {
    let byte = byte.min(source.len());
    let start = source[..byte]
        .iter()
        .rposition(|at| *at == b'\n')
        .map_or(0, |newline| newline + 1);
    let end = source[byte..]
        .iter()
        .position(|at| *at == b'\n')
        .map_or(source.len(), |offset| byte + offset);
    let line = String::from_utf8_lossy(&source[start..end]);
    let line = line.trim();
    if line.chars().count() <= QUOTED_MAX_CHARS {
        return line.to_string();
    }
    let cut: String = line.chars().take(QUOTED_MAX_CHARS).collect();
    format!("{} ...", cut.trim_end())
}

/// One sentence for `problem` at `line` of `what`, quoting the line when the
/// agent surface would carry the quote: code that spells a path is withheld
/// there whole, and the sentence matters more than the quote.
fn sentence(lead: String, quoted: &str, problem: &SyntaxProblem) -> String {
    let words = problem_words(problem);
    if !quoted.is_empty() && !quoted.contains('`') {
        let said = format!("{lead}, `{quoted}`: {words}");
        if crate::semantic_surface::is_semantic(&said) {
            return said;
        }
    }
    format!("{lead}: {words}")
}

/// Refuse `now`, source in `language` after a change, when it has a syntax
/// problem that `before`, the same source as it stood, did not.
///
/// `sent` names every run of bytes the change replaced, ascending and apart.
/// The refusal is one clause that follows "Nothing was written:" and names
/// the first such problem by its line in the code that was sent. A language
/// no grammar reads here is not judged.
pub fn refuse_unparseable(
    language: LanguageId,
    before: Option<&[u8]>,
    now: &[u8],
    sent: &[Sent],
) -> Result<(), String> {
    let Some(reading) = read_syntax(language, now) else {
        return Ok(());
    };
    if reading.problems.is_empty() {
        return Ok(());
    }
    let replaced: Vec<Replaced> = sent.iter().map(|sent| sent.replaced.clone()).collect();
    let stood = before
        .and_then(|before| read_syntax(language, before))
        .unwrap_or_default();
    let introduced = reading.introduced_since(&stood, &replaced);
    let Some(problem) = introduced.first() else {
        return Ok(());
    };
    let language = language_name(language);
    let places = Replaced::now(&replaced);
    // The code the problem is in: the run it starts in, or one it reaches
    // into from the same line, as an unclosed bracket's does when only the
    // bracket was sent.
    let line_start = |byte: usize| {
        now[..byte.min(now.len())]
            .iter()
            .rposition(|at| *at == b'\n')
            .map_or(0, |newline| newline + 1)
    };
    let within = places
        .iter()
        .position(|place| place.start <= problem.start_byte && problem.start_byte <= place.end)
        .or_else(|| {
            places.iter().position(|place| {
                problem.start_byte < place.start
                    && place.start <= problem.end_byte
                    && line_start(problem.start_byte) == line_start(place.start)
            })
        });
    let quoted = quoted_line(now, problem.start_byte);
    let around = |what: &str| {
        sentence(
            format!("{what} leaves code around it that is not valid {language}"),
            &quoted,
            problem,
        )
    };
    Err(match within.map(|index| (&sent[index], &places[index])) {
        Some((sent, _)) if sent.lines == Lines::Removed => around(&sent.what),
        Some((sent, place)) if sent.lines == Lines::Numbered => {
            let from = (place.start + sent.lead).min(problem.start_byte);
            let line = now[from..problem.start_byte.min(now.len())]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count()
                + 1;
            sentence(
                format!("{} is not valid {language} at line {line}", sent.what),
                &quoted,
                problem,
            )
        }
        Some((sent, _)) => sentence(
            format!("{} is not valid {language}", sent.what),
            &quoted,
            problem,
        ),
        None => around(
            sent.first()
                .map_or("this change", |sent| sent.what.as_str()),
        ),
    })
}

/// Refuse code of one entity's edit that ended up outside the entity.
///
/// `sent` is where the code a caller sent for `name` stands in `now`, and
/// `held` is its span after the language adapter reparses that source. Check
/// both sides of that span, including its first and last lines: an import
/// before a function or a statement after its closing brace can parse without
/// belonging to the declaration. The adapter owns the span (including such
/// syntax as Python decorators and Rust outer attributes), and its grammar
/// tells code from comments. Existing syntax outside `sent` is untouched.
pub fn refuse_code_outside(
    language: LanguageId,
    now: &[u8],
    what: &str,
    name: &str,
    sent: &std::ops::Range<usize>,
    held: &std::ops::Range<usize>,
) -> Result<(), String> {
    let before = sent.start..held.start.clamp(sent.start, sent.end);
    let after = held.end.clamp(sent.start, sent.end)..sent.end;
    let Some(outside) = [before, after]
        .into_iter()
        .find_map(|range| kin_parser::syntax_reading::first_code_in(language, now, range))
    else {
        return Ok(());
    };
    let line = now[sent.start..outside]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    let quoted = quoted_line(now, outside);
    let lead = format!("line {line} of {what}");
    let rest = format!(
        "stands outside {name}, and an edit holds the one declaration it names, so check that \
         line's indentation and the brackets before it"
    );
    if !quoted.contains('`') {
        let said = format!("{lead}, `{quoted}`, {rest}");
        if crate::semantic_surface::is_semantic(&said) {
            return Err(said);
        }
    }
    Err(format!("{lead} {rest}"))
}

/// Refuse `code`, one whole declaration as a caller sent it, when it does not
/// parse on its own as `language`. `what` names it: `the code sent for
/// added`.
///
/// For a declaration that is whole source by itself, a function or a class,
/// read before it is placed so the refusal can name the line as sent.
pub fn refuse_unparseable_code(language: LanguageId, code: &str, what: &str) -> Result<(), String> {
    refuse_unparseable_within(language, ("", ""), code, what)
}

/// [`refuse_unparseable_code`] for code that its language reads only inside
/// other code, a method inside a class body: `code` is read between `open`
/// and `close`, and a problem is still named by its line in `code`.
pub fn refuse_unparseable_within(
    language: LanguageId,
    (open, close): (&str, &str),
    code: &str,
    what: &str,
) -> Result<(), String> {
    let wrapped = format!("{open}{code}{close}");
    refuse_unparseable(
        language,
        None,
        wrapped.as_bytes(),
        &[Sent {
            what: what.to_string(),
            replaced: Replaced {
                before: open.len()..open.len(),
                now_len: code.len(),
            },
            lead: 0,
            lines: Lines::Numbered,
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(source: &str, held: &str, now: &str, what: &str) -> (String, Vec<Sent>) {
        let start = source.find(held).unwrap();
        (
            source.replacen(held, now, 1),
            vec![Sent {
                what: what.to_string(),
                replaced: Replaced {
                    before: start..start + held.len(),
                    now_len: now.len(),
                },
                lead: 0,
                lines: Lines::Numbered,
            }],
        )
    }

    const PYTHON: &str = "import os\n\n\nclass A:\n    def f(self):\n        return 1\n\n    def g(self):\n        return 2\n";

    #[test]
    fn a_problem_is_named_by_its_line_in_what_was_sent() {
        for (now, expected) in [
            (
                "def f(self):\n    return 1",
                "the code sent for A.f is not valid Python at line 2, `return 1`: expected an \
                 indented block",
            ),
            (
                "def f(self):\n        value = 1\n            return value",
                "the code sent for A.f is not valid Python at line 3, `return value`: unexpected \
                 indent",
            ),
            (
                "def f(self):\n        return (1",
                "the code sent for A.f is not valid Python at line 2, `return (1`: this does not \
                 parse",
            ),
        ] {
            let (after, sent) = sent(
                PYTHON,
                "def f(self):\n        return 1",
                now,
                "the code sent for A.f",
            );
            let refusal = refuse_unparseable(
                LanguageId::Python,
                Some(PYTHON.as_bytes()),
                after.as_bytes(),
                &sent,
            )
            .unwrap_err();
            assert_eq!(refusal, expected);
            assert!(crate::semantic_surface::is_semantic(&refusal));
            assert!(!refusal.contains('\n'));
        }
        let typescript = "class A {\n  f(): number {\n    return 1;\n  }\n}\n";
        let (after, sent) = sent(
            typescript,
            "f(): number {\n    return 1;\n  }",
            "f(): number {\n    return g(1;\n  }",
            "the code sent for A.f",
        );
        assert_eq!(
            refuse_unparseable(LanguageId::TypeScript, Some(typescript.as_bytes()), after.as_bytes(), &sent)
                .unwrap_err(),
            "the code sent for A.f is not valid TypeScript at line 2, `return g(1;`: `)` is missing"
        );
    }

    #[test]
    fn clean_code_and_a_problem_the_source_already_had_are_not_refused() {
        let (after, changed) = sent(
            PYTHON,
            "def f(self):\n        return 1",
            "def f(self):\n        return 3",
            "the code sent for A.f",
        );
        assert_eq!(
            refuse_unparseable(
                LanguageId::Python,
                Some(PYTHON.as_bytes()),
                after.as_bytes(),
                &changed
            ),
            Ok(())
        );
        // The source already had a problem below the change.
        let broken = format!("{PYTHON}\nx = (1 +)\n");
        let (after, changed) = sent(
            &broken,
            "def f(self):\n        return 1",
            "def f(self):\n        value = 3\n        return value",
            "the code sent for A.f",
        );
        assert_eq!(
            refuse_unparseable(
                LanguageId::Python,
                Some(broken.as_bytes()),
                after.as_bytes(),
                &changed
            ),
            Ok(())
        );
    }

    /// A quoted line that spells a path would have the whole sentence
    /// withheld at the agent surface, so the quote is what goes.
    #[test]
    fn a_line_the_surface_would_withhold_is_not_quoted() {
        let (after, sent) = sent(
            PYTHON,
            "def f(self):\n        return 1",
            "def f(self):\n        return [\"src/app.py\"",
            "the code sent for A.f",
        );
        let refusal = refuse_unparseable(
            LanguageId::Python,
            Some(PYTHON.as_bytes()),
            after.as_bytes(),
            &sent,
        )
        .unwrap_err();
        assert_eq!(
            refusal,
            "the code sent for A.f is not valid Python at line 2: this does not parse"
        );
        assert!(crate::semantic_surface::is_semantic(&refusal));
    }

    /// A change whose place the caller did not name is the run that differs,
    /// and its problem is named by the line of code, not by a count.
    #[test]
    fn a_changed_run_is_found_and_its_problem_quoted_without_a_line_number() {
        let now = PYTHON.replace("import os\n", "import os\nLIMIT = (3\n");
        let run = Sent::changed_run(
            "the module-level code of pkg.app as patched".into(),
            PYTHON.as_bytes(),
            now.as_bytes(),
        );
        assert_eq!(run.replaced.before, 10..10);
        assert_eq!(run.replaced.now_len, "LIMIT = (3\n".len());
        let refusal = refuse_unparseable(
            LanguageId::Python,
            Some(PYTHON.as_bytes()),
            now.as_bytes(),
            &[run],
        )
        .unwrap_err();
        assert!(
            refusal.starts_with(
                "the module-level code of pkg.app as patched is not valid Python, `LIMIT = (3`: "
            ),
            "{refusal}"
        );
        // Nothing changed, a shorter source and a longer one all find a run.
        for (before, after, expected) in [
            ("abc", "abc", (3..3, 0)),
            ("abcabc", "abc", (3..6, 0)),
            ("abc", "abcabc", (3..3, 3)),
            ("abXc", "abYYc", (2..3, 2)),
        ] {
            let run = Sent::changed_run(String::new(), before.as_bytes(), after.as_bytes());
            assert_eq!(
                (run.replaced.before, run.replaced.now_len),
                expected,
                "{before} {after}"
            );
        }
    }

    /// A statement dedented out of a method parses and is refused all the
    /// same; a comment after the method is nobody's code.
    #[test]
    fn code_that_ends_up_outside_its_entity_is_refused_by_its_line() {
        let now = "class A:\n    def f(self):\n        value = 1\n    return value\n    # done\n";
        let sent = now.find("def f").unwrap()..now.find("\n    # done").unwrap();
        let held = sent.start..now.find("\n    return value").unwrap();
        assert_eq!(
            refuse_code_outside(
                LanguageId::Python,
                now.as_bytes(),
                "the code sent for A.f",
                "A.f",
                &sent,
                &held
            )
            .unwrap_err(),
            "line 3 of the code sent for A.f, `return value`, stands outside A.f, and an edit \
             holds the one declaration it names, so check that line's indentation and the \
             brackets before it"
        );
        // The whole of what was sent is the entity: nothing is outside.
        assert_eq!(
            refuse_code_outside(
                LanguageId::Python,
                now.as_bytes(),
                "the code sent for A.f",
                "A.f",
                &sent,
                &sent
            ),
            Ok(())
        );
        // Only a comment follows the entity.
        let commented = "class A:\n    def f(self):\n        return 1\n        # done\n";
        let sent = commented.find("def f").unwrap()..commented.len() - 1;
        let held = sent.start..commented.find("\n        # done").unwrap();
        assert_eq!(
            refuse_code_outside(
                LanguageId::Python,
                commented.as_bytes(),
                "the code sent for A.f",
                "A.f",
                &sent,
                &held
            ),
            Ok(())
        );
    }

    /// Every language uses the same boundary check; the adapter supplies
    /// what the declaration owns, including its language-specific syntax.
    #[test]
    fn outside_code_checks_both_edges_of_the_adapters_entity_span() {
        for (language, code, name, outside_line) in [
            (
                LanguageId::TypeScript,
                "import { other } from \"other\";\nfunction helper() { return other(); }",
                "helper",
                Some(1),
            ),
            (
                LanguageId::TypeScript,
                "function helper() {} console.log(1);",
                "helper",
                Some(1),
            ),
            (
                LanguageId::Rust,
                "use std::fmt;\nfn helper() {}",
                "helper",
                Some(1),
            ),
            (
                LanguageId::Rust,
                "fn helper() {} use std::fmt;",
                "helper",
                Some(1),
            ),
            (
                LanguageId::Python,
                "import os\ndef helper():\n    return os.sep",
                "helper",
                Some(1),
            ),
            (
                LanguageId::Python,
                "def helper():\n    return 1\nprint(2)",
                "helper",
                Some(3),
            ),
            (
                LanguageId::TypeScript,
                "/* before */ function helper() { return 2; } // after",
                "helper",
                None,
            ),
            (
                LanguageId::Rust,
                "// before\n#[inline]\nfn helper() { let value = 2; } /* after */",
                "helper",
                None,
            ),
            (
                LanguageId::Python,
                "# before\n@decorator\ndef helper():\n    return 2\n# after",
                "helper",
                None,
            ),
        ] {
            let registry = kin_parser::AdapterRegistry::new();
            let adapter = registry.get_by_language(language).unwrap();
            let tree = adapter.parse(code.as_bytes()).unwrap();
            assert!(!tree.root_node().has_error(), "{code}");
            let file = kin_model::FilePathId::new("fixture");
            let parsed = adapter.extract(&tree, code.as_bytes(), &file).unwrap();
            let entity = parsed
                .entities
                .iter()
                .find(|entity| entity.name == name)
                .unwrap();
            let result = refuse_code_outside(
                language,
                code.as_bytes(),
                "the code sent for helper",
                name,
                &(0..code.len()),
                &(entity.span.start_byte..entity.span.end_byte),
            );
            match outside_line {
                Some(line) => {
                    let refusal = result.unwrap_err();
                    assert!(
                        refusal.starts_with(&format!("line {line} of ")),
                        "{code}: {refusal}"
                    );
                    assert!(
                        refusal.contains("stands outside helper"),
                        "{code}: {refusal}"
                    );
                }
                None => assert_eq!(result, Ok(()), "{code}"),
            }
        }
    }

    /// A created declaration is found as the run that differs, and its lines
    /// count from its own first line, past the space it was placed behind.
    #[test]
    fn a_placed_declaration_counts_lines_from_its_code_and_a_removal_names_what_it_leaves() {
        let now = PYTHON.replace(
            "        return 1\n",
            "        return 1\n\n    def added(self):\n        value = 3\n            return value\n",
        );
        let placed = Sent::placed(
            "the code sent for A.added".into(),
            PYTHON.as_bytes(),
            now.as_bytes(),
        );
        assert_eq!(placed.lines, Lines::Numbered);
        assert_eq!(
            refuse_unparseable(
                LanguageId::Python,
                Some(PYTHON.as_bytes()),
                now.as_bytes(),
                &[placed]
            )
            .unwrap_err(),
            "the code sent for A.added is not valid Python at line 3, `return value`: unexpected \
             indent"
        );
        // Taking away a class's only statement leaves a header with no body.
        let before = "class A:\n    def f(self):\n        return 1\n\n\nx = 1\n";
        let now = "class A:\n\n\nx = 1\n";
        let removed = Sent::removed("removing A.f".into(), before.as_bytes(), now.as_bytes());
        assert_eq!(
            refuse_unparseable(
                LanguageId::Python,
                Some(before.as_bytes()),
                now.as_bytes(),
                &[removed]
            )
            .unwrap_err(),
            "removing A.f leaves code around it that is not valid Python, `x = 1`: expected an \
             indented block"
        );
    }

    #[test]
    fn a_whole_declaration_is_read_on_its_own() {
        assert_eq!(
            refuse_unparseable_code(
                LanguageId::Rust,
                "pub fn added() -> u8 { 3 }",
                "the code sent for added"
            ),
            Ok(())
        );
        assert_eq!(
            refuse_unparseable_code(
                LanguageId::Rust,
                "pub fn added() -> u8 {\n    (3\n}",
                "the code sent for added"
            )
            .unwrap_err(),
            "the code sent for added is not valid Rust at line 2, `(3`: `)` is missing"
        );
    }
}
