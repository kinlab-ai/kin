// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Record, at parse time, how a file uses names as values: the
//! [`kin_model::FileEscapeRecord`] a focal's escape census reads instead of
//! the file's bytes, and the [`kin_model::ProtocolMember`] marks on members
//! the language invokes through syntax.
//!
//! The language facts live in each adapter's collector
//! (`languages/python_escape.rs`, `languages/javascript_escape.rs`,
//! `languages/cpp_escape.rs` for C and C++, `languages/swift_escape.rs`,
//! `languages/php_escape.rs`); this module only dispatches to them and places
//! the result on the file's entities. A language whose adapter records nothing
//! returns `None`, and the census reads its files as unrecorded. Go, Java,
//! Kotlin, C#, Ruby and Rust are the adapters to fill next.

use kin_model::{
    Entity, EntityKind, FileEscapeRecord, FilePathId, LanguageId, ProtocolMember,
    NON_ENUMERABLE_MEMBER_KEY, PROTOCOL_MEMBER_KEY, VALUE_ESCAPE_RECORD_KEY,
};
use tree_sitter::{Node, Tree};

use crate::extract::ExtractedEntity;

/// A member declaration the language invokes through syntax, found by its
/// name token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolStamp {
    /// The member's name as declared.
    pub name: String,
    /// The first byte of the name token.
    pub at: usize,
    pub member: ProtocolMember,
}

/// A member declaration no own-property enumeration yields, found by its
/// name token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberStamp {
    /// The member's name as declared.
    pub name: String,
    /// The first byte of the name token.
    pub at: usize,
}

/// Everything an adapter's collector found in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEscapeCensus {
    pub record: FileEscapeRecord,
    pub protocol_members: Vec<ProtocolStamp>,
    /// Members the language never yields to an enumeration of an object's
    /// own enumerable properties (see [`kin_model::NON_ENUMERABLE_MEMBER_KEY`]).
    pub non_enumerable: Vec<MemberStamp>,
}

/// The calls of a file whose callee is a bare name a function or a block
/// around the call binds, each by the byte range of its callee token, in
/// source order. Empty for a language whose adapter does not read them.
///
/// It is read from the tree, never kept in the file's record: a reader that
/// settles call sites holds the text and takes it again. `tree` is the tree
/// the adapter parsed the file into.
///
/// A call-site ledger records what this says, and nothing a store holds
/// changes when the reading does. So this file and each adapter source it
/// reads are part of the enrichment producer identity (the language-server
/// crate's `build/producer_identity.rs`): add a language's source there when
/// it is added here.
pub fn local_callees(tree: &Tree, source: &[u8], language: LanguageId) -> Vec<(usize, usize)> {
    match language {
        LanguageId::JavaScript | LanguageId::TypeScript => {
            crate::languages::javascript_escape::local_callees(tree, source)
        }
        _ => Vec::new(),
    }
}

/// The file's census, taken by its language's adapter, or `None` when that
/// adapter records none. `tree` is the tree the adapter parsed the file
/// into, which for a Flow file is a TypeScript-family tree.
pub fn census_file(tree: &Tree, source: &[u8], language: LanguageId) -> Option<FileEscapeCensus> {
    match language {
        LanguageId::Python => Some(crate::languages::python_escape::census(tree, source)),
        LanguageId::JavaScript | LanguageId::TypeScript => {
            Some(crate::languages::javascript_escape::census(tree, source))
        }
        LanguageId::Go => Some(crate::languages::go_escape::census(tree, source)),
        LanguageId::Java => Some(crate::languages::java_escape::census(tree, source)),
        LanguageId::Kotlin => Some(crate::languages::kotlin_escape::census(tree, source)),
        LanguageId::Rust => Some(crate::languages::rust_escape::census(tree, source)),
        LanguageId::Swift => Some(crate::languages::swift_escape::census(tree, source)),
        LanguageId::Php => Some(crate::languages::php_escape::census(tree, source)),
        LanguageId::C | LanguageId::Cpp => Some(crate::languages::cpp_escape::census(
            tree,
            source,
            language == LanguageId::Cpp,
        )),
        _ => None,
    }
}

/// The module surface an adapter that minted none for a file would mint, so a
/// file whose record holds something always has an entity to carry it. Only
/// the languages whose adapters mint a surface conditionally need one.
pub fn module_surface(
    language: LanguageId,
    file_id: &FilePathId,
    tree: &Tree,
    source: &[u8],
) -> Option<ExtractedEntity> {
    let suffixes = match language {
        LanguageId::JavaScript => crate::scope::JS_SUFFIXES,
        LanguageId::TypeScript => crate::languages::TS_SUFFIXES,
        LanguageId::C | LanguageId::Cpp | LanguageId::Swift | LanguageId::Php => {
            return file_surface(language, file_id, tree, source)
        }
        _ => return None,
    };
    let (name, is_package) = crate::languages::js_module_identity(&file_id.0, suffixes);
    if name.is_empty() {
        return None;
    }
    let root = tree.root_node();
    Some(ExtractedEntity {
        kind: EntityKind::Module,
        name,
        signature: if is_package {
            format!("package {}", file_id.0)
        } else {
            format!("module {}", file_id.0)
        },
        visibility: kin_model::Visibility::Public,
        doc_summary: None,
        fingerprint: crate::adapter::compute_fingerprint(&root, source),
        span: crate::adapter::span_from_node(&root, file_id),
        declaration_line: None,
    })
}

/// The shared file module surface ([`crate::adapter::file_module_surface_entity`])
/// for a language whose adapter mints it only for a file that declares or
/// imports something, or never: a file holding nothing but a table of
/// function pointers or top-level statements still records its uses.
fn file_surface(
    language: LanguageId,
    file_id: &FilePathId,
    tree: &Tree,
    source: &[u8],
) -> Option<ExtractedEntity> {
    let root = tree.root_node();
    let (declared, keyword) = match language {
        LanguageId::Php => (
            crate::adapter::declared_package_coordinate(&root, source),
            "namespace",
        ),
        _ => (None, "module"),
    };
    let name = crate::adapter::file_module_surface_name(declared.as_deref(), file_id)?;
    Some(crate::adapter::file_module_surface_entity(
        name,
        format!("{keyword} {}", file_id.0),
        &root,
        source,
        file_id,
    ))
}

/// The innermost method or function named `name` whose span holds the byte
/// `at`: the member a stamp found by its name token.
fn member_at<'e>(entities: &'e mut [Entity], name: &str, at: usize) -> Option<&'e mut Entity> {
    entities
        .iter_mut()
        .filter(|entity| {
            matches!(entity.kind, EntityKind::Method | EntityKind::Function)
                && entity.name.rsplit(['.', ':']).next() == Some(name)
                && entity
                    .span
                    .as_ref()
                    .is_some_and(|span| span.start_byte <= at && at < span.end_byte)
        })
        .min_by_key(|entity| {
            entity
                .span
                .as_ref()
                .map_or(usize::MAX, |span| span.end_byte - span.start_byte)
        })
}

/// Place a file's census on its entities: the record on one carrier (the
/// file's module surface when it has one, else its first entity), and each
/// protocol or non-enumerable mark on the innermost member declared at its
/// name token. Any record or mark an earlier parse left is removed first, so
/// a language whose adapter records nothing carries none.
pub fn attach(census: Option<&FileEscapeCensus>, entities: &mut [Entity]) {
    for entity in entities.iter_mut() {
        entity.metadata.extra.remove(VALUE_ESCAPE_RECORD_KEY);
        entity.metadata.extra.remove(PROTOCOL_MEMBER_KEY);
        entity.metadata.extra.remove(NON_ENUMERABLE_MEMBER_KEY);
    }
    let Some(census) = census else { return };
    for stamp in &census.protocol_members {
        if let Some(holder) = member_at(entities, &stamp.name, stamp.at) {
            holder
                .metadata
                .extra
                .insert(PROTOCOL_MEMBER_KEY.into(), stamp.member.to_metadata());
        }
    }
    for stamp in &census.non_enumerable {
        if let Some(holder) = member_at(entities, &stamp.name, stamp.at) {
            if holder.kind == EntityKind::Method {
                holder.metadata.extra.insert(
                    NON_ENUMERABLE_MEMBER_KEY.into(),
                    serde_json::Value::Bool(true),
                );
            }
        }
    }
    let carrier = entities
        .iter()
        .position(kin_model::is_file_module_surface)
        .or_else(|| {
            entities
                .iter()
                .enumerate()
                .min_by_key(|(_, entity)| {
                    (
                        entity
                            .span
                            .as_ref()
                            .map_or(usize::MAX, |span| span.start_byte),
                        entity.id,
                    )
                })
                .map(|(index, _)| index)
        });
    if let Some(carrier) = carrier {
        entities[carrier]
            .metadata
            .extra
            .insert(VALUE_ESCAPE_RECORD_KEY.into(), census.record.to_metadata());
    }
}

// ── Shared syntax helpers for the collectors ──────────────────────────────

/// The node's text with whitespace collapsed, cut to `max` characters.
pub(crate) fn collapsed_text(node: Node<'_>, source: &[u8], max: usize) -> String {
    let text = node.utf8_text(source).unwrap_or("");
    let mut out = String::new();
    for (index, word) in text.split_whitespace().enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() > max {
            break;
        }
    }
    if out.chars().count() > max {
        out = out.chars().take(max).collect();
    }
    out
}

/// The node's 0-based start line.
pub(crate) fn line_of(node: Node<'_>) -> u32 {
    node.start_position().row as u32
}

/// The node's text, or an empty string.
pub(crate) fn text_of<'s>(node: Node<'_>, source: &'s [u8]) -> &'s str {
    node.utf8_text(source).unwrap_or("")
}

/// Whether `child` is the node `parent` holds under `field`.
pub(crate) fn is_field(parent: Node<'_>, field: &str, child: Node<'_>) -> bool {
    parent
        .child_by_field_name(field)
        .is_some_and(|held| held.id() == child.id())
}

/// Every region the grammar could not parse, with the names it spells:
/// adjacent error nodes merge into one region.
pub(crate) fn unparsed_lines(tree: &Tree, source: &[u8]) -> Vec<kin_model::UnparsedRegion> {
    let mut regions: Vec<(u32, u32, std::collections::BTreeSet<String>)> = Vec::new();
    if !tree.root_node().has_error() {
        return Vec::new();
    }
    let mut stack = vec![tree.root_node()];
    let mut found = Vec::new();
    while let Some(node) = stack.pop() {
        if !node.has_error() {
            continue;
        }
        if node.is_error() || node.is_missing() {
            found.push(node);
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    found.sort_by_key(|node| node.start_byte());
    for node in found {
        let first = node.start_position().row as u32;
        let last = node.end_position().row as u32;
        let mut names = std::collections::BTreeSet::new();
        let mut inner = vec![node];
        while let Some(current) = inner.pop() {
            if current.child_count() == 0 {
                let text = text_of(current, source)
                    .trim_matches(|character| matches!(character, '"' | '\'' | '`'));
                if is_name(text) {
                    names.insert(text.to_string());
                }
                continue;
            }
            let mut cursor = current.walk();
            inner.extend(current.children(&mut cursor));
        }
        match regions.last_mut() {
            Some(region) if first <= region.1 + 1 => {
                region.1 = region.1.max(last);
                region.2.extend(names);
            }
            _ => regions.push((first, last, names)),
        }
    }
    regions
        .into_iter()
        .map(|(first, last, names)| kin_model::UnparsedRegion {
            first,
            last,
            names: names.into_iter().collect(),
        })
        .collect()
}

/// The literal text of a string node with no interpolation, escape or
/// prefix, for either grammar: its quotes stripped.
pub(crate) fn plain_string_literal(node: Node<'_>, source: &[u8]) -> Option<String> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }
    let mut cursor = node.walk();
    if node.named_children(&mut cursor).any(|child| {
        !matches!(
            child.kind(),
            "string_content" | "string_fragment" | "string_start" | "string_end"
        )
    }) {
        return None;
    }
    let text = text_of(node, source);
    let quote = text.chars().next()?;
    if !matches!(quote, '"' | '\'' | '`') {
        return None;
    }
    let inner = text.strip_prefix(quote)?.strip_suffix(quote)?;
    (!inner.contains('\\')).then(|| inner.to_string())
}

/// The literal runs a computed key provably holds, in the shape a
/// [`kin_model::ReflectiveSite`] records: the run it starts with, the run it
/// ends with, and the runs between them in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct KeyLiterals {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub parts: Vec<String>,
}

impl KeyLiterals {
    /// The literals of a key made of `pieces` in order, `Some(text)` for a
    /// literal run and `None` for a computed one. A key with no computed
    /// piece is one literal, recorded as its prefix: that admits every name
    /// it starts, which is more than the one it spells and never less.
    pub(crate) fn from_pieces(pieces: Vec<Option<String>>) -> Self {
        // Merge adjacent literal runs and drop empty ones.
        let mut merged: Vec<Option<String>> = Vec::new();
        for piece in pieces {
            match (piece, merged.last_mut()) {
                (Some(text), _) if text.is_empty() => {}
                (Some(text), Some(Some(last))) => last.push_str(&text),
                (None, Some(None)) => {}
                (piece, _) => merged.push(piece),
            }
        }
        if !merged.iter().any(Option::is_none) {
            return Self {
                prefix: merged.into_iter().flatten().next(),
                suffix: None,
                parts: Vec::new(),
            };
        }
        let prefix = match merged.first() {
            Some(Some(text)) => Some(text.clone()),
            _ => None,
        };
        let suffix = match merged.last() {
            Some(Some(text)) => Some(text.clone()),
            _ => None,
        };
        let inner_start = usize::from(prefix.is_some());
        let inner_end = merged.len() - usize::from(suffix.is_some());
        let parts = merged[inner_start..inner_end]
            .iter()
            .flatten()
            .cloned()
            .collect();
        Self {
            prefix,
            suffix,
            parts,
        }
    }
}

/// Append the pieces of a formatting template: its literal runs, and one
/// computed piece per conversion. `marker` is `%` for printf-style
/// templates, where `%%` is a literal percent, and `{` for brace templates,
/// where `{{` and `}}` are literal braces.
pub(crate) fn push_template(template: &str, marker: char, pieces: &mut Vec<Option<String>>) {
    let mut literal = String::new();
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        match (marker, character) {
            ('%', '%') => {
                if chars.peek() == Some(&'%') {
                    chars.next();
                    literal.push('%');
                    continue;
                }
                // A conversion runs to its type letter.
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                pieces.push(Some(std::mem::take(&mut literal)));
                pieces.push(None);
            }
            ('{', '{') => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    literal.push('{');
                    continue;
                }
                for next in chars.by_ref() {
                    if next == '}' {
                        break;
                    }
                }
                pieces.push(Some(std::mem::take(&mut literal)));
                pieces.push(None);
            }
            ('{', '}') => {
                if chars.peek() == Some(&'}') {
                    chars.next();
                }
                literal.push('}');
            }
            (_, other) => literal.push(other),
        }
    }
    pieces.push(Some(literal));
}

/// Whether `name` is an identifier a call could spell: letters, digits, `_`
/// and `$`, not starting with a digit.
pub(crate) fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars
            .all(|character| character.is_alphanumeric() || character == '_' || character == '$')
}

#[cfg(test)]
mod corpus_probe {
    /// Temporary corpus probe: `KIN_ESCAPE_CENSUS_DIR=<repo> cargo test -p kin-parser
    /// corpus_probe -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn corpus_probe() {
        let Ok(dir) = std::env::var("KIN_ESCAPE_CENSUS_DIR") else {
            return;
        };
        let names: Vec<String> = std::env::var("KIN_ESCAPE_NAMES")
            .unwrap_or_default()
            .split(',')
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        let registry = crate::languages::AdapterRegistry::new();
        let mut stack = vec![std::path::PathBuf::from(&dir)];
        let (mut files, mut bytes, mut max, mut values, mut unparsed) =
            (0usize, 0usize, 0usize, 0usize, 0usize);
        let mut reflective: std::collections::BTreeMap<String, usize> = Default::default();
        let mut protocol: std::collections::BTreeMap<String, usize> = Default::default();
        let mut contexts: std::collections::BTreeMap<String, usize> = Default::default();
        // KIN_ESCAPE_FOCI="containsElement:method;advanceTimers:private:packages/scheduler/src/forks/Scheduler.js"
        let foci: Vec<(String, String, String)> = std::env::var("KIN_ESCAPE_FOCI")
            .unwrap_or_default()
            .split(';')
            .filter(|spec| !spec.is_empty())
            .map(|spec| {
                let mut parts = spec.splitn(3, ':');
                (
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                    parts.next().unwrap_or("").to_string(),
                )
            })
            .collect();
        let mut residue: std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, usize>,
        > = Default::default();
        let started = std::time::Instant::now();
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                if path.is_dir() {
                    if !matches!(name.as_str(), ".git" | "node_modules" | ".kin" | "build") {
                        stack.push(path);
                    }
                    continue;
                }
                let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
                    continue;
                };
                if !matches!(ext, "py" | "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx") {
                    continue;
                }
                let Ok(source) = std::fs::read(&path) else {
                    continue;
                };
                let Some(adapter) = registry.get_by_extension_and_content(ext, &source) else {
                    continue;
                };
                let Ok(tree) = adapter.parse(&source) else {
                    continue;
                };
                let Some(census) = super::census_file(&tree, &source, adapter.language_id()) else {
                    continue;
                };
                files += 1;
                let size = serde_json::to_string(&census.record.to_metadata())
                    .unwrap()
                    .len();
                bytes += size;
                max = max.max(size);
                if !census.record.unparsed.is_empty() {
                    unparsed += 1;
                    if names.iter().any(|name| name == "*unparsed") {
                        println!(
                            "UNPARSED {} {:?}",
                            path.display(),
                            census
                                .record
                                .unparsed
                                .iter()
                                .map(|region| (region.first, region.last, region.names.len()))
                                .collect::<Vec<_>>()
                        );
                    }
                }
                for (name, occurrences) in &census.record.values {
                    values += occurrences.len();
                    for occurrence in occurrences {
                        *contexts
                            .entry(occurrence.context.wire().to_string())
                            .or_default() += 1;
                        if names.contains(name) {
                            println!(
                                "VALUE {name} {}:{} {:?}",
                                path.display(),
                                occurrence.line + 1,
                                occurrence
                            );
                        }
                    }
                }
                let relative = path
                    .strip_prefix(&dir)
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string();
                for (name, kind, file) in &foci {
                    for site in &census.record.reflective {
                        if site.form == kin_model::ReflectiveForm::DynamicImport {
                            continue;
                        }
                        // The census rule without aliases: a member (`method`,
                        // or `class-method` when not enumerable), a
                        // module-private function (`private`) or any other.
                        let member = kind.ends_with("method");
                        let own_file = &relative == file;
                        if site.enumerable_only && kind == "class-method" {
                            continue;
                        }
                        let as_module = !member && (kind != "private" || own_file);
                        let reaches = match site.reach {
                            kin_model::ReflectiveReach::Nothing => false,
                            kin_model::ReflectiveReach::OwnModule => own_file && !member,
                            kin_model::ReflectiveReach::Members => member,
                            kin_model::ReflectiveReach::Any => match &site.receiver {
                                Some(kin_model::LookupReceiver::Declared { .. }) => member,
                                Some(kin_model::LookupReceiver::Module) => as_module,
                                None => member || as_module,
                            },
                        };
                        if reaches && site.key_admits(name) {
                            *residue
                                .entry(format!("{name}:{kind}"))
                                .or_default()
                                .entry(relative.clone())
                                .or_default() += 1;
                            if names.iter().any(|name| name == "*residue") {
                                println!(
                                    "RESIDUE_SITE {name}:{kind} {relative}:{} {:?} {}",
                                    site.line + 1,
                                    site.form,
                                    site.excerpt
                                );
                            }
                        }
                    }
                }
                for site in &census.record.reflective {
                    *reflective
                        .entry(format!(
                            "{}/{:?}/called={}",
                            site.form.wire(),
                            site.reach,
                            site.called
                        ))
                        .or_default() += 1;
                    if names.iter().any(|name| name == "*reflective") {
                        println!(
                            "REFLECT {}:{} {:?} {}",
                            path.display(),
                            site.line + 1,
                            site.form,
                            site.excerpt
                        );
                    }
                }
                for site in &census.record.protocol {
                    *protocol.entry(site.form.wire().to_string()).or_default() += 1;
                    if site.name.as_ref().is_some_and(|name| names.contains(name)) {
                        println!("PROTOCOL {}:{} {:?}", path.display(), site.line + 1, site);
                    }
                }
            }
        }
        println!("files={files} record_bytes={bytes} max={max} values={values} unparsed_files={unparsed} took={:?}", started.elapsed());
        println!("contexts={contexts:?}");
        println!("reflective={reflective:?}");
        println!("protocol={protocol:?}");
        for (name, files) in &residue {
            let total: usize = files.values().sum();
            let mut top: Vec<(&String, &usize)> = files.iter().collect();
            top.sort_by(|left, right| right.1.cmp(left.1));
            println!(
                "RESIDUE {name} reflective={total} files={} top={:?}",
                files.len(),
                top.into_iter().take(8).collect::<Vec<_>>()
            );
        }
    }
}
