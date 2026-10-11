// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Reserved per-file evidence records, carried from extraction to the linker.
//!
//! Some facts about a file are known only while its syntax tree is in hand:
//! which declarations the grammar shows and which of them extraction admitted
//! as entities, where the parse recovered from an error, and which call sites
//! no named call relation carries. The linker writes those facts into the
//! file's coverage certificate, and it receives a file's parse output only as
//! [`crate::ExtractedRelation`]s, so they travel on that seam the way the call
//! coverage markers already do. [`crate::ParseOutput`] and the linker's input
//! structs stay source compatible.
//!
//! Every record is inert everywhere else. Its kind is `DependsOn`, and both its
//! source and destination name are the record's reserved label, which no
//! declaration can carry, so a consumer that never learned to skip it resolves
//! neither end and mints nothing. The payload lives in `receiver`, split on an
//! ASCII unit separator, and `import_source` stays empty so no import resolver
//! reads it as a module path. Consumers skip the whole family through
//! [`is_reserved_parser_record`].

use kin_model::RelationKind;
use tree_sitter::{Node, Tree};

use crate::adapter::{site_from_node, LanguageAdapter};
use crate::declaration_census::{take_census, DeclarationCensus, UnadmittedDeclaration};
use crate::extract::{ExtractedRelation, ParseOutput, RelationSite};

/// The label every record in this family starts with.
pub const FILE_EVIDENCE_PREFIX: &str = "kin-internal://file-evidence/v1/";

/// A file's declaration census: how many declarations the grammar shows and
/// how many of them extraction admitted. Payload `seen<US>admitted`.
pub const DECLARATION_CENSUS_RECORD: &str = "kin-internal://file-evidence/v1/declaration-census";

/// One declaration the grammar shows that extraction did not admit. Payload
/// `name<US>kind<US>reason<US>owner`; the site is the declaration node.
pub const UNADMITTED_DECLARATION_RECORD: &str =
    "kin-internal://file-evidence/v1/unadmitted-declaration";

/// One region where the parse recovered from a syntax error. No payload; the
/// site is the outermost error or missing node.
pub const PARSE_ERROR_RECORD: &str = "kin-internal://file-evidence/v1/parse-error";

/// How many call expressions an adapter that tracks extraction gaps counted in
/// the file. Payload `count`. Its presence is what separates "no gaps" from
/// "no gap tracking", which the gap records alone cannot.
pub const CALL_EXTRACTION_CENSUS_RECORD: &str =
    "kin-internal://file-evidence/v1/call-extraction-census";

/// One call site that no named call relation carries. Payload
/// `reason<US>owner`; the site is the call expression.
pub const CALL_EXTRACTION_GAP_RECORD: &str = "kin-internal://file-evidence/v1/call-extraction-gap";

/// The call expression's callee could not be named (a subscript, a call
/// result, a computed member), so no call relation exists for it.
pub const GAP_CALLEE_NOT_NAMEABLE: &str = "callee_not_nameable";

/// The call expression sits where no callable declaration owns it, such as
/// module or class body code, so extraction attributed no call relation to it.
pub const GAP_OUTSIDE_CALLABLE: &str = "outside_callable";

const SEPARATOR: char = '\u{1f}';

fn record(label: &str, payload: Option<String>, site: Option<RelationSite>) -> ExtractedRelation {
    ExtractedRelation {
        kind: RelationKind::DependsOn,
        src_name: label.to_string(),
        dst_name: label.to_string(),
        import_source: None,
        call_shape: None,
        receiver: payload,
        site,
    }
}

fn fields(relation: &ExtractedRelation, label: &str) -> Option<Vec<String>> {
    if relation.kind != RelationKind::DependsOn
        || relation.src_name != label
        || relation.dst_name != label
    {
        return None;
    }
    Some(
        relation
            .receiver
            .as_deref()
            .unwrap_or_default()
            .split(SEPARATOR)
            .map(str::to_string)
            .collect(),
    )
}

/// Whether a raw relation belongs to this record family.
pub fn is_file_evidence_record(relation: &ExtractedRelation) -> bool {
    relation.kind == RelationKind::DependsOn
        && (relation.src_name.starts_with(FILE_EVIDENCE_PREFIX)
            || relation.dst_name.starts_with(FILE_EVIDENCE_PREFIX))
}

/// Whether a raw relation is any reserved parser record rather than a graph
/// relation: a call coverage marker, an exact import witness, or a file
/// evidence record. Every consumer that turns raw relations into edges skips
/// these.
pub fn is_reserved_parser_record(relation: &ExtractedRelation) -> bool {
    crate::extract::is_call_extraction_incomplete_marker(relation)
        || crate::import_witness::claims_import_witness(relation)
        || is_file_evidence_record(relation)
}

/// Append the records for one declaration census.
pub fn push_declaration_census(relations: &mut Vec<ExtractedRelation>, census: &DeclarationCensus) {
    relations.push(record(
        DECLARATION_CENSUS_RECORD,
        Some(format!("{}{SEPARATOR}{}", census.seen, census.admitted)),
        None,
    ));
    for declaration in &census.unadmitted {
        relations.push(record(
            UNADMITTED_DECLARATION_RECORD,
            Some(format!(
                "{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}",
                declaration.name,
                declaration.kind.label(),
                declaration.reason,
                declaration.owner.as_deref().unwrap_or_default(),
            )),
            Some(declaration.site.clone()),
        ));
    }
}

/// Read a file's declaration census back from its raw relations. `None` when
/// the adapter took no census, which a reader treats as unmeasured.
pub fn declaration_census_of(relations: &[ExtractedRelation]) -> Option<DeclarationCensus> {
    let mut summary = None;
    let mut unadmitted = Vec::new();
    for relation in relations {
        if let Some(fields) = fields(relation, DECLARATION_CENSUS_RECORD) {
            let [seen, admitted] = fields.as_slice() else {
                continue;
            };
            if let (Ok(seen), Ok(admitted)) = (seen.parse(), admitted.parse()) {
                summary = Some((seen, admitted));
            }
        } else if let Some(fields) = fields(relation, UNADMITTED_DECLARATION_RECORD) {
            let [name, kind, reason, owner] = fields.as_slice() else {
                continue;
            };
            let (Some(kind), Some(site)) = (
                crate::declaration_census::DeclarationKind::from_label(kind),
                relation.site.clone(),
            ) else {
                continue;
            };
            unadmitted.push(UnadmittedDeclaration {
                name: name.clone(),
                kind,
                reason: reason.clone(),
                owner: (!owner.is_empty()).then(|| owner.clone()),
                site,
            });
        }
    }
    let (seen, admitted) = summary?;
    Some(DeclarationCensus {
        seen,
        admitted,
        unadmitted,
    })
}

/// The outermost error and missing regions of a parse, in source order.
pub fn parse_error_sites(tree: &Tree) -> Vec<RelationSite> {
    fn walk(node: &Node, out: &mut Vec<RelationSite>) {
        if node.is_error() || node.is_missing() {
            out.push(site_from_node(node));
            return;
        }
        if !node.has_error() {
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(&child, out);
        }
    }
    let mut out = Vec::new();
    walk(&tree.root_node(), &mut out);
    out
}

/// Append one record per parse error region.
pub fn push_parse_errors(relations: &mut Vec<ExtractedRelation>, sites: &[RelationSite]) {
    for site in sites {
        relations.push(record(PARSE_ERROR_RECORD, None, Some(site.clone())));
    }
}

/// The parse error regions a file's raw relations list.
pub fn parse_error_sites_of(relations: &[ExtractedRelation]) -> Vec<RelationSite> {
    relations
        .iter()
        .filter(|relation| fields(relation, PARSE_ERROR_RECORD).is_some())
        .filter_map(|relation| relation.site.clone())
        .collect()
}

/// One call site no named call relation carries, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallExtractionGap {
    pub reason: String,
    pub owner: Option<String>,
    pub site: RelationSite,
}

/// Record how many call expressions a gap-tracking adapter counted.
pub fn call_extraction_census_record(count: u64) -> ExtractedRelation {
    record(CALL_EXTRACTION_CENSUS_RECORD, Some(count.to_string()), None)
}

/// The call census a gap-tracking adapter recorded, or `None` when the adapter
/// tracks no gaps.
pub fn call_extraction_census_of(relations: &[ExtractedRelation]) -> Option<u64> {
    relations.iter().find_map(|relation| {
        fields(relation, CALL_EXTRACTION_CENSUS_RECORD)?
            .first()?
            .parse()
            .ok()
    })
}

/// Record one call extraction gap.
pub fn call_extraction_gap_record(
    reason: &str,
    owner: Option<&str>,
    site: RelationSite,
) -> ExtractedRelation {
    record(
        CALL_EXTRACTION_GAP_RECORD,
        Some(format!("{reason}{SEPARATOR}{}", owner.unwrap_or_default())),
        Some(site),
    )
}

/// The call extraction gaps a file's raw relations list. Read beside
/// [`call_extraction_census_of`], which says whether the adapter tracked any.
pub fn call_extraction_gaps_of(relations: &[ExtractedRelation]) -> Vec<CallExtractionGap> {
    relations
        .iter()
        .filter_map(|relation| {
            let fields = fields(relation, CALL_EXTRACTION_GAP_RECORD)?;
            let [reason, owner] = fields.as_slice() else {
                return None;
            };
            Some(CallExtractionGap {
                reason: reason.clone(),
                owner: (!owner.is_empty()).then(|| owner.clone()),
                site: relation.site.clone()?,
            })
        })
        .collect()
}

/// Attach the file evidence an adapter-independent pass can take from a parse:
/// the declaration census when the adapter takes one, and the parse's error
/// regions. Called by every ingest path right after extraction, before the
/// output is converted to graph entities.
pub fn attach_file_evidence(
    adapter: &dyn LanguageAdapter,
    tree: &Tree,
    source: &[u8],
    output: &mut ParseOutput,
) {
    if let Some(seen) = adapter.declaration_census(tree, source) {
        let census = take_census(seen, &output.entities);
        push_declaration_census(&mut output.relations, &census);
    }
    if tree.root_node().has_error() {
        push_parse_errors(&mut output.relations, &parse_error_sites(tree));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::declaration_census::DeclarationKind;

    fn site(start: usize) -> RelationSite {
        RelationSite {
            start_byte: start,
            end_byte: start + 3,
            start_line: 1,
            start_col: 0,
            end_line: 1,
            end_col: 3,
            syntactic_role: None,
        }
    }

    #[test]
    fn records_round_trip_and_are_reserved() {
        let census = DeclarationCensus {
            seen: 3,
            admitted: 1,
            unadmitted: vec![
                UnadmittedDeclaration {
                    name: "inner".into(),
                    kind: DeclarationKind::Function,
                    reason: "nested_in_callable".into(),
                    owner: Some("outer".into()),
                    site: site(10),
                },
                UnadmittedDeclaration {
                    name: "value".into(),
                    kind: DeclarationKind::Attribute,
                    reason: "class_attribute".into(),
                    owner: None,
                    site: site(20),
                },
            ],
        };
        let mut relations = Vec::new();
        push_declaration_census(&mut relations, &census);
        push_parse_errors(&mut relations, &[site(30)]);
        relations.push(call_extraction_gap_record(
            GAP_CALLEE_NOT_NAMEABLE,
            Some("outer"),
            site(40),
        ));
        assert!(relations.iter().all(is_reserved_parser_record));
        assert!(relations
            .iter()
            .all(|relation| !crate::extract::is_call_extraction_incomplete_marker(relation)));
        assert_eq!(declaration_census_of(&relations), Some(census));
        assert_eq!(parse_error_sites_of(&relations), vec![site(30)]);
        assert_eq!(
            call_extraction_gaps_of(&relations),
            vec![CallExtractionGap {
                reason: GAP_CALLEE_NOT_NAMEABLE.into(),
                owner: Some("outer".into()),
                site: site(40),
            }]
        );
    }

    #[test]
    fn a_file_without_a_census_reads_unmeasured() {
        assert_eq!(declaration_census_of(&[]), None);
    }
}
