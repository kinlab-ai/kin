// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What a parse says is wrong with source text, and which of its lines begin
//! inside a string.
//!
//! Extraction records *that* a parse was incomplete
//! ([`kin_model::ParseState::Incomplete`]) and where. A write path that
//! refuses source it cannot parse has to say more: what is wrong, on which
//! line of what the caller sent. This reads the same tree for the problems
//! themselves, in the grammar's own terms: a token the grammar expected and
//! did not find, or text it could not place.
//!
//! The second reading serves placement. Code written for one indentation is
//! moved to another by shifting its lines, and a line that begins inside a
//! string literal is content, not layout: in a language where indentation is
//! not syntax, shifting it changes a value for the sake of appearance. So the
//! tree is asked which lines begin inside one.
//!
//! Nothing here extracts an entity or a relation, so nothing a store holds
//! depends on it.

use kin_model::LanguageId;
use tree_sitter::{Node, Tree};

use std::sync::OnceLock;

use crate::languages::AdapterRegistry;

/// What is wrong at one place a parse could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxProblemKind {
    /// The grammar expected this token here and did not find it, in the
    /// grammar's spelling: `)`, `:`, `identifier`.
    Missing(String),
    /// Text the grammar could not place: the bytes of the range.
    Unexpected,
    /// The lines are laid out in a way the language refuses although every
    /// token is in order: a block that is not indented under its header, a
    /// statement indented unlike the statements beside it. The words are the
    /// language's own for it.
    Layout(&'static str),
}

/// One place a parse could not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxProblem {
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: SyntaxProblemKind,
}

/// What one parse of a source says beyond its entities.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyntaxReading {
    /// Every error and missing node, outermost first: the nodes
    /// [`crate::adapter::collect_error_ranges`] reports. Then, for a language
    /// whose indentation is syntax, every line laid out as that language
    /// refuses ([`SyntaxProblemKind::Layout`]), which its grammar reads
    /// without complaint and its adapter reports
    /// ([`crate::LanguageAdapter::layout_problems`]). Ascending by where each
    /// starts.
    pub problems: Vec<SyntaxProblem>,
    /// The byte offset of every line start that lies inside a string literal,
    /// ascending: the first byte of a line whose leading whitespace is part
    /// of a value.
    pub string_line_starts: Vec<usize>,
    /// The members of [`Self::string_line_starts`] that lie inside a
    /// docstring, the string a definition or a module opens with. Its text
    /// is laid out with the code around it and read with that layout taken
    /// off, so moving its lines with the code changes nothing a reader of
    /// the docstring sees.
    pub docstring_line_starts: Vec<usize>,
}

/// One run of bytes a change replaced: where it stood in the source before,
/// and how long what stands there now is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replaced {
    pub before: std::ops::Range<usize>,
    pub now_len: usize,
}

impl Replaced {
    /// Where each of `replaced`, ascending and apart, stands in the source
    /// after the change.
    pub fn now(replaced: &[Replaced]) -> Vec<std::ops::Range<usize>> {
        let mut moved = 0isize;
        replaced
            .iter()
            .map(|run| {
                let start = (run.before.start as isize + moved) as usize;
                moved += run.now_len as isize - run.before.len() as isize;
                start..start + run.now_len
            })
            .collect()
    }
}

impl SyntaxReading {
    /// Whether the line that starts at byte `line_start` begins inside a
    /// string literal.
    pub fn starts_inside_string(&self, line_start: usize) -> bool {
        self.string_line_starts.binary_search(&line_start).is_ok()
    }

    /// Whether the line that starts at byte `line_start` begins inside a
    /// docstring.
    pub fn starts_inside_docstring(&self, line_start: usize) -> bool {
        self.docstring_line_starts
            .binary_search(&line_start)
            .is_ok()
    }

    /// The problems of this reading that `before`, the reading of the source
    /// as it stood, does not account for, given the runs a change `replaced`
    /// (ascending and apart).
    ///
    /// A problem the source already had outside every replaced run is still
    /// there after the change, moved by what the runs before it grew or
    /// shrank, and is nobody's new mistake: a construct newer than the
    /// grammar reads as a problem in source that is perfectly valid, and a
    /// change beside it must not be refused for it. Everything else the
    /// change brought.
    pub fn introduced_since<'a>(
        &'a self,
        before: &SyntaxReading,
        replaced: &[Replaced],
    ) -> Vec<&'a SyntaxProblem> {
        let carried: Vec<SyntaxProblem> = before
            .problems
            .iter()
            .filter_map(|problem| {
                let mut moved = 0isize;
                for run in replaced {
                    if problem.end_byte <= run.before.start && problem.start_byte < run.before.start
                        || problem.end_byte < run.before.start
                    {
                        break;
                    }
                    if problem.start_byte < run.before.end {
                        return None;
                    }
                    moved += run.now_len as isize - run.before.len() as isize;
                }
                Some(SyntaxProblem {
                    start_byte: (problem.start_byte as isize + moved) as usize,
                    end_byte: (problem.end_byte as isize + moved) as usize,
                    kind: problem.kind.clone(),
                })
            })
            .collect();
        self.problems
            .iter()
            .filter(|problem| !carried.contains(problem))
            .collect()
    }
}

fn registry() -> &'static AdapterRegistry {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AdapterRegistry::new)
}

/// Read `source` as `language`. `None` when no grammar reads the language or
/// the parser gave no tree, which a caller treats as nothing known rather than
/// as clean.
pub fn read_syntax(language: LanguageId, source: &[u8]) -> Option<SyntaxReading> {
    let registry = AdapterRegistry::new();
    let tree = registry.get_by_language(language)?.parse(source).ok()?;
    Some(read_tree(language, &tree, source))
}

/// The first byte of `range` in `source` that begins code: not whitespace and
/// not part of a comment. `None` when the range holds no code, or no grammar
/// reads the language.
///
/// For a write path that must know whether lines a caller sent ended up
/// outside the declaration they were sent for: a comment after it is no
/// one's code, and anything else is.
pub fn first_code_in(
    language: LanguageId,
    source: &[u8],
    range: std::ops::Range<usize>,
) -> Option<usize> {
    let end = range.end.min(source.len());
    let mut at = range.start;
    if source[at.min(end)..end]
        .iter()
        .all(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    let registry = AdapterRegistry::new();
    let tree = registry.get_by_language(language)?.parse(source).ok()?;
    let root = tree.root_node();
    while at < end {
        if source[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        let mut node = root.descendant_for_byte_range(at, at + 1)?;
        let comment = loop {
            if node.kind().contains("comment") {
                break Some(node);
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => break None,
            }
        };
        match comment {
            Some(comment) => at = comment.end_byte().max(at + 1),
            None => return Some(at),
        }
    }
    None
}

/// Whether `language` reads indentation as structure, so that code moved to
/// another indentation must move as one block and a line laid out wrongly is
/// a syntax error no token shows.
pub fn indentation_is_syntax(language: LanguageId) -> bool {
    language.profile().indentation_is_syntax
}

/// [`read_syntax`] over a tree the caller already holds.
pub fn read_tree(language: LanguageId, tree: &Tree, source: &[u8]) -> SyntaxReading {
    let root = tree.root_node();
    let mut problems = Vec::new();
    if root.has_error() {
        collect_problems(&root, &mut problems);
    }
    let adapter = registry().get_by_language(language);
    if indentation_is_syntax(language) {
        if let Some(adapter) = adapter {
            adapter.layout_problems(&root, source, &mut problems);
        }
        problems.sort_by_key(|problem| (problem.start_byte, problem.end_byte));
    }
    let mut string_line_starts = Vec::new();
    let mut docstring_line_starts = Vec::new();
    for (at, byte) in source.iter().enumerate() {
        if *byte != b'\n' || at + 1 >= source.len() {
            continue;
        }
        // The smallest node that holds the line break itself. Between two
        // tokens that is the construct around them; inside a literal it is
        // the literal, or the piece of it that is text.
        let Some(node) = root
            .descendant_for_byte_range(at, at + 1)
            .filter(holds_text)
        else {
            continue;
        };
        string_line_starts.push(at + 1);
        if adapter.is_some_and(|adapter| adapter.is_docstring(node)) {
            docstring_line_starts.push(at + 1);
        }
    }
    SyntaxReading {
        problems,
        string_line_starts,
        docstring_line_starts,
    }
}

fn collect_problems(node: &Node, problems: &mut Vec<SyntaxProblem>) {
    if node.is_missing() {
        problems.push(SyntaxProblem {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind: SyntaxProblemKind::Missing(node.kind().to_string()),
        });
    } else if node.is_error() {
        problems.push(SyntaxProblem {
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            kind: SyntaxProblemKind::Unexpected,
        });
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() || child.is_missing() || child.is_error() {
            collect_problems(&child, problems);
        }
    }
}

/// Whether a line break inside `node`, and in no smaller node, is part of a
/// string's text.
///
/// Grammars name string nodes differently, and all of them say `string`,
/// `str_text`, `heredoc` or `nowdoc` somewhere in the name of the literal or
/// of its text. A token that spans a line break and is none of those is still
/// text of some kind, unless it is a comment or a preprocessor line, whose
/// layout is free. Adjacent literals a grammar groups (`concatenated_string`)
/// are separate tokens with layout between them.
fn holds_text(node: &Node) -> bool {
    let kind = node.kind();
    if kind.contains("comment") || kind.starts_with("preproc") || kind == "concatenated_string" {
        return false;
    }
    if node.is_error() {
        return false;
    }
    kind.contains("string")
        || kind.contains("str_text")
        || kind.contains("heredoc")
        || kind.contains("nowdoc")
        || node.child_count() == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_of(source: &str, byte: usize) -> usize {
        source[..byte].matches('\n').count() + 1
    }

    #[test]
    fn clean_source_reads_no_problem_in_each_language_family() {
        for (language, source) in [
            (
                LanguageId::Python,
                "class A:\n    def f(self):\n        return 1\n",
            ),
            (
                LanguageId::TypeScript,
                "export class A {\n  f(): number {\n    return 1;\n  }\n}\n",
            ),
            (
                LanguageId::Rust,
                "impl A {\n    fn f(&self) -> u8 {\n        1\n    }\n}\n",
            ),
        ] {
            let reading = read_syntax(language, source.as_bytes()).unwrap();
            assert_eq!(reading.problems, Vec::new(), "{language:?}");
            assert_eq!(reading.string_line_starts, Vec::<usize>::new());
        }
    }

    fn layout(source: &str) -> Vec<(usize, &'static str)> {
        read_syntax(LanguageId::Python, source.as_bytes())
            .unwrap()
            .problems
            .iter()
            .filter_map(|problem| match problem.kind {
                SyntaxProblemKind::Layout(what) => {
                    Some((line_of(source, problem.start_byte), what))
                }
                _ => None,
            })
            .collect()
    }

    /// A Python body written at its `def` line's indentation is not a body.
    /// The grammar reads it as an empty function followed by loose
    /// statements, so the layout check is what says so.
    #[test]
    fn python_body_at_the_indentation_of_its_def_is_a_problem() {
        let source =
            "class A:\n    def f(self):\n    return 1\n\n    def g(self):\n        return 2\n";
        assert_eq!(layout(source), vec![(3, "expected an indented block")]);
        // The same at the top level, and at the end of the source.
        assert_eq!(
            layout("def f():\nreturn 1\n"),
            vec![(2, "expected an indented block")]
        );
        assert_eq!(
            layout("def f():\n"),
            vec![(1, "expected an indented block")]
        );
    }

    #[test]
    fn python_statements_of_one_block_share_one_indentation() {
        assert_eq!(
            layout("def f():\n    a = 1\n        b = 2\n    return a\n"),
            vec![(3, "unexpected indent")]
        );
        assert_eq!(
            layout("def f():\n        a = 1\n    b = 2\n"),
            vec![(3, "unindent does not match any outer indentation level")]
        );
        assert_eq!(layout("a = 1\n    b = 2\n"), vec![(2, "unexpected indent")]);
        assert_eq!(layout("    a = 1\n"), vec![(1, "unexpected indent")]);
        // A tab and eight spaces are not one indentation.
        assert_eq!(
            layout("def f():\n\ta = 1\n        b = 2\n"),
            vec![(3, "unexpected indent")]
        );
    }

    #[test]
    fn python_clauses_and_decorated_definitions_line_up() {
        assert_eq!(
            layout("def f(a):\n    if a:\n        return 1\n      else:\n        return 2\n"),
            vec![(4, "unindent does not match any outer indentation level")]
        );
        assert_eq!(
            layout("class A:\n    @property\n        def f(self):\n            return 1\n"),
            vec![(3, "a definition is not indented like its decorator")]
        );
    }

    /// Everything Python accepts reads without a layout problem.
    #[test]
    fn valid_python_layouts_read_clean() {
        for source in [
            "if a: b = 1\nelse: b = 2\n",
            "a = 1; b = 2\nif a:\n    c = 3; d = 4\n",
            "class A: pass\nclass B:\n    x = 1\n\n    @staticmethod\n    def f(): return 1\n\n    @property\n    def g(self):\n        return 2\n",
            "def f(a,\n      b):\n    return (a +\n            b)\n",
            "def f():\n    s = \"\"\"\ntext at the margin\n  and deeper\n\"\"\"\n    return s\n",
            "def f():\n    a = 1\n# a comment at the margin\n        # and a deep one\n    return a\n",
            "try:\n    a = 1\nexcept ValueError:\n    a = 2\nelse:\n    a = 3\nfinally:\n    a = 4\n",
            "for a in b:\n    if a:\n        continue\n    elif c:\n        break\n    else:\n        pass\nelse:\n    d = 1\nwhile a:\n    a -= 1\nelse:\n    a = 0\n",
            "match a:\n    case 1:\n        b = 1\n    case _:\n        b = 2\n",
            "async def f(a):\n    async with a as b:\n        async for c in b:\n            await c\n    return [\n        x\n        for x in a\n    ]\n",
            "def outer():\n    def inner():\n        return 1\n    return inner\n",
            "class A:\n\tdef f(self):\n\t\treturn 1\n\n\tdef g(self):\n\t\tif self:\n\t\t\treturn 2\n\t\treturn 3\n",
            "x = 1 if a else \\\n    2\nwith a as b, \\\n        c as d:\n    pass\n",
            "@decorator(\n    1,\n)\nclass A:\n    \"\"\"Doc.\n\n    More.\n    \"\"\"\n\n    def f(self): ...\n",
            "",
            "\n\n# only a comment\n",
        ] {
            let reading = read_syntax(LanguageId::Python, source.as_bytes()).unwrap();
            assert_eq!(reading.problems, Vec::new(), "{source}");
        }
    }

    #[test]
    fn an_unclosed_bracket_is_a_problem_in_python_typescript_and_rust() {
        for (language, source, line) in [
            (
                LanguageId::Python,
                "def f(a):\n    return (a + 1\n\ndef g():\n    return 2\n",
                2,
            ),
            (
                LanguageId::TypeScript,
                "function f(a: number): number {\n  return g(a;\n}\n",
                2,
            ),
            (LanguageId::Rust, "fn f(a: u8) -> u8 {\n    g(a\n}\n", 2),
        ] {
            let reading = read_syntax(language, source.as_bytes()).unwrap();
            assert!(!reading.problems.is_empty(), "{language:?}");
            let lines: Vec<usize> = reading
                .problems
                .iter()
                .map(|problem| line_of(source, problem.start_byte))
                .collect();
            println!("{language:?}: {:?} at lines {lines:?}", reading.problems);
            assert!(
                lines.iter().any(|at| (line..=line + 1).contains(at)),
                "{language:?}: {lines:?}"
            );
        }
    }

    /// A problem the source already had beside a change is not the change's.
    #[test]
    fn only_problems_a_change_brought_are_introduced() {
        let before_source =
            "a = 1\n    b = 2\n\ndef f():\n    return 1\n\ndef g():\n    c = 3\n        d = 4\n";
        let before = read_syntax(LanguageId::Python, before_source.as_bytes()).unwrap();
        assert_eq!(before.problems.len(), 2, "{before:?}");
        let held = "def f():\n    return 1";
        let start = before_source.find(held).unwrap();
        let replaced = |now: &str| {
            vec![Replaced {
                before: start..start + held.len(),
                now_len: now.len(),
            }]
        };
        // A clean replacement, longer than what it replaces: the two
        // problems are the ones the source had, one of them moved.
        let now = "def f():\n    value = 2\n    return value";
        let after_source = before_source.replace(held, now);
        let after = read_syntax(LanguageId::Python, after_source.as_bytes()).unwrap();
        assert_eq!(after.problems.len(), 2, "{after:?}");
        assert_ne!(after.problems[1], before.problems[1]);
        assert_eq!(
            after.introduced_since(&before, &replaced(now)),
            Vec::<&SyntaxProblem>::new()
        );
        // A replacement with a problem of its own.
        let now = "def f():\nreturn 1";
        let after_source = before_source.replace(held, now);
        let after = read_syntax(LanguageId::Python, after_source.as_bytes()).unwrap();
        let introduced = after.introduced_since(&before, &replaced(now));
        assert_eq!(introduced.len(), 1, "{introduced:?}");
        assert_eq!(
            introduced[0].kind,
            SyntaxProblemKind::Layout("expected an indented block")
        );
        assert_eq!(line_of(&after_source, introduced[0].start_byte), 5);
        assert_eq!(
            Replaced::now(&replaced(now)),
            vec![start..start + now.len()]
        );
        // A replacement that covers a problem the source had takes it over:
        // what stands there now is judged as new.
        let whole = "a = 1\n    b = 2";
        let covering = vec![Replaced {
            before: 0..whole.len(),
            now_len: whole.len(),
        }];
        let same = read_syntax(LanguageId::Python, before_source.as_bytes()).unwrap();
        assert_eq!(same.introduced_since(&before, &covering).len(), 1);
    }

    #[test]
    fn a_docstring_is_told_from_any_other_string() {
        let source = "\"\"\"Module.\n\nMore.\n\"\"\"\n\ndef f():\n    \"\"\"Doc.\n\n    More.\n    \"\"\"\n    text = \"\"\"\n    not a docstring\n    \"\"\"\n    return text\n";
        let reading = read_syntax(LanguageId::Python, source.as_bytes()).unwrap();
        let lines = |starts: &[usize]| -> Vec<usize> {
            starts.iter().map(|start| line_of(source, *start)).collect()
        };
        assert_eq!(
            lines(&reading.string_line_starts),
            vec![2, 3, 4, 8, 9, 10, 12, 13]
        );
        assert_eq!(
            lines(&reading.docstring_line_starts),
            vec![2, 3, 4, 8, 9, 10]
        );
    }

    #[test]
    fn code_is_told_from_the_comments_and_space_around_it() {
        let python = "def f():\n    return 1\n    # after\n\nx = 2  # trailing\n";
        let after = python.find("\n    # after").unwrap();
        let assignment = python.find("x = 2").unwrap();
        assert_eq!(
            first_code_in(LanguageId::Python, python.as_bytes(), after..python.len()),
            Some(assignment)
        );
        assert_eq!(
            first_code_in(LanguageId::Python, python.as_bytes(), after..assignment),
            None
        );
        assert_eq!(
            first_code_in(
                LanguageId::Python,
                python.as_bytes(),
                assignment + "x = 2".len()..python.len()
            ),
            None
        );
        let rust = "fn f() {}\n/* block\n   comment */ // line\nfn g() {}\n";
        assert_eq!(
            first_code_in(
                LanguageId::Rust,
                rust.as_bytes(),
                "fn f() {}".len()..rust.len()
            ),
            rust.find("fn g")
        );
    }

    #[test]
    fn a_missing_token_is_named() {
        let source = "function f(a: number): number {\n  return g(a;\n}\n";
        let reading = read_syntax(LanguageId::TypeScript, source.as_bytes()).unwrap();
        assert!(
            reading
                .problems
                .iter()
                .any(|problem| problem.kind == SyntaxProblemKind::Missing(")".into())),
            "{reading:?}"
        );
    }

    /// The lines of a multi-line literal are content; the lines of code
    /// around it, a comment and an interpolation are layout.
    #[test]
    fn only_lines_inside_a_string_literal_start_inside_one() {
        let inside = |language, source: &str| -> Vec<usize> {
            read_syntax(language, source.as_bytes())
                .unwrap()
                .string_line_starts
                .iter()
                .map(|start| line_of(source, *start))
                .collect()
        };
        assert_eq!(
            inside(
                LanguageId::Python,
                "def f():\n    \"\"\"Doc.\n\n    More.\n    \"\"\"\n    x = (\n        1\n    )\n    # note\n    return x\n"
            ),
            vec![3, 4, 5]
        );
        assert_eq!(
            inside(
                LanguageId::Python,
                "def f(a):\n    return (\"one\"\n            \"two\")\n"
            ),
            Vec::<usize>::new()
        );
        assert_eq!(
            inside(
                LanguageId::TypeScript,
                "function f(a: number) {\n  /* a\n     b */\n  return `x\n  y ${\n    a\n  } z\nw`;\n}\n"
            ),
            vec![5, 8]
        );
        assert_eq!(
            inside(
                LanguageId::Rust,
                "fn f() -> &'static str {\n    r#\"a\nb\"#\n}\n"
            ),
            vec![3]
        );
        assert_eq!(
            inside(
                LanguageId::Go,
                "package p\n\nfunc f() string {\n\treturn `a\nb`\n}\n"
            ),
            vec![5]
        );
    }
}
