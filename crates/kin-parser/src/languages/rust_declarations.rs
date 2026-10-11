// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The Rust declaration census.
//!
//! Counted, at any depth: functions, and methods in impl and trait bodies
//! whether required or provided; structs, unions, enums and each enum
//! variant; traits; type aliases and associated types; consts and statics,
//! associated ones too; named struct and union fields; `macro_rules!`
//! definitions; declarations in `extern` blocks; and modules declared inside a
//! function body.
//!
//! A macro's token trees are not parsed as items, but the grammar still shows
//! the keyword and the literal name of every item a macro spells:
//! `fn helper`, `struct Fixed`, `static CACHE` in a `macro_rules!` body, a
//! `thread_local!` or a `lazy_static!` block. Each is counted, owned by the
//! macro, since extraction admits none of them and a name search has to be
//! able to say so.
//!
//! Not counted: `use` and `extern crate` bindings, which the import evidence
//! covers; `let` bindings, which are locals; a module declared at module scope,
//! which extraction always admits as a module entity; and a name a macro
//! metavariable spells (`fn $name`), which is not known until expansion.

use tree_sitter::{Node, Tree};

use crate::adapter::site_from_node;
use crate::declaration_census::{DeclarationKind, DeclarationScope, SeenDeclaration};
use crate::extract::RelationSite;

use super::rust_nested::{item_name, text};

#[derive(Clone)]
struct Place {
    scope: DeclarationScope,
    in_callable: bool,
    owner: Option<String>,
}

pub(crate) fn rust_declaration_census(tree: &Tree, source: &[u8]) -> Vec<SeenDeclaration> {
    let mut out = Vec::new();
    walk(
        tree.root_node(),
        source,
        &Place {
            scope: DeclarationScope::Module,
            in_callable: false,
            owner: None,
        },
        &mut out,
    );
    out
}

fn push(
    out: &mut Vec<SeenDeclaration>,
    place: &Place,
    name: &str,
    kind: DeclarationKind,
    site: RelationSite,
) {
    out.push(SeenDeclaration {
        name: name.to_string(),
        kind,
        scope: place.scope,
        in_callable: place.in_callable,
        owner: place.owner.clone(),
        site,
    });
}

/// The place inside a callable body or an initializer: whatever it declares
/// is local to it.
fn inside_callable(place: &Place, owner: Option<&str>) -> Place {
    Place {
        scope: DeclarationScope::Module,
        in_callable: true,
        owner: owner.map(str::to_string).or_else(|| place.owner.clone()),
    }
}

fn walk(node: Node<'_>, source: &[u8], place: &Place, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    for child in children {
        let name = item_name(child, source);
        match child.kind() {
            "function_item" | "function_signature_item" => {
                let kind = if place.scope == DeclarationScope::Class && !place.in_callable {
                    DeclarationKind::Method
                } else {
                    DeclarationKind::Function
                };
                if let Some(name) = name {
                    push(out, place, name, kind, site_from_node(&child));
                }
                walk(child, source, &inside_callable(place, name), out);
            }
            "struct_item" | "union_item" | "enum_item" => {
                let kind = if child.kind() == "enum_item" {
                    DeclarationKind::Enum
                } else {
                    DeclarationKind::Class
                };
                if let Some(name) = name {
                    push(out, place, name, kind, site_from_node(&child));
                }
                if let Some(body) = child.child_by_field_name("body") {
                    let members = Place {
                        scope: DeclarationScope::Class,
                        in_callable: place.in_callable,
                        owner: name.map(str::to_string).or_else(|| place.owner.clone()),
                    };
                    members_of(body, source, &members, out);
                }
            }
            "trait_item" | "impl_item" => {
                if child.kind() == "trait_item" {
                    if let Some(name) = name {
                        push(
                            out,
                            place,
                            name,
                            DeclarationKind::Interface,
                            site_from_node(&child),
                        );
                    }
                }
                let owner = match child.kind() {
                    "trait_item" => name.map(str::to_string),
                    _ => child
                        .child_by_field_name("type")
                        .map(|ty| text(ty, source).to_string()),
                };
                if let Some(body) = child.child_by_field_name("body") {
                    let members = Place {
                        scope: DeclarationScope::Class,
                        in_callable: place.in_callable,
                        owner: owner.or_else(|| place.owner.clone()),
                    };
                    walk(body, source, &members, out);
                }
            }
            "type_item" | "associated_type" => {
                if let Some(name) = name {
                    push(
                        out,
                        place,
                        name,
                        DeclarationKind::TypeAlias,
                        site_from_node(&child),
                    );
                }
            }
            "const_item" | "static_item" => {
                let kind = if place.scope == DeclarationScope::Class {
                    DeclarationKind::Attribute
                } else {
                    DeclarationKind::Variable
                };
                if let Some(name) = name {
                    push(out, place, name, kind, site_from_node(&child));
                }
                walk(child, source, &inside_callable(place, name), out);
            }
            "macro_definition" => {
                if let Some(name) = name {
                    push(
                        out,
                        place,
                        name,
                        DeclarationKind::Function,
                        site_from_node(&child),
                    );
                }
                let owner = name.map(str::to_string).or_else(|| place.owner.clone());
                let mut rules = child.walk();
                let rules: Vec<Node<'_>> = child.named_children(&mut rules).collect();
                for rule in rules {
                    if let Some(right) = rule.child_by_field_name("right") {
                        token_tree_declarations(right, source, place, owner.as_deref(), out);
                    }
                }
            }
            "macro_invocation" => {
                let owner = child
                    .child_by_field_name("macro")
                    .map(|path| text(path, source).to_string());
                let mut trees = child.walk();
                let trees: Vec<Node<'_>> = child
                    .children(&mut trees)
                    .filter(|tree| tree.kind() == "token_tree")
                    .collect();
                for tree in trees {
                    token_tree_declarations(tree, source, place, owner.as_deref(), out);
                }
            }
            "mod_item" => {
                if place.in_callable {
                    if let Some(name) = name {
                        push(
                            out,
                            place,
                            name,
                            DeclarationKind::Namespace,
                            site_from_node(&child),
                        );
                    }
                }
                if let Some(body) = child.child_by_field_name("body") {
                    let inner = Place {
                        scope: DeclarationScope::Module,
                        in_callable: place.in_callable,
                        owner: name.map(str::to_string).or_else(|| place.owner.clone()),
                    };
                    walk(body, source, &inner, out);
                }
            }
            "closure_expression" => walk(child, source, &inside_callable(place, None), out),
            "use_declaration"
            | "extern_crate_declaration"
            | "attribute_item"
            | "inner_attribute_item"
            | "line_comment"
            | "block_comment" => {}
            _ => walk(child, source, place, out),
        }
    }
}

/// The fields of a struct or union body and the variants of an enum body.
fn members_of(body: Node<'_>, source: &[u8], place: &Place, out: &mut Vec<SeenDeclaration>) {
    let mut cursor = body.walk();
    let members: Vec<Node<'_>> = body.named_children(&mut cursor).collect();
    for member in members {
        match member.kind() {
            "field_declaration" | "enum_variant" => {
                if let Some(name) = member
                    .child_by_field_name("name")
                    .filter(|name| matches!(name.kind(), "field_identifier" | "identifier"))
                {
                    push(
                        out,
                        place,
                        text(name, source),
                        DeclarationKind::Attribute,
                        site_from_node(&member),
                    );
                }
            }
            _ => {}
        }
    }
}

/// Items a macro token tree spells with a literal name.
fn token_tree_declarations(
    tree: Node<'_>,
    source: &[u8],
    place: &Place,
    owner: Option<&str>,
    out: &mut Vec<SeenDeclaration>,
) {
    let mut cursor = tree.walk();
    let tokens: Vec<Node<'_>> = tree
        .children(&mut cursor)
        .filter(|token| !matches!(token.kind(), "line_comment" | "block_comment"))
        .collect();
    let spelled = Place {
        scope: DeclarationScope::Module,
        in_callable: place.in_callable,
        owner: owner.map(str::to_string).or_else(|| place.owner.clone()),
    };
    for (index, token) in tokens.iter().enumerate() {
        if token.kind() == "token_tree" {
            token_tree_declarations(*token, source, place, owner, out);
            continue;
        }
        let kind = match token.kind() {
            "fn" => DeclarationKind::Function,
            "struct" => DeclarationKind::Class,
            "enum" => DeclarationKind::Enum,
            "trait" => DeclarationKind::Interface,
            "type" => DeclarationKind::TypeAlias,
            "mod" => DeclarationKind::Namespace,
            "static" | "const" => DeclarationKind::Variable,
            _ => continue,
        };
        // `*const T`, `<const N: usize>` and `, const M` are types and
        // generic parameters, never items.
        if token.kind() == "const"
            && index
                .checked_sub(1)
                .and_then(|at| tokens.get(at))
                .is_some_and(|previous| matches!(previous.kind(), "*" | "<" | ","))
        {
            continue;
        }
        let mut at = index + 1;
        while tokens
            .get(at)
            .is_some_and(|next| matches!(next.kind(), "ref" | "mut" | "mutable_specifier"))
        {
            at += 1;
        }
        let Some(name) = tokens.get(at).filter(|name| name.kind() == "identifier") else {
            continue;
        };
        let (start, end) = (token.start_position(), name.end_position());
        push(
            out,
            &spelled,
            text(*name, source),
            kind,
            RelationSite {
                start_byte: token.start_byte(),
                end_byte: name.end_byte(),
                start_line: start.row as u32,
                start_col: start.column as u32,
                end_line: end.row as u32,
                end_col: end.column as u32,
                syntactic_role: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::adapter::LanguageAdapter;
    use crate::declaration_census::{take_census, REASON_CLASS_MEMBER, REASON_NESTED_IN_CALLABLE};
    use crate::languages::RustAdapter;
    use kin_model::FilePathId;

    fn census(source: &str) -> crate::declaration_census::DeclarationCensus {
        let tree = RustAdapter.parse(source.as_bytes()).unwrap();
        let output = RustAdapter
            .extract(&tree, source.as_bytes(), &FilePathId::new("src/store.rs"))
            .unwrap();
        let seen = RustAdapter
            .declaration_census(&tree, source.as_bytes())
            .expect("the Rust adapter takes a census");
        take_census(seen, &output.entities)
    }

    fn unadmitted(census: &crate::declaration_census::DeclarationCensus) -> Vec<(&str, &str)> {
        census
            .unadmitted
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.reason.as_str()))
            .collect()
    }

    #[test]
    fn every_admitted_item_pairs_and_members_extraction_skips_are_listed() {
        let census = census(
            "\
/// A store.
#[derive(Debug)]
pub struct Store {
    path: String,
}

impl Store {
    pub const LIMIT: usize = 4;
    pub fn open(path: &str) -> Self { Store { path: path.into() } }
}

pub trait Sink {
    type Item;
    fn matched(&mut self) -> bool;
    fn finish(&mut self) {}
}

pub enum Shape { Circle, Square }
pub type Db = Store;
static CACHE: u8 = 0;
mod inner { pub fn helper() {} }
",
        );
        assert_eq!(
            unadmitted(&census),
            vec![
                ("LIMIT", REASON_CLASS_MEMBER),
                ("Item", REASON_CLASS_MEMBER),
            ]
        );
        assert_eq!(census.seen, census.admitted + 2);
    }

    #[test]
    fn nested_items_pair_with_their_local_scope_entities() {
        let census = census(
            "\
fn outer() {
    struct Fake;
    impl Sink for Fake {
        fn matched(&mut self) -> bool { helper() }
    }
    fn helper() -> bool { true }
    mod hidden { fn deep() {} }
}
",
        );
        assert_eq!(
            unadmitted(&census),
            vec![
                ("hidden", REASON_NESTED_IN_CALLABLE),
                ("deep", REASON_NESTED_IN_CALLABLE),
            ]
        );
    }

    #[test]
    fn items_a_macro_spells_with_literal_names_are_listed_under_the_macro() {
        let census = census(
            "\
macro_rules! make {
    ($name:ident) => {
        fn $name() {}
        struct Fixed;
        static COUNT: u8 = 0;
    };
}
thread_local! {
    static CURRENT: std::cell::Cell<*const u8> = std::cell::Cell::new(std::ptr::null());
}
",
        );
        let listed: Vec<(&str, Option<&str>)> = census
            .unadmitted
            .iter()
            .map(|declaration| (declaration.name.as_str(), declaration.owner.as_deref()))
            .collect();
        assert_eq!(
            listed,
            vec![
                ("Fixed", Some("make")),
                ("COUNT", Some("make")),
                ("CURRENT", Some("thread_local")),
            ]
        );
    }
}
