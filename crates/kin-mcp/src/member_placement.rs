// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Where a declaration is written among a type's members or a unit's
//! declarations, and how, in any language whose adapter implements
//! [`kin_parser::member_placement::MemberPlacement`].
//!
//! A method is added to a type by naming a method that is already there, or
//! the type itself, and sending the new method's own text. The type is never
//! read back or resent: an application class can run to tens of kilobytes,
//! and an agent that has to patch it to add twelve lines has to hold all of
//! it. Everything here is computed from the unit's exact bytes and the span
//! the graph holds for the anchor, so a caller names entities and never a
//! line or an offset.
//!
//! The text of the new declaration is taken as the caller wrote it and set
//! at the indentation of the place it goes. Three ways of writing it are
//! read as the same declaration: starting at the left margin, indented as a
//! whole, and with its first line at the margin and the rest indented the
//! way a method's source is served. A line the language says is content, a
//! line that begins inside a string laid out with no code around it, is
//! written exactly as sent. This file names no language: the hook says where
//! a declaration stands and whether text is the one declaration it names.

use std::ops::Range;

use kin_model::LanguageId;
pub use kin_parser::member_placement::{MemberPlacement, Shape, Site};

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// One declaration as it is written into a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// Its text: its first line from its first character, every later line
    /// at the indentation of its place.
    pub text: String,
    /// Where in [`Self::text`] its entity's span starts: past a doc comment
    /// or a decorator its language keeps outside the span.
    pub declaration_start: usize,
}

/// `body`, one declaration of `shape` named `name`, as it is written at
/// `indent`: its first line from its first character, and every later line
/// at `indent` plus its own depth. `member` says whether it stands as a
/// member of a type. `sibling` is the indentation of a declaration beside
/// it and of that declaration's body, which tells a body sent the way source
/// is served, first line at the margin and the rest at their place in the
/// unit, from one written at the margin throughout.
#[allow(clippy::too_many_arguments)]
pub fn written(
    language: LanguageId,
    hook: &dyn MemberPlacement,
    body: &str,
    name: &str,
    shape: Shape,
    member: bool,
    indent: &str,
    sibling: Option<(&str, &str)>,
) -> Result<Written, String> {
    let raw: Vec<&str> = body
        .trim_end()
        .split('\n')
        .skip_while(|line| line.trim().is_empty())
        .collect();
    let Some(first) = raw.first() else {
        return Err("EntityCreate requires the exact nonempty body of one declaration".into());
    };
    // Indented as a whole: set at the margin.
    let lead = leading_whitespace(first).to_string();
    let mut lines: Vec<String> = raw
        .iter()
        .map(|line| line.strip_prefix(lead.as_str()).unwrap_or(line).to_string())
        .collect();
    // First line at the margin and the rest where the unit had them.
    if let (true, Some((beside, beside_body))) = (lead.is_empty() && !indent.is_empty(), sibling) {
        let step = beside_body.strip_prefix(beside).unwrap_or_default();
        let later: Vec<&String> = lines[1..]
            .iter()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let shallowest = later
            .iter()
            .map(|line| leading_whitespace(line).len())
            .min();
        if !step.is_empty()
            && shallowest == Some(indent.len() + step.len())
            && later.iter().all(|line| line.starts_with(indent))
        {
            for line in lines[1..].iter_mut() {
                if let Some(rest) = line.strip_prefix(indent) {
                    *line = rest.to_string();
                }
            }
        }
    }
    let margin = lines.join("\n");
    // Said by its line in what was sent, before the shape of it is asked
    // about. A member is read inside the shell its language needs around it.
    crate::syntax_gate::refuse_unparseable_within(
        language,
        hook.shell(member),
        &margin,
        &format!("the code sent for {name}"),
    )?;
    let starts = hook.check_one_declaration(&margin, name, shape, member)?;
    let content = hook.verbatim_lines(&margin, member)?;
    // The line the declaration's span starts on, and where on it.
    let start_line = margin[..starts].matches('\n').count();
    let start_column = starts
        - margin[..starts]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
    let mut out = String::with_capacity(body.len() + indent.len() * lines.len());
    let mut declaration_start = 0;
    for (at, line) in lines.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        let line_start = out.len();
        if at == 0 {
            out.push_str(line);
        } else if content.get(at).copied().unwrap_or(false) {
            out.push_str(raw[at]);
        } else if !line.trim().is_empty() {
            out.push_str(indent);
            out.push_str(line);
        }
        if at == start_line {
            let moved = if at == 0 { 0 } else { indent.len() };
            declaration_start = (line_start + moved + start_column).min(out.len());
        }
    }
    Ok(Written {
        text: out,
        declaration_start,
    })
}

/// Text to write into a unit, and the bytes of it that are the new
/// declaration itself, which the graph will span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Insertion {
    pub at: usize,
    pub text: String,
    pub declaration: String,
}

/// Write `body` as the declaration after `site`, in the scope `site` is in,
/// as far from it as the language sets two such declarations apart.
pub fn sibling_after(
    language: LanguageId,
    hook: &dyn MemberPlacement,
    site: &Site,
    body: &str,
    name: &str,
    shape: Shape,
) -> Result<Insertion, String> {
    if !site.top_level && !site.member {
        return Err(
            "a declaration can be created beside one written in a module or in a class body, \
             not one nested in a function"
                .into(),
        );
    }
    let sibling = site
        .body_indent
        .as_deref()
        .map(|body_indent| (site.indent.as_str(), body_indent));
    let written = written(
        language,
        hook,
        body,
        name,
        shape,
        site.member,
        &site.indent,
        sibling,
    )?;
    Ok(Insertion {
        at: site.extent.end,
        text: format!("{}{}{}", hook.gap_after(site), site.indent, written.text),
        declaration: written.text[written.declaration_start..].to_string(),
    })
}

/// Write `body` as the last member of the type at `class`.
pub fn last_member(
    language: LanguageId,
    hook: &dyn MemberPlacement,
    class: &Site,
    body: &str,
    name: &str,
    shape: Shape,
) -> Result<Insertion, String> {
    if class.shape != Shape::Class {
        return Err("placement member needs a class as its anchor".into());
    }
    let Some(at) = class.members_end else {
        return Err(hook.members_elsewhere(class));
    };
    let Some(member_indent) = class.body_indent.as_deref() else {
        return Err(
            "the class keeps its body on its header line, so a member has no indentation to \
             take; patch the class instead"
                .into(),
        );
    };
    // A member's body is one step deeper than the member, the step the
    // type's own body takes.
    let step = member_indent
        .strip_prefix(class.indent.as_str())
        .unwrap_or_default();
    let member_body = format!("{member_indent}{step}");
    let written = written(
        language,
        hook,
        body,
        name,
        shape,
        true,
        member_indent,
        Some((member_indent, member_body.as_str())),
    )?;
    Ok(Insertion {
        at,
        text: format!("\n\n{member_indent}{}", written.text),
        declaration: written.text[written.declaration_start..].to_string(),
    })
}

/// The bytes to take out to remove the declaration at `site`: its lines,
/// whatever its language reads as its own around it, and the space that
/// set it apart, so its neighbours stay as far apart as they were from it
/// and the last member leaves no blank line behind
/// ([`kin_parser::code_placement::removal_of`]).
pub fn member_removal(source: &[u8], site: &Site) -> Range<usize> {
    kin_parser::code_placement::removal_of(source, site.extent.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python() -> &'static dyn MemberPlacement {
        kin_parser::member_placement::member_placement_for(LanguageId::Python).unwrap()
    }

    fn locate(source: &[u8], span: Range<usize>) -> Result<Site, String> {
        python().locate(source, span)
    }

    fn parses(source: &[u8]) -> bool {
        python().parses(source)
    }

    const SOURCE: &str = "import os\n\n\nclass App(Base):\n    \"\"\"Doc.\"\"\"\n\n    name = \"app\"\n\n    def first(self):\n        return 1\n\n    @property\n    def second(self):\n        def inner():\n            return 2\n\n        return inner()\n\n\ndef top():\n    return 1\n";

    fn span_of(needle_start: &str, needle_end: &str) -> Range<usize> {
        let start = SOURCE.find(needle_start).unwrap();
        let end = SOURCE[start..].find(needle_end).unwrap() + start + needle_end.len();
        start..end
    }

    fn apply(insertion: &Insertion) -> String {
        let mut out = SOURCE.to_string();
        out.insert_str(insertion.at, &insertion.text);
        out
    }

    #[test]
    fn a_definition_is_located_with_its_scope_and_indentation() {
        let first = locate(SOURCE.as_bytes(), span_of("def first", "return 1")).unwrap();
        assert_eq!(
            (
                first.shape,
                first.indent.as_str(),
                first.top_level,
                first.member
            ),
            (Shape::Function, "    ", false, true)
        );
        assert_eq!(first.body_indent.as_deref(), Some("        "));
        let second = locate(SOURCE.as_bytes(), span_of("@property", "return inner()")).unwrap();
        assert!(second.member && second.shape == Shape::Function);
        let class = locate(SOURCE.as_bytes(), span_of("class App", "return inner()")).unwrap();
        assert_eq!(
            (class.shape, class.top_level, class.body_indent.as_deref()),
            (Shape::Class, true, Some("    "))
        );
        let top = locate(SOURCE.as_bytes(), span_of("def top", "return 1")).unwrap();
        assert!(top.top_level && !top.member && top.indent.is_empty());
        // A function nested in a method is neither, and part of a
        // definition is no definition.
        let inner = locate(SOURCE.as_bytes(), span_of("def inner", "return 2")).unwrap();
        assert!(!inner.top_level && !inner.member);
        assert!(locate(SOURCE.as_bytes(), span_of("def first", "(self)")).is_err());
        assert!(!first.only_statement && !top.only_statement);
    }

    /// A method that is all its class holds is known as that, a comment
    /// beside it or not, and a docstring is a statement the class keeps.
    #[test]
    fn a_method_that_is_all_its_class_holds_is_known() {
        let method = "def one(self):\n        return 1";
        for (source, only) in [
            ("class Only:\n    def one(self):\n        return 1\n", true),
            (
                "class Only:\n    # why\n    def one(self):\n        return 1\n\n\ndef top():\n    return 2\n",
                true,
            ),
            (
                "class Only:\n    \"\"\"Doc.\"\"\"\n\n    def one(self):\n        return 1\n",
                false,
            ),
        ] {
            let start = source.find(method).unwrap();
            let site = locate(source.as_bytes(), start..start + method.len()).unwrap();
            assert_eq!(site.only_statement, only, "{source}");
            // The grammar accepts the class without it either way, which is
            // why a second parse cannot answer this.
            let mut without = source.as_bytes().to_vec();
            without.drain(member_removal(source.as_bytes(), &site));
            assert!(parses(&without), "{source}");
        }
    }

    /// The three ways an agent writes a method are one method at the class's
    /// indentation.
    #[test]
    fn a_method_is_written_at_its_siblings_indentation_however_it_was_sent() {
        let first = locate(SOURCE.as_bytes(), span_of("def first", "return 1")).unwrap();
        let expected = "def third(self, value):\n        \"\"\"Third.\n\n        More.\n        \"\"\"\n        if value:\n            return 3\n        return 0";
        for sent in [
            // At the margin.
            "def third(self, value):\n    \"\"\"Third.\n\n    More.\n    \"\"\"\n    if value:\n        return 3\n    return 0\n",
            // Indented as a whole.
            "    def third(self, value):\n        \"\"\"Third.\n\n        More.\n        \"\"\"\n        if value:\n            return 3\n        return 0",
            // As source is served: first line at the margin, the rest in place.
            "def third(self, value):\n        \"\"\"Third.\n\n        More.\n        \"\"\"\n        if value:\n            return 3\n        return 0",
        ] {
            let insertion = sibling_after(LanguageId::Python, python(), &first, sent, "third", Shape::Function).unwrap();
            assert_eq!(insertion.declaration, expected, "{sent}");
            assert_eq!(insertion.at, first.span.end);
            let result = apply(&insertion);
            assert!(
                result.contains(
                    "        return 1\n\n    def third(self, value):\n        \"\"\"Third.\n"
                ) && result.contains("        return 0\n\n    @property\n"),
                "{result}"
            );
            assert!(parses(result.as_bytes()), "{result}");
            // The new method's bytes sit where the insertion says.
            let at = insertion.at + insertion.text.len() - insertion.declaration.len();
            assert_eq!(&result[at..at + expected.len()], expected);
        }
    }

    #[test]
    fn a_string_that_is_not_a_docstring_keeps_its_own_lines() {
        let first = locate(SOURCE.as_bytes(), span_of("def first", "return 1")).unwrap();
        let sent = "def query(self):\n    \"\"\"Doc\n    line.\n    \"\"\"\n    return \"\"\"\nSELECT 1\n  FROM t\n\"\"\"\n";
        let insertion = sibling_after(
            LanguageId::Python,
            python(),
            &first,
            sent,
            "query",
            Shape::Function,
        )
        .unwrap();
        assert_eq!(
            insertion.declaration,
            "def query(self):\n        \"\"\"Doc\n        line.\n        \"\"\"\n        return \"\"\"\nSELECT 1\n  FROM t\n\"\"\""
        );
        assert!(parses(apply(&insertion).as_bytes()));
    }

    #[test]
    fn a_member_is_appended_to_a_class_and_a_class_after_a_declaration() {
        let class = locate(SOURCE.as_bytes(), span_of("class App", "return inner()")).unwrap();
        let insertion = last_member(
            LanguageId::Python,
            python(),
            &class,
            "def last(self):\n    return 9\n",
            "last",
            Shape::Function,
        )
        .unwrap();
        let result = apply(&insertion);
        assert!(
            result.contains(
                "        return inner()\n\n    def last(self):\n        return 9\n\n\ndef top():"
            ),
            "{result}"
        );
        assert!(parses(result.as_bytes()));

        let created = sibling_after(
            LanguageId::Python,
            python(),
            &class,
            "class Other:\n    limit = 3\n\n    def get(self):\n        return self.limit\n",
            "Other",
            Shape::Class,
        )
        .unwrap();
        let result = apply(&created);
        assert!(
            result.contains(
                "        return inner()\n\n\nclass Other:\n    limit = 3\n\n    def get(self):\n        return self.limit\n\n\ndef top():"
            ),
            "{result}"
        );
        assert!(parses(result.as_bytes()));
        // A class written on one line gives a member nothing to line up with.
        let one_line = "class Tiny: pass\n";
        let tiny = locate(one_line.as_bytes(), 0..one_line.len() - 1).unwrap();
        assert!(last_member(
            LanguageId::Python,
            python(),
            &tiny,
            "def f(self): pass",
            "f",
            Shape::Function
        )
        .is_err());
    }

    #[test]
    fn a_body_that_is_not_the_named_definition_alone_is_refused() {
        let first = locate(SOURCE.as_bytes(), span_of("def first", "return 1")).unwrap();
        for (sent, said) in [
            ("def other(self):\n    pass", "declares the function other"),
            ("class third:\n    pass", "declares the class third"),
            (
                "import os\ndef third(self):\n    pass",
                "exactly one function",
            ),
            (
                "def third(self):\n    pass\n\ndef fourth(self):\n    pass",
                "exactly one function",
            ),
            ("def third(self:\n    pass", "does not parse"),
            ("   \n", "nonempty"),
        ] {
            let refusal = sibling_after(
                LanguageId::Python,
                python(),
                &first,
                sent,
                "third",
                Shape::Function,
            )
            .unwrap_err();
            assert!(refusal.contains(said), "{sent}: {refusal}");
        }
        let inner = locate(SOURCE.as_bytes(), span_of("def inner", "return 2")).unwrap();
        assert!(sibling_after(
            LanguageId::Python,
            python(),
            &inner,
            "def x():\n    pass",
            "x",
            Shape::Function
        )
        .unwrap_err()
        .contains("nested in a function"));
    }

    #[test]
    fn removing_a_member_takes_its_lines_and_the_space_that_set_it_apart() {
        let first = locate(SOURCE.as_bytes(), span_of("def first", "return 1")).unwrap();
        let cut = member_removal(SOURCE.as_bytes(), &first);
        let mut result = SOURCE.to_string();
        result.replace_range(cut, "");
        assert!(
            result.contains("    name = \"app\"\n\n    @property\n    def second(self):"),
            "{result}"
        );
        assert!(parses(result.as_bytes()));
        // The last member leaves the class's end as it was.
        let second = locate(SOURCE.as_bytes(), span_of("@property", "return inner()")).unwrap();
        let cut = member_removal(SOURCE.as_bytes(), &second);
        let mut result = SOURCE.to_string();
        result.replace_range(cut, "");
        assert!(
            result.contains("        return 1\n\n\ndef top():"),
            "{result}"
        );
        assert!(parses(result.as_bytes()));
        // The last member of the last class leaves no blank line behind.
        let source = "class A:\n    def one(self):\n        return 1\n\n    def two(self):\n        return 2\n";
        let start = source.find("def two").unwrap();
        let two = locate(source.as_bytes(), start..source.len() - 1).unwrap();
        let mut result = source.to_string();
        result.replace_range(member_removal(source.as_bytes(), &two), "");
        assert_eq!(result, "class A:\n    def one(self):\n        return 1\n");
    }
}
