// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! An entity's whole code, as a caller wrote it, at the indentation its place
//! in the source has.
//!
//! An entity's code is read from its first character: a method's first line
//! has no indentation and every later line carries the source's. A caller
//! that replaces the whole code sends it one of three ways, and all three
//! mean the same code:
//!
//! - as it was read, first line at the margin and the rest where the source
//!   has them;
//! - at the margin throughout, the way anyone writes a snippet;
//! - indented as a whole, the way it looks in an editor.
//!
//! Spliced in verbatim, only the first is right. The second leaves a method's
//! body at its own header's indentation, which in Python is not a body at
//! all. So the code is placed: the lines after the first are moved as one
//! block to the entity's indentation, each keeping its depth relative to the
//! others.
//!
//! A line that begins inside a string literal is the string's content and is
//! written exactly as it was sent, except a docstring's, which is laid out
//! with the code around it and read with that layout taken off. In a language
//! where indentation is not syntax, moving the other lines changes nothing
//! but the look, so it is done only for code not already standing at its
//! place.

use std::ops::Range;

use kin_model::LanguageId;

use crate::syntax_reading::{indentation_is_syntax, read_syntax, Replaced};

/// The whitespace the line holding `start` carries ahead of it, or the empty
/// string when `start` is not the first thing on its line.
pub fn indentation_at(source: &[u8], start: usize) -> &str {
    let start = start.min(source.len());
    let line_start = source[..start]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1);
    let indent = &source[line_start..start];
    if indent.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
        std::str::from_utf8(indent).unwrap_or_default()
    } else {
        ""
    }
}

fn lead(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// `code` with the blank lines around it dropped, its first line at the
/// margin, and, when it was indented as a whole, every line set back by the
/// first line's indentation.
fn at_first_column(code: &str) -> Vec<String> {
    let raw: Vec<&str> = code
        .trim_end()
        .split('\n')
        .skip_while(|line| blank(line))
        .collect();
    let Some(first) = raw.first() else {
        return Vec::new();
    };
    let whole = lead(first);
    let as_a_whole = !whole.is_empty()
        && raw[1..]
            .iter()
            .all(|line| blank(line) || line.starts_with(whole));
    raw.iter()
        .enumerate()
        .map(|(at, line)| {
            if at == 0 || as_a_whole {
                line.strip_prefix(whole).unwrap_or(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect()
}

/// `code`, one whole declaration as a caller wrote it, set at the margin: the
/// blank lines around it dropped and, when it was indented as a whole, every
/// line set back by the first line's indentation.
pub fn at_margin(code: &str) -> String {
    at_first_column(code).join("\n")
}

/// The start of the line that holds `at`.
fn line_start(source: &[u8], at: usize) -> usize {
    source[..at.min(source.len())]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1)
}

/// The end of the line that holds `at`: the offset of its line break, or the
/// end of the source.
fn line_end(source: &[u8], at: usize) -> usize {
    let at = at.min(source.len());
    source[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |offset| at + offset)
}

/// The next line at or after `from` that holds anything but space, as where
/// the line starts and where its first such byte is.
fn next_code_line(source: &[u8], from: usize) -> Option<(usize, usize)> {
    let from = from.min(source.len());
    let first = from
        + source[from..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())?;
    Some((line_start(source, first), first))
}

/// Whether the line whose first code is at `first` holds a declaration that
/// stands beside one indented by `indent`: indented the same, and not the
/// bracket that closes the scope both would stand in.
fn stands_beside(source: &[u8], start: usize, first: usize, indent: &str) -> bool {
    &source[start..first] == indent.as_bytes() && !matches!(source[first], b'}' | b')' | b']')
}

/// How many lines of only space stand directly above the line that starts at
/// `start`, and where the first of them starts.
fn blank_lines_above(source: &[u8], start: usize) -> (usize, usize) {
    let (mut count, mut top) = (0, start);
    while top > 0 {
        let above = line_start(source, top - 1);
        if !source[above..top - 1]
            .iter()
            .all(|byte| byte.is_ascii_whitespace())
        {
            break;
        }
        count += 1;
        top = above;
    }
    (count, top)
}

/// Where a declaration goes after the one at `anchor` in `source`, and what
/// is written ahead of it there: line breaks and the anchor's indentation.
///
/// The new declaration stands as far from the anchor as the anchor's
/// neighbours stand from it. When a declaration follows the anchor, the
/// space between the two is written again ahead of the new one, so it
/// stands between them exactly as far from each as they stood from one
/// another. When the anchor is the last in its scope, the blank lines
/// between it and the declaration above it are. An anchor with no neighbour
/// says nothing about its source, and `otherwise` blank lines are written.
pub fn after_sibling(source: &[u8], anchor: Range<usize>, otherwise: usize) -> (usize, String) {
    let indent = indentation_at(source, anchor.start);
    // Whatever shares the anchor's last line, a comment or a semicolon,
    // stays on it.
    let at = line_end(source, anchor.end);
    if let Some((start, first)) = next_code_line(source, at) {
        if start > at && stands_beside(source, start, first, indent) {
            let between = String::from_utf8_lossy(&source[at..start]);
            return (at, format!("{between}{indent}"));
        }
    }
    // A comment that stands directly above the anchor is the anchor's, so
    // the blank lines are counted above it.
    let mut own_line = line_start(source, anchor.start);
    while own_line > 0 {
        let above = line_start(source, own_line - 1);
        let text = &source[above..own_line - 1];
        let code = &text[indentation_of_line(source, above).len().min(text.len())..];
        let comment = [&b"#"[..], b"//", b"/*", b"*"]
            .iter()
            .any(|opener| code.starts_with(opener));
        if !comment || indentation_of_line(source, above) != indent.as_bytes() {
            break;
        }
        own_line = above;
    }
    let (above, top) = blank_lines_above(source, own_line);
    // The line above the blank ones is a neighbour when it is indented as
    // the anchor is; indented less, it opens the scope the anchor is in.
    let neighbour_above = top > 0 && {
        let line = line_start(source, top - 1);
        indentation_of_line(source, line).len() >= indent.len()
    };
    let blank = if neighbour_above { above } else { otherwise };
    (at, format!("{}{indent}", "\n".repeat(blank + 1)))
}

/// The whitespace the line that starts at `start` opens with.
fn indentation_of_line(source: &[u8], start: usize) -> &[u8] {
    let width = source[start..]
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(source.len() - start);
    &source[start..start + width]
}

/// The bytes to take out of `source` to remove the declaration at `span`, so
/// that what stood around it stands as far apart as its neighbours did.
///
/// The declaration goes with its own lines. When a declaration follows it,
/// the space after it goes too, and the one above and the one below are left
/// as far apart as the removed one stood from the one above. When it is the
/// last in its scope, the blank lines above it go instead. A declaration
/// that shares a line with other code is taken out alone.
pub fn removal_of(source: &[u8], span: Range<usize>) -> Range<usize> {
    let start = line_start(source, span.start);
    let last = line_end(source, span.end);
    let own_lines = source[start..span.start]
        .iter()
        .chain(&source[span.end..last])
        .all(|byte| byte.is_ascii_whitespace());
    if !own_lines {
        return span;
    }
    let indent = indentation_at(source, span.start);
    // Past the line break that ends the declaration's last line.
    let end = (last + 1).min(source.len());
    if let Some((next, first)) = next_code_line(source, end) {
        if stands_beside(source, next, first, indent) {
            return start..next;
        }
    }
    let (_, top) = blank_lines_above(source, start);
    top..end
}

/// Byte offset of the start of each line of `lines` joined by line breaks.
fn line_starts(lines: &[String]) -> Vec<usize> {
    let mut at = 0;
    lines
        .iter()
        .map(|line| {
            let start = at;
            at += line.len() + 1;
            start
        })
        .collect()
}

/// `source` with the bytes of `span` replaced by `code`.
fn with_code(source: &[u8], span: &Range<usize>, code: &str) -> Vec<u8> {
    let mut placed = Vec::with_capacity(source.len() + code.len());
    placed.extend_from_slice(&source[..span.start]);
    placed.extend_from_slice(code.as_bytes());
    placed.extend_from_slice(&source[span.end..]);
    placed
}

/// `code`, the whole code of the entity at `span` in `source` as a caller
/// wrote it, as it is written there: its first line from its first
/// character, and every later line at the entity's indentation plus its own
/// depth.
///
/// Code already written that way comes back as it is, apart from blank lines
/// around it. Nothing here decides whether the result parses; the write path
/// reads the result for that.
pub fn place_entity_code(
    language: LanguageId,
    source: &[u8],
    span: Range<usize>,
    code: &str,
) -> String {
    let lines = at_first_column(code);
    if lines.is_empty() || span.start > span.end || span.end > source.len() {
        return code.to_string();
    }
    let margin = lines.join("\n");
    let place = indentation_at(source, span.start);
    if place.is_empty() || lines.len() == 1 {
        return margin;
    }
    let starts = line_starts(&lines);
    let stands = |at: usize| blank(&lines[at]) || lines[at].starts_with(place);

    if indentation_is_syntax(language) {
        // Read at the margin, where a definition is whole whichever way its
        // later lines were indented.
        let reading = read_syntax(language, margin.as_bytes()).unwrap_or_default();
        let counted = |at: usize| !reading.starts_inside_string(starts[at]);
        // Standing at its place means every line of code starts with the
        // entity's indentation and the source still parses with it there. A
        // body written at the margin of a method one step deep passes the
        // first test and fails the second.
        let stands_there = (1..lines.len()).all(|at| !counted(at) || stands(at)) && {
            let replaced = [Replaced {
                before: span.clone(),
                now_len: margin.len(),
            }];
            match (
                read_syntax(language, source),
                read_syntax(language, &with_code(source, &span, &margin)),
            ) {
                (Some(before), Some(after)) => {
                    after.introduced_since(&before, &replaced).is_empty()
                }
                _ => true,
            }
        };
        if stands_there {
            return margin;
        }
        return moved(&lines, place, |at| {
            reading.starts_inside_string(starts[at]) && !reading.starts_inside_docstring(starts[at])
        });
    }

    if (1..lines.len()).all(stands) {
        return margin;
    }
    // Which lines are a string's content is read where the code goes: a
    // method is not a whole source on its own in most of these languages.
    let reading = read_syntax(language, &with_code(source, &span, &margin)).unwrap_or_default();
    let content = |at: usize| reading.starts_inside_string(span.start + starts[at]);
    if (1..lines.len()).all(|at| content(at) || stands(at)) {
        return margin;
    }
    moved(&lines, place, content)
}

/// `lines` with `place` ahead of every line after the first that holds code,
/// leaving the lines `content` names exactly as they are.
fn moved(lines: &[String], place: &str, content: impl Fn(usize) -> bool) -> String {
    let mut out =
        String::with_capacity(lines.iter().map(|line| line.len() + place.len() + 1).sum());
    for (at, line) in lines.iter().enumerate() {
        if at > 0 {
            out.push('\n');
            if !blank(line) && !content(at) {
                out.push_str(place);
            }
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYTHON: &str = "class Circle:\n    \"\"\"A circle.\"\"\"\n\n    def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)\n\n    def name(self):\n        return \"circle\"\n";
    const SERVED: &str =
        "def area(self):\n        if self.radius < 0:\n            return 0\n        return area(self.radius)";

    fn span_of(source: &str, code: &str) -> Range<usize> {
        let start = source.find(code).expect("the fixture holds the code");
        start..start + code.len()
    }

    fn place(language: LanguageId, source: &str, held: &str, code: &str) -> String {
        place_entity_code(language, source.as_bytes(), span_of(source, held), code)
    }

    /// The three ways a caller writes a method's whole code are one code.
    #[test]
    fn a_python_method_lands_at_its_place_however_it_was_indented() {
        let wanted = "def area(self):\n        if self.radius <= 0:\n            return 0.0\n        return area(self.radius)";
        for (how, sent) in [
            ("as it was read", wanted.to_string()),
            (
                "at the margin",
                "def area(self):\n    if self.radius <= 0:\n        return 0.0\n    return area(self.radius)"
                    .to_string(),
            ),
            (
                "indented as a whole",
                "    def area(self):\n        if self.radius <= 0:\n            return 0.0\n        return area(self.radius)"
                    .to_string(),
            ),
            (
                "indented as a whole, deeper",
                "            def area(self):\n                if self.radius <= 0:\n                    return 0.0\n                return area(self.radius)\n"
                    .to_string(),
            ),
            (
                "with blank lines around it",
                format!("\n\n{wanted}\n\n"),
            ),
            (
                "at the margin with two-space steps",
                "def area(self):\n  if self.radius <= 0:\n    return 0.0\n  return area(self.radius)"
                    .to_string(),
            ),
        ] {
            let placed = place(LanguageId::Python, PYTHON, SERVED, &sent);
            if how == "at the margin with two-space steps" {
                // Its own steps are kept; only the block moves.
                assert_eq!(
                    placed,
                    "def area(self):\n      if self.radius <= 0:\n        return 0.0\n      return area(self.radius)",
                    "{how}"
                );
            } else {
                assert_eq!(placed, wanted, "{how}");
            }
        }
    }

    /// A decorator makes the `def` line a later line, which stands at the
    /// entity's own indentation when the code is sent as read.
    #[test]
    fn a_decorated_python_method_is_placed_the_same_three_ways() {
        let source =
            "class A:\n    @property\n    def size(self):\n        return 1\n\n    x = 2\n";
        let held = "@property\n    def size(self):\n        return 1";
        let wanted = "@property\n    def size(self):\n        return 2";
        for sent in [
            wanted,
            "@property\ndef size(self):\n    return 2",
            "    @property\n    def size(self):\n        return 2",
        ] {
            assert_eq!(
                place(LanguageId::Python, source, held, sent),
                wanted,
                "{sent}"
            );
        }
    }

    /// A docstring moves with the code; the text of any other string is kept
    /// exactly as it was sent.
    #[test]
    fn python_strings_keep_their_text_and_docstrings_move_with_the_code() {
        let sent = "def area(self):\n    \"\"\"Area.\n\n    In square units.\n    \"\"\"\n    table = \"\"\"\na\n  b\n\"\"\"\n    return table";
        assert_eq!(
            place(LanguageId::Python, PYTHON, SERVED, sent),
            "def area(self):\n        \"\"\"Area.\n\n        In square units.\n        \"\"\"\n        table = \"\"\"\na\n  b\n\"\"\"\n        return table"
        );
        // Sent as read, with a string whose text stands at the margin: as it is.
        let as_read = "def area(self):\n        table = \"\"\"\na\n\"\"\"\n        return table";
        assert_eq!(place(LanguageId::Python, PYTHON, SERVED, as_read), as_read);
    }

    #[test]
    fn a_top_level_entity_and_a_one_line_entity_are_only_set_at_the_margin() {
        let source = "def area(radius):\n    return radius\n";
        let held = "def area(radius):\n    return radius";
        assert_eq!(
            place(
                LanguageId::Python,
                source,
                held,
                "  def area(radius):\n      return 2 * radius\n"
            ),
            "def area(radius):\n    return 2 * radius"
        );
        let class = "class A:\n    limit = 3\n";
        assert_eq!(
            place(LanguageId::Python, class, "limit = 3", "    limit = 4"),
            "limit = 4"
        );
    }

    const TYPESCRIPT: &str = "export class Circle {\n  area(): number {\n    return area(this.radius);\n  }\n\n  name(): string {\n    return \"circle\";\n  }\n}\n";
    const TS_SERVED: &str = "area(): number {\n    return area(this.radius);\n  }";

    #[test]
    fn a_typescript_method_lands_at_its_place_however_it_was_indented() {
        let wanted = "area(): number {\n    return 2 * area(this.radius);\n  }";
        for sent in [
            wanted,
            "area(): number {\n  return 2 * area(this.radius);\n}",
            "  area(): number {\n    return 2 * area(this.radius);\n  }",
            "\tarea(): number {\n\t  return 2 * area(this.radius);\n\t}",
        ] {
            assert_eq!(
                place(LanguageId::TypeScript, TYPESCRIPT, TS_SERVED, sent),
                wanted,
                "{sent}"
            );
        }
    }

    /// Where indentation is not syntax, a string's lines are never moved:
    /// that would change a value for the sake of the look.
    #[test]
    fn a_template_literal_keeps_its_text_when_the_code_around_it_moves() {
        let sent = "area(): string {\n  return `a\n  b\nc`;\n}";
        assert_eq!(
            place(LanguageId::TypeScript, TYPESCRIPT, TS_SERVED, sent),
            "area(): string {\n    return `a\n  b\nc`;\n  }"
        );
        // Sent as read, the text at the margin does not make it look unplaced.
        let as_read = "area(): string {\n    return `a\nc`;\n  }";
        assert_eq!(
            place(LanguageId::TypeScript, TYPESCRIPT, TS_SERVED, as_read),
            as_read
        );
    }

    fn inserted(source: &str, anchor: &str, declaration: &str, otherwise: usize) -> String {
        let (at, before) = after_sibling(source.as_bytes(), span_of(source, anchor), otherwise);
        format!("{}{before}{declaration}{}", &source[..at], &source[at..])
    }

    fn removed(source: &str, held: &str) -> String {
        let cut = removal_of(source.as_bytes(), span_of(source, held));
        format!("{}{}", &source[..cut.start], &source[cut.end..])
    }

    /// A new declaration stands as far from its neighbours as they stood
    /// from one another, whatever that distance is.
    #[test]
    fn a_created_declaration_is_as_far_from_its_neighbours_as_they_are_from_each_other() {
        // Two blank lines between top-level Python definitions.
        let python = "import os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n";
        assert_eq!(
            inserted(python, "def a():\n    return 1", "def new():\n    return 3", 2),
            "import os\n\n\ndef a():\n    return 1\n\n\ndef new():\n    return 3\n\n\ndef b():\n    return 2\n"
        );
        // After the last one: as far below it as the one above it stands.
        assert_eq!(
            inserted(python, "def b():\n    return 2", "def new():\n    return 3", 1),
            "import os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n\n\ndef new():\n    return 3\n"
        );
        // One blank line between Rust functions, and none between these.
        let rust = "fn a() {}\n\nfn b() {}\n";
        assert_eq!(
            inserted(rust, "fn a() {}", "fn new() {}", 1),
            "fn a() {}\n\nfn new() {}\n\nfn b() {}\n"
        );
        let tight = "fn a() {}\nfn b() {}\n";
        assert_eq!(
            inserted(tight, "fn a() {}", "fn new() {}", 1),
            "fn a() {}\nfn new() {}\nfn b() {}\n"
        );
        assert_eq!(
            inserted(tight, "fn b() {}", "fn new() {}", 1),
            "fn a() {}\nfn b() {}\nfn new() {}\n"
        );
        // The only declaration of its source has no neighbour to read.
        assert_eq!(
            inserted(
                "def a():\n    return 1\n",
                "def a():\n    return 1",
                "def new():\n    return 3",
                2
            ),
            "def a():\n    return 1\n\n\ndef new():\n    return 3\n"
        );
        assert_eq!(
            inserted(
                "package p\n\nfunc a() {}\n",
                "func a() {}",
                "func b() {}",
                1
            ),
            "package p\n\nfunc a() {}\n\nfunc b() {}\n"
        );
        // A comment directly above the last declaration is that declaration's.
        let commented = "def a():\n    return 1\n\n\n# About b.\ndef b():\n    return 2\n";
        assert_eq!(
            inserted(commented, "def b():\n    return 2", "def new():\n    return 3", 1),
            "def a():\n    return 1\n\n\n# About b.\ndef b():\n    return 2\n\n\ndef new():\n    return 3\n"
        );
        // Indented neighbours, the last one before the bracket that closes
        // their scope, and a comment that shares the anchor's line.
        let members = "impl A {\n    fn a(&self) {} // first\n\n    fn b(&self) {}\n}\n";
        assert_eq!(
            inserted(members, "fn a(&self) {}", "fn new(&self) {}", 1),
            "impl A {\n    fn a(&self) {} // first\n\n    fn new(&self) {}\n\n    fn b(&self) {}\n}\n"
        );
        assert_eq!(
            inserted(members, "fn b(&self) {}", "fn new(&self) {}", 1),
            "impl A {\n    fn a(&self) {} // first\n\n    fn b(&self) {}\n\n    fn new(&self) {}\n}\n"
        );
    }

    /// Taking a declaration out leaves its neighbours as far apart as they
    /// were from it, and nothing where it stood.
    #[test]
    fn a_removed_declaration_leaves_the_separation_its_neighbours_had() {
        let python = "import os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n\n\ndef c():\n    return 3\n";
        assert_eq!(
            removed(python, "def b():\n    return 2"),
            "import os\n\n\ndef a():\n    return 1\n\n\ndef c():\n    return 3\n"
        );
        assert_eq!(
            removed(python, "def c():\n    return 3"),
            "import os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n"
        );
        assert_eq!(
            removed(python, "def a():\n    return 1"),
            "import os\n\n\ndef b():\n    return 2\n\n\ndef c():\n    return 3\n"
        );
        let rust = "fn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        assert_eq!(removed(rust, "fn b() {}"), "fn a() {}\n\nfn c() {}\n");
        assert_eq!(removed(rust, "fn c() {}"), "fn a() {}\n\nfn b() {}\n");
        assert_eq!(
            removed("package p\n\nfunc a() {}\n", "func a() {}"),
            "package p\n"
        );
        let members = "impl A {\n    fn a(&self) {}\n\n    fn b(&self) {}\n}\n";
        assert_eq!(
            removed(members, "fn b(&self) {}"),
            "impl A {\n    fn a(&self) {}\n}\n"
        );
        assert_eq!(
            removed(members, "fn a(&self) {}"),
            "impl A {\n    fn b(&self) {}\n}\n"
        );
        // Sharing its line with other code, it goes alone.
        assert_eq!(
            removed("fn a() {} fn b() {}\n", "fn a() {}"),
            " fn b() {}\n"
        );
    }

    #[test]
    fn a_rust_method_lands_at_its_place_however_it_was_indented() {
        let source = "impl Builder {\n    pub fn set(&mut self) -> u8 {\n        1\n    }\n}\n";
        let held = "pub fn set(&mut self) -> u8 {\n        1\n    }";
        let wanted = "pub fn set(&mut self) -> u8 {\n        2\n    }";
        for sent in [
            wanted,
            "pub fn set(&mut self) -> u8 {\n    2\n}",
            "    pub fn set(&mut self) -> u8 {\n        2\n    }",
        ] {
            assert_eq!(
                place(LanguageId::Rust, source, held, sent),
                wanted,
                "{sent}"
            );
        }
    }
}
