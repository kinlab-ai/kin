// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A unit editor for a language whose unit has no clause of its own: the
//! unit is its imports, then its declarations, and a declaration created in
//! it is written after the last one.
//!
//! What a declaration is, is read from the language's own extraction, so a
//! created declaration is named exactly as the graph will name it. What an
//! import statement looks like, and how far apart two top-level declarations
//! stand, are the language's, stated in an [`AppendSyntax`]. Nothing here
//! names a language.

use std::ops::Range;

use kin_model::{EntityKind, FilePathId, LanguageId};
use tree_sitter::Node;

use crate::languages::AdapterRegistry;
use crate::unit_editor::{
    UnitDeclaration, UnitDeclarationKind, UnitEdit, UnitEditOutcome, UnitEditor, UnitImport,
    UnitRole,
};

/// The facts an appending unit editor needs from its language.
pub struct AppendSyntax {
    pub language: LanguageId,
    /// Blank lines between two top-level declarations.
    pub separation: usize,
    /// The node kinds of a top-level import statement.
    pub import_kinds: &'static [&'static str],
    /// One import as the one statement the editor writes for it.
    pub render_import: fn(&UnitImport) -> String,
    /// The import path a top-level import statement imports, when it is a
    /// statement of the one form the editor writes.
    pub imported_path: fn(Node, &[u8]) -> Option<String>,
    /// Whether a top-level statement must stay ahead of imports: a module
    /// docstring, a directive prologue.
    pub leads_imports: fn(Node, &[u8]) -> bool,
}

fn display(language: LanguageId) -> &'static str {
    language.profile().display_name
}

fn parse(language: LanguageId, source: &[u8]) -> Result<tree_sitter::Tree, String> {
    AdapterRegistry::new()
        .get_by_language(language)
        .ok_or("no parser reads this language")?
        .parse(source)
        .map_err(|error| error.to_string())
}

/// The unit kind a graph kind is created as, when units create it.
fn unit_kind(kind: EntityKind) -> Option<UnitDeclarationKind> {
    Some(match kind {
        EntityKind::Function => UnitDeclarationKind::Function,
        EntityKind::Class => UnitDeclarationKind::Struct,
        EntityKind::Interface | EntityKind::TraitDef => UnitDeclarationKind::Interface,
        EntityKind::TypeAlias | EntityKind::EnumDef => UnitDeclarationKind::Type,
        EntityKind::Constant => UnitDeclarationKind::Const,
        EntityKind::StaticVar => UnitDeclarationKind::Var,
        _ => return None,
    })
}

/// Read one top-level declaration a caller sent, as the language's adapter
/// extracts it: its kind, the names of the entities it declares at its top,
/// and its statement's bytes. A doc comment may lead it; an import, a second
/// declaration or anything the adapter records no entity for is refused.
pub fn read_declaration(syntax: &AppendSyntax, body: &str) -> Result<UnitDeclaration, String> {
    let language = syntax.language;
    let body = body.trim();
    if body.is_empty() {
        return Err("a declaration body must be nonempty".into());
    }
    let source = format!("{body}\n");
    let tree = parse(language, source.as_bytes())?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(format!(
            "the body is not one complete, valid {} declaration; send exactly the declaration, \
             with no imports",
            display(language)
        ));
    }
    let mut cursor = root.walk();
    let statements: Vec<Node> = root
        .named_children(&mut cursor)
        .filter(|node| !node.kind().contains("comment"))
        .collect();
    let [statement] = statements.as_slice() else {
        return Err(
            "the body must be exactly one declaration: no imports, statements or other \
             declarations"
                .into(),
        );
    };
    if syntax.import_kinds.contains(&statement.kind()) {
        return Err(
            "the body is an import; send imports in the operation's imports, and the \
             declaration alone as its body"
                .into(),
        );
    }
    let adapter_registry = AdapterRegistry::new();
    let adapter = adapter_registry
        .get_by_language(language)
        .ok_or("no parser reads this language")?;
    let parsed = adapter
        .extract(&tree, source.as_bytes(), &FilePathId::new("kin-unit"))
        .map_err(|error| error.to_string())?;
    // Every entity the body declares is the statement's: it is the one
    // statement there. An adapter may widen an entity's span over a doc
    // comment or an attribute ahead of the statement.
    let held: Vec<&crate::ExtractedEntity> = parsed
        .entities
        .iter()
        .filter(|entity| {
            !matches!(
                entity.kind,
                EntityKind::Module | EntityKind::Package | EntityKind::File
            )
        })
        .collect();
    // The entities declared at the statement's top: inside no other one.
    let top: Vec<&crate::ExtractedEntity> = held
        .iter()
        .copied()
        .filter(|entity| {
            !held.iter().any(|other| {
                !std::ptr::eq(*other, *entity)
                    && other.span.start_byte <= entity.span.start_byte
                    && entity.span.end_byte <= other.span.end_byte
                    && (other.span.start_byte, other.span.end_byte)
                        != (entity.span.start_byte, entity.span.end_byte)
            })
        })
        .collect();
    let range = top.iter().fold(statement.byte_range(), |range, entity| {
        range.start.min(entity.span.start_byte)..range.end.max(entity.span.end_byte)
    });
    let Some(first) = top.first() else {
        return Err(format!(
            "the body declares nothing Kin records as a {} entity",
            display(language)
        ));
    };
    let kind = unit_kind(first.kind).ok_or_else(|| {
        format!(
            "the body declares a {:?}, which a unit does not create; create it beside a current \
             declaration",
            first.kind
        )
    })?;
    Ok(UnitDeclaration {
        kind,
        names: top.iter().map(|entity| entity.name.clone()).collect(),
        receiver: None,
        range,
    })
}

fn line_end(source: &[u8], at: usize) -> usize {
    source[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(source.len(), |offset| at + offset + 1)
}

fn line_start(source: &[u8], at: usize) -> usize {
    source[..at]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |newline| newline + 1)
}

/// `existing` (or a new unit) with `edit` applied: imports removed and added
/// in the unit's import block, and each declaration written after the unit's
/// last statement, as far apart as the language sets top-level declarations.
pub fn append_edit(
    syntax: &AppendSyntax,
    existing: Option<&[u8]>,
    edit: &UnitEdit,
) -> Result<UnitEditOutcome, String> {
    let language = syntax.language;
    let mut source = existing.unwrap_or_default().to_vec();
    let mut imports_changed = false;
    // The import statements the unit holds, by where they stand.
    let held_imports =
        |source: &[u8]| -> Result<(Vec<(Range<usize>, Option<String>)>, usize), String> {
            let tree = parse(language, source)?;
            let root = tree.root_node();
            if root.has_error() {
                return Err(format!(
                    "the unit is not complete valid {}; fix that first with a patch to the \
                 declaration that holds the problem",
                    display(language)
                ));
            }
            let mut cursor = root.walk();
            let mut imports = Vec::new();
            // Where a first import goes: ahead of the first statement that need
            // not lead them.
            let mut first_code = None;
            for node in root.named_children(&mut cursor) {
                if node.kind().contains("comment") {
                    continue;
                }
                if syntax.import_kinds.contains(&node.kind()) {
                    imports.push((node.byte_range(), (syntax.imported_path)(node, source)));
                } else if first_code.is_none() && !(syntax.leads_imports)(node, source) {
                    first_code = Some(line_start(source, node.start_byte()));
                }
            }
            let after_leading = first_code.unwrap_or(source.len());
            Ok((imports, after_leading))
        };
    // Added first, so an import swapped for another keeps its block.
    if !edit.add_imports.is_empty() {
        let (imports, after_leading) = held_imports(&source)?;
        let lines: Vec<String> = edit
            .add_imports
            .iter()
            .filter(|import| {
                !imports
                    .iter()
                    .any(|(_, path)| path.as_deref() == Some(import.path.as_str()))
            })
            .map(|import| (syntax.render_import)(import))
            .collect();
        if !lines.is_empty() {
            let block = format!("{}\n", lines.join("\n"));
            let (at, text) = match imports.last() {
                Some((range, _)) => (line_end(&source, range.end), block),
                None if source[..after_leading].iter().all(u8::is_ascii_whitespace)
                    && after_leading == source.len() =>
                {
                    (0, block)
                }
                None => (after_leading, format!("{block}\n")),
            };
            let at = at.min(source.len());
            let text = if at > 0 && source[at - 1] != b'\n' {
                format!("\n{text}")
            } else {
                text
            };
            source.splice(at..at, text.into_bytes());
            imports_changed = true;
        }
    }
    if !edit.remove_imports.is_empty() {
        let (imports, _) = held_imports(&source)?;
        let mut cuts: Vec<Range<usize>> = imports
            .iter()
            .filter(|(_, path)| {
                path.as_ref()
                    .is_some_and(|path| edit.remove_imports.contains(path))
            })
            .map(|(range, _)| line_start(&source, range.start)..line_end(&source, range.end))
            .collect();
        cuts.sort_by_key(|range| std::cmp::Reverse(range.start));
        for cut in cuts {
            source.drain(cut);
            imports_changed = true;
        }
    }
    // A unit never opens with blank lines a removed import left.
    let lead = source.iter().take_while(|byte| **byte == b'\n').count();
    source.drain(..lead);
    let mut created = Vec::new();
    for body in &edit.declarations {
        let declaration = read_declaration(syntax, body)?;
        let body = body.trim();
        while source.last().is_some_and(u8::is_ascii_whitespace) {
            source.pop();
        }
        if !source.is_empty() {
            source.extend(std::iter::repeat_n(b'\n', syntax.separation + 1));
        }
        let at = source.len();
        source.extend_from_slice(body.as_bytes());
        created.push(UnitDeclaration {
            range: at + declaration.range.start..at + declaration.range.end,
            ..declaration
        });
    }
    if !source.is_empty() && source.last() != Some(&b'\n') {
        source.push(b'\n');
    }
    // What is written must read as the language.
    let tree = parse(language, &source)?;
    if tree.root_node().has_error() {
        return Err(format!(
            "the unit this writes is not complete valid {}; nothing was written",
            display(language)
        ));
    }
    Ok(UnitEditOutcome {
        source,
        created,
        imports_changed,
    })
}

/// How a language names its units, for an [`AppendingEditor`].
pub struct UnitIdentity {
    pub validate: fn(&str, &str, UnitRole) -> Result<(), String>,
    pub path: fn(&str, &str, UnitRole) -> String,
    pub describe: fn(&str, &str, UnitRole) -> String,
    pub validate_import: fn(&UnitImport) -> Result<(), String>,
    pub is_identifier: fn(&str) -> bool,
    pub is_test_unit: fn(&str) -> bool,
}

/// A unit editor for a language whose units are imports then declarations.
pub struct AppendingEditor {
    pub syntax: AppendSyntax,
    pub identity: UnitIdentity,
}

impl UnitEditor for AppendingEditor {
    fn validate_unit(&self, package: &str, name: &str, role: UnitRole) -> Result<(), String> {
        (self.identity.validate)(package, name, role)
    }

    fn unit_path(&self, package: &str, name: &str, role: UnitRole) -> Result<String, String> {
        self.validate_unit(package, name, role)?;
        Ok((self.identity.path)(package, name, role))
    }

    fn describe_unit(&self, package: &str, name: &str, role: UnitRole) -> String {
        (self.identity.describe)(package, name, role)
    }

    fn parse_declaration(&self, body: &str) -> Result<UnitDeclaration, String> {
        read_declaration(&self.syntax, body)
    }

    fn edit(
        &self,
        existing: Option<&[u8]>,
        _unit_name: &str,
        edit: &UnitEdit,
    ) -> Result<UnitEditOutcome, String> {
        append_edit(&self.syntax, existing, edit)
    }

    fn validate_import(&self, import: &UnitImport) -> Result<(), String> {
        (self.identity.validate_import)(import)
    }

    fn is_identifier(&self, name: &str) -> bool {
        (self.identity.is_identifier)(name)
    }

    fn is_test_unit(&self, path: &str) -> bool {
        (self.identity.is_test_unit)(path)
    }

    /// The unit's name is its own; no entity declares another.
    fn declared_unit<'e>(&self, _entity: &'e kin_model::Entity) -> Option<&'e str> {
        None
    }

    fn check_directory(
        &self,
        _sources: &std::collections::BTreeSet<String>,
        _tests: &std::collections::BTreeSet<String>,
    ) -> Result<(), String> {
        Ok(())
    }
}

/// Whether `name` is one ASCII identifier: a letter or `_`, then letters,
/// digits and `_`, and none of `reserved`.
pub fn is_ascii_identifier(name: &str, reserved: &[&str]) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && !reserved.contains(&name)
}

/// Refuse an import path a string literal cannot hold as written.
pub fn quoted_import_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 512 {
        return Err("an import path must be 1 to 512 bytes".into());
    }
    if path.chars().any(|character| {
        character.is_control() || character.is_whitespace() || "\"'`\\".contains(character)
    }) {
        return Err(format!(
            "import path {path:?} may not hold quotes, backslashes, spaces or control characters"
        ));
    }
    Ok(())
}
