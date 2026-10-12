// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A Rust file's tests are cut from what a key reads, and nothing else is.
//!
//! The same cases kin-lsp's producer identity holds its own copy of the cutter
//! to, so the two read a test the same way.

use kin_derivation_inputs::{items, kept, without_tests};

#[test]
fn top_level_test_items_are_cut_and_everything_else_is_kept() {
    let source = r##"use std::fmt;

/// Kept: compiled outside tests too.
#[cfg(any(test, feature = "test-support"))]
pub fn seam() {}

#[cfg(test)]
static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn kept(text: &str) -> usize {
    #[cfg(test)]
    let _nested = '{';
    text.matches(|c: char| c == '}' || c == '"').count()
}

#[cfg( test )]
#[derive(Debug)]
pub(crate) struct Fixture<'a>(&'a [u8; 4]);

#[cfg(test)]
mod tests {
    use super::*;

    const BRACE: char = '}';
    const QUOTE: char = '\'';
    const TEXT: &str = "} #[cfg(test)] mod inner { \" ";
    const RAW: &str = r#"
}
#[cfg(test)]
mod not_a_module {
"#;

    /* a comment with a brace } and /* a nested one } */ */
    #[test]
    fn runs<'a>(_: &'a str) {
        let _ = (BRACE, QUOTE, TEXT, RAW, b'}', br#"}"#);
    }
}

pub fn also_kept() {}

#[cfg(test)]
mod declared_elsewhere;

#[cfg(test)]
impl fmt::Debug for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("}")
    }
}
"##;
    let kept = without_tests(source).unwrap();
    for stays in [
        "use std::fmt;",
        "#[cfg(any(test, feature = \"test-support\"))]\npub fn seam() {}",
        "pub fn kept(text: &str) -> usize {",
        "    #[cfg(test)]\n    let _nested = '{';",
        "pub fn also_kept() {}",
    ] {
        assert!(kept.contains(stays), "lost `{stays}` from:\n{kept}");
    }
    for goes in [
        "COUNTER",
        "Fixture",
        "mod tests",
        "not_a_module",
        "declared_elsewhere",
        "Hidden",
        "#[derive(Debug)]",
    ] {
        assert!(!kept.contains(goes), "kept `{goes}` in:\n{kept}");
    }

    // Changing only what was cut leaves the kept text as it was.
    let edited = source
        .replace("fn runs<'a>", "fn runs_differently<'a>")
        .replace("AtomicU64::new(0)", "AtomicU64::new(7)");
    assert_ne!(edited, source);
    assert_eq!(without_tests(&edited).unwrap(), kept);
}

#[test]
fn a_test_item_whose_end_cannot_be_found_is_an_error() {
    assert!(without_tests("#[cfg(test)]\nmod tests {\n    fn open() {\n").is_err());
    assert!(without_tests("pub fn f() {\n").is_err());
    assert!(without_tests("pub fn f() {}\n}\n").is_err());
    assert_eq!(
        without_tests("pub fn f() {}\n").as_deref(),
        Ok("pub fn f() {}")
    );
}

#[test]
fn a_test_and_the_lines_around_it_leave_no_trace() {
    let plain = without_tests("fn a() {}\n\nfn b() {}\n").unwrap();
    for with_a_test in [
        // The first test a file gets, at its end.
        "fn a() {}\n\nfn b() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n}\n",
        // One between two items, with its own comment and a second attribute.
        "fn a() {}\n\n/// What the tests below cover.\n#[allow(dead_code)]\n#[cfg(test)]\n\
         mod tests {}\n\n\n\nfn b() {}\n",
        // Several, and a test-only import ahead of everything.
        "#[cfg(test)]\nuse std::fmt;\nfn a() {}\n#[cfg(test)]\nmod one {}\n#[cfg(test)]\n\
         mod two {}\nfn b() {}",
    ] {
        assert_eq!(without_tests(with_a_test).unwrap(), plain, "{with_a_test}");
    }
    // What is kept is kept byte for byte, comments included.
    assert_ne!(
        without_tests("fn a() {}\n\n// why b exists\nfn b() {}\n").unwrap(),
        plain
    );
    assert_ne!(without_tests("fn a() { }\n\nfn b() {}\n").unwrap(), plain);
    // An inner attribute configures the file and stays, whatever follows it.
    assert_eq!(
        without_tests("#![allow(dead_code)]\n#[cfg(test)]\nmod tests {}\nfn a() {}\n").unwrap(),
        "#![allow(dead_code)]\nfn a() {}"
    );
}

fn paths(source: &str) -> Vec<String> {
    fn walk(items: &[kin_derivation_inputs::RustItem], out: &mut Vec<String>) {
        for item in items {
            out.push(item.path.clone());
            walk(&item.members, out);
        }
    }
    let mut out = Vec::new();
    walk(&items(source).unwrap(), &mut out);
    out
}

#[test]
fn items_are_named_by_kind_name_and_block() {
    let source = r#"#![allow(dead_code)]
use crate::name_syntax::{NameSyntax, Quote};
pub(crate) const LIMIT: usize = 4;
static mut COUNT: u64 = 0;
/// Docs above.
#[inline]
pub async unsafe fn parse_module<'a>(text: &'a str) -> Result<(), ()> where 'a: 'a { Ok(()) }
pub struct Python<T> { inner: T }
pub enum Kind { A, B }
pub trait Adapter: Send {
    const NAME: &'static str;
    fn name_syntax(&self) -> Option<NameSyntax> { None }
    fn parse(&self, source: &[u8]) -> Vec<u8>;
}
impl<'a, F: Fn() -> u8> Adapter for Python<&'a F> where F: Send {
    const NAME: &'static str = "python";
    fn name_syntax(&self) -> Option<NameSyntax> { Some(NameSyntax) }
    fn parse(&self, source: &[u8]) -> Vec<u8> { source.to_vec() }
}
mod inner {
    pub fn helper() {}
}
macro_rules! rule { () => {}; }
thread_local! { static CELL: u8 = 0; }
"#;
    assert_eq!(
        paths(source),
        [
            "#![allow(dead_code)]",
            "use crate::name_syntax::{NameSyntax,Quote}",
            "const LIMIT",
            "static COUNT",
            "fn parse_module",
            "struct Python",
            "enum Kind",
            "trait Adapter",
            "trait Adapter / const NAME",
            "trait Adapter / fn name_syntax",
            "trait Adapter / fn parse",
            "impl Adapter for Python<&'a F>",
            "impl Adapter for Python<&'a F> / const NAME",
            "impl Adapter for Python<&'a F> / fn name_syntax",
            "impl Adapter for Python<&'a F> / fn parse",
            "mod inner",
            "mod inner / fn helper",
            "macro_rules! rule",
            "thread_local!",
        ]
    );
}

#[test]
fn kept_text_drops_comments_tests_and_named_items_only() {
    let source = "// head\nfn a() { 1 } // tail\n#[cfg(test)]\nmod tests { fn t() {} }\n\
                  impl X { fn keep(&self) {} fn drop(&self) { \"} /* not a comment */\" } }\n";
    let plain = kept(source, &[]).unwrap();
    assert_eq!(
        plain.text,
        "fn a ( ) { 1 } impl X { fn keep ( & self ) { } fn drop ( & self ) { \"} /* not a comment */\" } } "
    );
    assert!(plain.identifiers.contains("drop") && !plain.identifiers.contains("t"));

    let without = kept(source, &["impl X / fn drop".to_string()]).unwrap();
    assert_eq!(
        without.text,
        "fn a ( ) { 1 } impl X { fn keep ( & self ) { } } "
    );
    assert_eq!(
        without.excluded,
        [("impl X / fn drop".to_string(), vec!["drop".to_string()])]
    );
    assert!(!without.identifiers.contains("drop"));

    let error = kept(source, &["fn missing".to_string()]).unwrap_err();
    assert!(
        error.contains("holds no item `fn missing`") && error.contains("impl X / fn keep"),
        "{error}"
    );
}

#[test]
fn a_use_or_a_module_declaration_names_nothing_it_brings_in() {
    let source = "mod name_syntax;\nuse crate::name_syntax::{name_syntax, NameSyntax};\n\
                  mod inline { pub fn helper() { quote() } }\nfn a() { parse() }\n";
    let kept = kept(source, &[]).unwrap();
    for named in ["a", "parse", "helper", "quote", "inline"] {
        assert!(kept.identifiers.contains(named), "{named}");
    }
    for unnamed in ["name_syntax", "NameSyntax", "crate", "use"] {
        assert!(!kept.identifiers.contains(unnamed), "{unnamed}");
    }
    // The `use` still moves the key: another import of one name is another
    // function. A plain module declaration does not, since its file is keyed
    // as itself; one with an attribute does.
    assert!(
        kept.text.starts_with("use crate : : name_syntax : :"),
        "{}",
        kept.text
    );
    let attributed =
        kin_derivation_inputs::kept("#[path = \"other.rs\"]\nmod name_syntax;\n", &[]).unwrap();
    assert_eq!(
        attributed.text,
        "# [ path = \"other.rs\" ] mod name_syntax ; "
    );
}

#[test]
fn a_trait_implementation_is_keyed_with_its_trait_whatever_its_generics() {
    let source = "impl<'de> Deserialize<'de> for Delta { fn deserialize() {} }\n\
                  impl<T: Clone> kin::Hook<T> for Vec<T> { fn hook(&self) {} }\n";
    let found = paths(source);
    assert_eq!(found[0], "impl Deserialize<'de> for Delta");
    assert_eq!(found[2], "impl kin::Hook<T> for Vec<T>");
    assert_eq!(
        kin_derivation_inputs::implemented_trait(&found[0]),
        Some("Deserialize")
    );
    assert_eq!(
        kin_derivation_inputs::implemented_trait(&found[2]),
        Some("Hook")
    );
}

#[test]
fn a_trait_implemented_for_a_reference_or_tuple_is_still_a_trait_implementation() {
    let by_reference = paths("impl<G: Store> ChangeStore for &G { fn history(&self) {} }\n");
    assert_eq!(
        kin_derivation_inputs::implemented_trait(&by_reference[0]),
        Some("ChangeStore"),
        "{by_reference:?}"
    );
    let tuple = paths("impl Pair for (A, B) { fn first(&self) {} }\n");
    assert_eq!(
        kin_derivation_inputs::implemented_trait(&tuple[0]),
        Some("Pair"),
        "{tuple:?}"
    );
    assert_eq!(
        kin_derivation_inputs::implemented_trait("impl Formatter"),
        None
    );
    assert_eq!(
        kin_derivation_inputs::implemented_trait("impl Platform<T>"),
        None
    );
}
