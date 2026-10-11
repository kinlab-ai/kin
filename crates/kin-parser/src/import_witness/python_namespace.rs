// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source-local binding authority for explicit Python namespace imports.
//! Resolution and admitted-source closure remain the linker's exact-import path.

use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Tree};

use super::{identifier, module_name, node_name, text, NamedImport, MAX_BINDINGS};
use crate::extract::FileImport;

pub(super) struct Bindings {
    pub namespaces: BTreeMap<String, String>,
    pub reexports: BTreeMap<String, NamedImport>,
    pub declarations: BTreeSet<String>,
    pub module_bindings: BTreeSet<String>,
    pub namespace_hazards: bool,
    pub mutated_paths: BTreeSet<String>,
}

/// The namespaces a plain, unconditional import statement actually binds.
/// Keep even a rebound root here: it still claims an import, but requires a
/// source witness rather than the legacy name-only module receiver tier.
pub(super) fn namespaces(tree: &Tree, imports: &[FileImport]) -> BTreeMap<String, String> {
    let root = tree.root_node();
    let mut cursor = root.walk();
    let sites: BTreeSet<_> = root
        .named_children(&mut cursor)
        .filter(|node| node.kind() == "import_statement")
        .map(|node| (node.start_byte(), node.end_byte()))
        .collect();
    let mut result = BTreeMap::new();
    for import in imports
        .iter()
        .filter(|import| sites.contains(&(import.site.start_byte, import.site.end_byte)))
    {
        for specifier in &import.specifiers {
            if !module_name(&import.module_path) || specifier.is_default {
                continue;
            }
            if specifier.original_name.is_some() {
                if identifier(&specifier.local_name) {
                    result.insert(specifier.local_name.clone(), import.module_path.clone());
                }
            } else {
                result.insert(specifier.local_name.clone(), import.module_path.clone());
                if let Some(first) = specifier
                    .local_name
                    .split('.')
                    .next()
                    .filter(|name| identifier(name))
                {
                    result.insert(first.to_string(), first.to_string());
                }
            }
        }
    }
    result
}

/// Count writes in module scope separately from locals. An unrelated literal,
/// conditional or ordinary class does not erase a different binding's evidence.
/// Unknown namespace mutation and affected outward writes still refuse proof.
pub(super) fn bindings(
    tree: &Tree,
    nodes: &[Node<'_>],
    source: &[u8],
    imports: &[FileImport],
) -> Option<Bindings> {
    if imports.iter().any(|import| {
        matches!(import.module_path.as_str(), "sys" | "importlib")
            && import.specifiers.iter().any(|specifier| {
                matches!(
                    specifier
                        .original_name
                        .as_deref()
                        .unwrap_or(&specifier.local_name),
                    "modules" | "import_module"
                )
            })
    }) {
        return None;
    }
    let mut writes = BTreeMap::<String, usize>::new();
    count_module_writes(tree.root_node(), source, &mut writes)?;
    let imported_namespaces = namespaces(tree, imports);
    let mut blocked = BTreeSet::new();
    let mut mutated_paths = BTreeSet::new();
    for node in nodes {
        if matches!(
            node.kind(),
            "named_expression" | "wildcard_import" | "match_statement" | "type_alias_statement"
        ) {
            return None;
        }
        if node.kind() == "identifier"
            && text(*node, source).is_some_and(|name| {
                matches!(
                    name,
                    "globals"
                        | "locals"
                        | "vars"
                        | "eval"
                        | "exec"
                        | "__import__"
                        | "__builtins__"
                        | "builtins"
                        | "__dict__"
                        | "__globals__"
                        | "import_module"
                )
            })
        {
            return None;
        }
        if node.kind() == "identifier" {
            let name = text(*node, source)?;
            if matches!(name, "setattr" | "delattr")
                && !node.parent().is_some_and(|parent| {
                    let callee = if parent.kind() == "attribute" {
                        parent
                    } else {
                        *node
                    };
                    callee.parent().is_some_and(|call| {
                        call.kind() == "call"
                            && call
                                .child_by_field_name("function")
                                .map(|function| function.id())
                                == Some(callee.id())
                    })
                })
            {
                return None;
            }
            // A module value passed, returned or aliased can be mutated under
            // another name. Only its attribute reads retain bounded authority.
            if imported_namespaces.contains_key(name)
                && !node.parent().is_some_and(|parent| {
                    (parent.kind() == "attribute"
                        && parent
                            .child_by_field_name("object")
                            .map(|object| object.id())
                            == Some(node.id()))
                        || matches!(parent.kind(), "dotted_name" | "aliased_import")
                })
            {
                if imported_namespaces
                    .get(name)
                    .is_some_and(|module| module == "sys")
                {
                    return None;
                }
                blocked.insert(name.to_string());
            }
        }
        if matches!(node.kind(), "global_statement" | "nonlocal_statement") {
            let mut cursor = node.walk();
            for child in node
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
            {
                blocked.insert(text(child, source)?.to_string());
            }
        }
        let target = match node.kind() {
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                node.child_by_field_name("left")
            }
            "as_pattern" => node.child_by_field_name("alias"),
            "delete_statement" => node.named_child(0),
            _ => None,
        };
        if let Some(target) = target {
            block_mutated_roots(target, source, &mut blocked, &mut mutated_paths)?;
        }
        if node.kind() == "call"
            && node
                .child_by_field_name("function")
                .and_then(|callee| text(callee, source))
                .is_some_and(|name| matches!(name.rsplit('.').next(), Some("setattr" | "delattr")))
        {
            let arguments = node.child_by_field_name("arguments")?;
            blocked.insert(root_name(arguments.named_child(0)?, source)?.to_string());
        }
        if node.kind() == "attribute" {
            let object = node.child_by_field_name("object")?;
            let member = text(node.child_by_field_name("attribute")?, source)?;
            if let Some(path) = attribute_path(*node, source) {
                if imported_namespaces.contains_key(&path)
                    && !node.parent().is_some_and(|parent| {
                        parent.kind() == "attribute"
                            && parent
                                .child_by_field_name("object")
                                .map(|object| object.id())
                                == Some(node.id())
                    })
                {
                    mutated_paths.insert(path);
                }
            }
            if member == "modules"
                && text(object, source)
                    .and_then(|name| imported_namespaces.get(name))
                    .is_some_and(|module| module == "sys")
            {
                return None;
            }
            if matches!(member, "__setattr__" | "__delattr__") {
                blocked.insert(root_name(object, source)?.to_string());
            }
        }
    }
    for path in &mutated_paths {
        let root = path.split('.').next()?;
        if !imported_namespaces.contains_key(root) {
            blocked.insert(root.to_string());
        }
    }
    let safe = |name: &str| writes.get(name) == Some(&1) && !blocked.contains(name);
    let namespaces = imported_namespaces
        .into_iter()
        .filter(|(name, _)| name.split('.').next().is_some_and(|root| safe(root)))
        .collect();
    let mut reexports = BTreeMap::new();
    let mut declarations = BTreeSet::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    for node in root.named_children(&mut cursor) {
        match node.kind() {
            "import_from_statement" => {
                for import in imports.iter().filter(|import| {
                    import.site.start_byte == node.start_byte()
                        && import.site.end_byte == node.end_byte()
                }) {
                    for specifier in &import.specifiers {
                        let local = &specifier.local_name;
                        let original = specifier.original_name.as_deref().unwrap_or(local);
                        if safe(local)
                            && identifier(local)
                            && identifier(original)
                            && module_name(&import.module_path)
                            && !specifier.is_default
                        {
                            reexports.insert(
                                local.clone(),
                                NamedImport {
                                    module: import.module_path.clone(),
                                    original: original.to_string(),
                                },
                            );
                        }
                    }
                }
            }
            "function_definition" | "class_definition" => {
                let name = node_name(node, source)?;
                // A decorator or custom metaclass can replace the declaration.
                let custom_metaclass =
                    node.child_by_field_name("superclasses")
                        .is_some_and(|bases| {
                            let mut cursor = bases.walk();
                            let keyword = bases
                                .named_children(&mut cursor)
                                .any(|base| base.kind() == "keyword_argument");
                            keyword
                        });
                if safe(name) && !custom_metaclass {
                    declarations.insert(name.to_string());
                }
            }
            _ => {}
        }
    }
    Some(Bindings {
        namespaces,
        reexports,
        declarations,
        module_bindings: writes.into_keys().collect(),
        namespace_hazards: !blocked.is_empty() || !mutated_paths.is_empty(),
        mutated_paths,
    })
}

fn root_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.kind() {
        "identifier" => text(node, source).filter(|name| identifier(name)),
        "attribute" => root_name(node.child_by_field_name("object")?, source),
        "subscript" => root_name(node.child_by_field_name("value")?, source),
        _ => None,
    }
}

fn block_mutated_roots(
    node: Node<'_>,
    source: &[u8],
    blocked: &mut BTreeSet<String>,
    paths: &mut BTreeSet<String>,
) -> Option<()> {
    match node.kind() {
        "attribute" => {
            paths.insert(attribute_path(node, source)?);
        }
        "subscript" => {
            blocked.insert(root_name(node, source)?.to_string());
        }
        "identifier" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                block_mutated_roots(child, source, blocked, paths)?;
            }
        }
    }
    Some(())
}

fn attribute_path(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(
            text(node, source)
                .filter(|name| identifier(name))?
                .to_string(),
        ),
        "attribute" => Some(format!(
            "{}.{}",
            attribute_path(node.child_by_field_name("object")?, source)?,
            text(node.child_by_field_name("attribute")?, source).filter(|name| identifier(name))?
        )),
        _ => None,
    }
}

fn count_target(node: Node<'_>, source: &[u8], writes: &mut BTreeMap<String, usize>) -> Option<()> {
    match node.kind() {
        "identifier" => {
            let name = text(node, source)?;
            if !identifier(name) || writes.len() >= MAX_BINDINGS {
                return None;
            }
            *writes.entry(name.to_string()).or_default() += 1;
        }
        "attribute" | "subscript" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                count_target(child, source, writes)?;
            }
        }
    }
    Some(())
}

fn count_module_writes(
    node: Node<'_>,
    source: &[u8],
    writes: &mut BTreeMap<String, usize>,
) -> Option<()> {
    match node.kind() {
        "wildcard_import" | "match_statement" => return None,
        "function_definition" | "class_definition" => {
            count_target(node.child_by_field_name("name")?, source, writes)?;
            return Some(());
        }
        "lambda"
        | "list_comprehension"
        | "set_comprehension"
        | "dictionary_comprehension"
        | "generator_expression" => return Some(()),
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            count_target(node.child_by_field_name("left")?, source, writes)?;
        }
        "named_expression" => return None,
        "delete_statement" => count_target(node, source, writes)?,
        "as_pattern" | "except_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                count_target(alias, source, writes)?;
            }
        }
        "import_statement" | "import_from_statement" => {
            let module = node.child_by_field_name("module_name");
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if Some(child.id()) == module.map(|module| module.id()) {
                    continue;
                }
                if child.kind() == "wildcard_import" {
                    return None;
                }
                let local = match child.kind() {
                    "aliased_import" => text(child.child_by_field_name("alias")?, source)?,
                    "dotted_name" => text(child, source)?.split('.').next()?,
                    _ => continue,
                };
                if !identifier(local) || writes.len() >= MAX_BINDINGS {
                    return None;
                }
                *writes.entry(local.to_string()).or_default() += 1;
            }
            return Some(());
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        count_module_writes(child, source, writes)?;
    }
    Some(())
}
