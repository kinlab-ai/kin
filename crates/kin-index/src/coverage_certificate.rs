// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The detail entries of a file's coverage certificate.
//!
//! The certificate's first entries answer yes or no: did the file parse fully
//! and was every call named with a proven target, and how many import
//! statements reached the repository. A no could never say what was missing,
//! and it folded three facts into one: a file whose parse was clean but which
//! held one call through an untyped receiver read exactly like a file the
//! parser could not read. These entries record each fact on its own and list
//! what is missing:
//!
//! - parse completeness, with the regions where the parse recovered from an
//!   error;
//! - the declaration census: declarations the grammar shows, how many
//!   extraction admitted, and every one it did not, with name, kind, span and
//!   reason;
//! - call extraction: the call census, every call site no named call relation
//!   carries, and how many named calls had a receiver the parser could not
//!   type, which is a resolution question the language-server sweep settles
//!   rather than an extraction gap;
//! - import outcomes: how many unresolved imports are provably external, and
//!   every one that is not;
//! - receivers proven outside: every named call whose receiver the adapter's
//!   value-origin facts type as a value of a type the repository does not
//!   declare, with the chain that proved it. Such a call reaches no
//!   declaration of the repository, and the call-site ledger settles it as
//!   going elsewhere. Only a builder holding the link context can prove one,
//!   so a builder without it records none.
//!
//! Every entry is reconstructible from the certificate itself, so the shared
//! validator can keep requiring whole-relation equality with what the factory
//! would mint.

use std::collections::HashSet;

use kin_model::relation::RelationEvidence;
use kin_model::{FilePathId, ParseCompleteness, Relation, SourceSpan};
use kin_parser::declaration_census::DeclarationKind;
use kin_parser::import_origin::ImportSpecifierRoot;
use kin_parser::{ExtractedRelation, FileImport};

/// Parse completeness on its own: `token` is `full`, `partial`, `failed` or
/// `missing`, and `occurrence_count` is how many error regions the parse
/// recovered from.
pub const PARSE_COMPLETENESS_V1: &str = "parse_completeness_v1";

/// One error region, by `source_span`. At most [`PARSE_ERROR_SPAN_LIMIT`] are
/// listed; the completeness entry counts them all.
pub const PARSE_ERROR_SPAN_V1: &str = "parse_error_span_v1";

/// The declaration census: `occurrence_count` declarations the grammar shows
/// and `token` of them admitted as entities, as a decimal string. Absent when
/// the adapter takes no census.
pub const DECLARATION_COVERAGE_V1: &str = "declaration_coverage_v1";

/// One declaration extraction did not admit. `token` is
/// `name<US>kind<US>reason<US>owner` and `source_span` its declaration node.
pub const UNADMITTED_DECLARATION_V1: &str = "unadmitted_declaration_v1";

/// The call census of an adapter that tracks extraction gaps: `token` is the
/// census and `occurrence_count` the number of gaps. Absent when the adapter
/// tracks none.
pub const CALL_EXTRACTION_CENSUS_V1: &str = "call_extraction_census_v1";

/// One call site no named call relation carries. `token` is
/// `reason<US>owner` and `source_span` the call expression.
pub const CALL_EXTRACTION_GAP_V1: &str = "call_extraction_gap_v1";

/// How many named calls had a receiver the parser could not type, as `token`.
pub const CALL_RECEIVER_UNRESOLVED_V1: &str = "call_receiver_unresolved_v1";

/// How many import statements that reached no repository file are provably
/// external, as `token`.
pub const IMPORT_EXTERNAL_COVERAGE_V1: &str = "import_external_coverage_v1";

/// One import statement that reached no repository file and is not provably
/// external. `token` is the specifier and `source_span` the statement.
pub const UNRESOLVED_IMPORT_V1: &str = "unresolved_import_v1";

/// One named call whose receiver the value-origin facts prove to hold a value
/// of a type outside the repository. `token` is `method<US>chain` and
/// `source_span` the call expression.
pub const RECEIVER_OUTSIDE_V1: &str = "receiver_outside_v1";

/// One read of a member that is not a call (`partial(runner.invoke, cli)`),
/// through a receiver the value-origin facts prove to hold a value whose
/// member of that name is no declaration of the repository. `token` is
/// `line<US>member<US>receiver<US>owner<US>chain`, the line counted from zero as the
/// file's value-escape record counts it: a reader matches the entry against
/// that record, which addresses a read by its line, so the entry carries no
/// span.
pub const RECEIVER_OUTSIDE_READ_V1: &str = "receiver_outside_read_v1";

/// One read of a member that is not a call, through a receiver the
/// value-origin facts type as a class of the repository. `token` is
/// `line<US>member<US>receiver<US>class`, addressed by line as
/// [`RECEIVER_OUTSIDE_READ_V1`] is. It says what the receiver is, never what
/// the read gets: a census decides, for the member it is asked about, whether
/// a value of that class could hold it.
pub const RECEIVER_TYPED_READ_V1: &str = "receiver_typed_read_v1";

/// One named call whose receiver the value-origin facts bind, where the
/// chain they spell stops short of a method. `token` is JSON naming the
/// receiver, the method, the links established and the one missing, and
/// `source_span` the call expression.
pub const RECEIVER_ORIGIN_GAP_V1: &str = "receiver_origin_gap_v1";

/// How many parse error regions a certificate lists.
pub const PARSE_ERROR_SPAN_LIMIT: usize = 16;

const SEPARATOR: char = '\u{1f}';

/// Every label this module writes.
pub const DETAIL_LABELS: &[&str] = &[
    PARSE_COMPLETENESS_V1,
    PARSE_ERROR_SPAN_V1,
    DECLARATION_COVERAGE_V1,
    UNADMITTED_DECLARATION_V1,
    CALL_EXTRACTION_CENSUS_V1,
    CALL_EXTRACTION_GAP_V1,
    CALL_RECEIVER_UNRESOLVED_V1,
    IMPORT_EXTERNAL_COVERAGE_V1,
    UNRESOLVED_IMPORT_V1,
    RECEIVER_OUTSIDE_V1,
    RECEIVER_OUTSIDE_READ_V1,
    RECEIVER_TYPED_READ_V1,
    RECEIVER_ORIGIN_GAP_V1,
];

/// A declaration the grammar shows that extraction did not admit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnadmittedDeclarationEntry {
    pub name: String,
    pub kind: DeclarationKind,
    pub reason: String,
    pub owner: Option<String>,
    pub span: SourceSpan,
}

/// A call site no named call relation carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallGapEntry {
    pub reason: String,
    pub owner: Option<String>,
    pub span: SourceSpan,
}

/// A named call proved to reach no declaration of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverOutsideEntry {
    /// The member the call names.
    pub method: String,
    /// The facts that typed its receiver, ending at the external type.
    pub chain: String,
    /// The call expression.
    pub span: SourceSpan,
}

/// A member read, not a call, proved to read no declaration of the
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReceiverOutsideReadEntry {
    /// The line of the read, counted from zero.
    pub line: u32,
    /// The member the read names.
    pub member: String,
    /// The receiver it is read off, as written.
    pub receiver: String,
    /// The type outside the repository whose member the read gets.
    pub owner: String,
    /// The facts that typed the receiver, ending outside the repository.
    pub chain: String,
}

/// A member read, not a call, whose receiver is typed as a class of the
/// repository.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReceiverTypedReadEntry {
    /// The line of the read, counted from zero.
    pub line: u32,
    /// The member the read names.
    pub member: String,
    /// The receiver it is read off, as written.
    pub receiver: String,
    /// The class the receiver's value is declared, constructed or returned
    /// as, by its own name.
    pub class: String,
}

/// A named call whose receiver chain stops short of a method.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReceiverOriginGapEntry {
    /// The receiver the call is made through, as the scope binds it.
    pub receiver: String,
    /// The member the call names.
    pub method: String,
    /// The links the chain established, the receiver's binding first.
    pub steps: Vec<crate::receiver_origin::OriginStep>,
    /// The link it could not.
    pub missing: crate::receiver_origin::MissingStep,
    /// The call expression.
    #[serde(skip)]
    pub span: SourceSpan,
}

impl ReceiverOriginGapEntry {
    fn token(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    fn from_token(token: &str, span: SourceSpan) -> Option<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Token {
            receiver: String,
            method: String,
            steps: Vec<crate::receiver_origin::OriginStep>,
            missing: crate::receiver_origin::MissingStep,
        }
        let token: Token = serde_json::from_str(token).ok()?;
        Some(Self {
            receiver: token.receiver,
            method: token.method,
            steps: token.steps,
            missing: token.missing,
            span,
        })
    }
}

/// An import statement that is neither resolved nor provably external.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedImportEntry {
    pub specifier: String,
    pub span: SourceSpan,
}

/// The declaration census as the certificate records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarationCoverage {
    pub seen: usize,
    pub admitted: usize,
    pub unadmitted: Vec<UnadmittedDeclarationEntry>,
}

/// The call extraction census as the certificate records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallExtractionCoverage {
    pub census: u64,
    pub gaps: Vec<CallGapEntry>,
}

/// Everything the detail entries record for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateDetail {
    pub parse: Option<ParseCompleteness>,
    pub parse_errors: usize,
    pub parse_error_spans: Vec<SourceSpan>,
    pub declarations: Option<DeclarationCoverage>,
    pub call_extraction: Option<CallExtractionCoverage>,
    pub receiver_unresolved: usize,
    pub imports_external: usize,
    pub unresolved_imports: Vec<UnresolvedImportEntry>,
    /// Named calls proved to reach no declaration of the repository, in site
    /// order. Empty from a builder without the link context.
    pub receiver_outside: Vec<ReceiverOutsideEntry>,
    /// Member reads proved to read no declaration of the repository, in line
    /// order. Empty from a builder without the link context.
    pub receiver_outside_reads: Vec<ReceiverOutsideReadEntry>,
    /// Member reads whose receiver is typed as a class of the repository, in
    /// line order. Empty from a builder without the link context.
    pub receiver_typed_reads: Vec<ReceiverTypedReadEntry>,
    /// Named calls whose receiver chain stops short of a method, in site
    /// order. Empty from a builder without the link context.
    pub receiver_origin_gaps: Vec<ReceiverOriginGapEntry>,
}

/// How one file's imports came out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportOutcomes {
    pub external: usize,
    pub unresolved: Vec<UnresolvedImportEntry>,
}

fn parse_bucket(parse: Option<&ParseCompleteness>) -> &'static str {
    parse.map(ParseCompleteness::bucket).unwrap_or("missing")
}

fn parse_from_bucket(bucket: &str) -> Option<Option<ParseCompleteness>> {
    Some(match bucket {
        "full" => Some(ParseCompleteness::Full),
        "partial" => Some(ParseCompleteness::Partial(String::new())),
        "failed" => Some(ParseCompleteness::Failed(String::new())),
        "missing" => None,
        _ => return None,
    })
}

fn entry(file_path: &str, label: &str, token: String, count: usize) -> RelationEvidence {
    RelationEvidence {
        token: Some(token),
        source_path: Some(file_path.to_string()),
        parser_rule: Some(label.to_string()),
        occurrence_count: u32::try_from(count).unwrap_or(u32::MAX),
        ..RelationEvidence::default()
    }
}

fn spanned(file_path: &str, label: &str, token: String, span: &SourceSpan) -> RelationEvidence {
    RelationEvidence {
        source_span: Some(span.clone()),
        ..entry(file_path, label, token, 1)
    }
}

/// Normalize a span the way it reads back, so reconstruction compares equal:
/// the bucket's detail strings and the span's file are not recoverable
/// otherwise.
fn normalized_parse(parse: Option<&ParseCompleteness>) -> Option<ParseCompleteness> {
    parse_from_bucket(parse_bucket(parse)).flatten()
}

impl CertificateDetail {
    /// Gather the detail of one file from its parse output and its import
    /// outcomes.
    pub fn of_file(
        file_path: &str,
        relations: &[ExtractedRelation],
        parse: Option<&ParseCompleteness>,
        imports: ImportOutcomes,
    ) -> Self {
        let file = FilePathId::new(file_path);
        let errors = kin_parser::file_evidence::parse_error_sites_of(relations);
        let declarations =
            kin_parser::file_evidence::declaration_census_of(relations).map(|census| {
                DeclarationCoverage {
                    seen: census.seen,
                    admitted: census.admitted,
                    unadmitted: census
                        .unadmitted
                        .into_iter()
                        .map(|declaration| UnadmittedDeclarationEntry {
                            name: declaration.name,
                            kind: declaration.kind,
                            reason: declaration.reason,
                            owner: declaration.owner,
                            span: declaration.site.to_source_span(&file),
                        })
                        .collect(),
                }
            });
        let call_extraction =
            kin_parser::file_evidence::call_extraction_census_of(relations).map(|census| {
                CallExtractionCoverage {
                    census,
                    gaps: kin_parser::file_evidence::call_extraction_gaps_of(relations)
                        .into_iter()
                        .map(|gap| CallGapEntry {
                            reason: gap.reason,
                            owner: gap.owner,
                            span: gap.site.to_source_span(&file),
                        })
                        .collect(),
                }
            });
        let receiver_unresolved = relations
            .iter()
            .filter(|relation| {
                kin_parser::is_scoped_call_extraction_incomplete_marker(relation)
                    && relation.receiver.is_some()
            })
            .count();
        Self {
            parse: normalized_parse(parse),
            parse_errors: errors.len(),
            parse_error_spans: errors
                .iter()
                .take(PARSE_ERROR_SPAN_LIMIT)
                .map(|site| site.to_source_span(&file))
                .collect(),
            declarations,
            call_extraction,
            receiver_unresolved,
            imports_external: imports.external,
            unresolved_imports: imports.unresolved,
            receiver_outside: Vec::new(),
            receiver_outside_reads: Vec::new(),
            receiver_typed_reads: Vec::new(),
            receiver_origin_gaps: Vec::new(),
        }
    }

    /// Record the named calls the link context proved to reach no
    /// declaration of the repository, in site order.
    pub fn with_receiver_outside(
        mut self,
        entries: impl IntoIterator<Item = ReceiverOutsideEntry>,
    ) -> Self {
        let mut entries: Vec<ReceiverOutsideEntry> = entries.into_iter().collect();
        entries.sort_by(|a, b| {
            (a.span.start_byte, a.span.end_byte, &a.method, &a.chain).cmp(&(
                b.span.start_byte,
                b.span.end_byte,
                &b.method,
                &b.chain,
            ))
        });
        entries.dedup();
        self.receiver_outside = entries;
        self
    }

    /// Record the member reads the link context proved to read no
    /// declaration of the repository, in line order.
    pub fn with_receiver_outside_reads(
        mut self,
        entries: impl IntoIterator<Item = ReceiverOutsideReadEntry>,
    ) -> Self {
        let mut entries: Vec<ReceiverOutsideReadEntry> = entries.into_iter().collect();
        entries.sort();
        entries.dedup();
        self.receiver_outside_reads = entries;
        self
    }

    /// Record the member reads whose receiver the link context typed as a
    /// class of the repository, in line order.
    pub fn with_receiver_typed_reads(
        mut self,
        entries: impl IntoIterator<Item = ReceiverTypedReadEntry>,
    ) -> Self {
        let mut entries: Vec<ReceiverTypedReadEntry> = entries.into_iter().collect();
        entries.sort();
        entries.dedup();
        self.receiver_typed_reads = entries;
        self
    }

    /// Record the named calls whose receiver chain the link context could not
    /// finish, in site order.
    pub fn with_receiver_origin_gaps(
        mut self,
        entries: impl IntoIterator<Item = ReceiverOriginGapEntry>,
    ) -> Self {
        let mut entries: Vec<(String, ReceiverOriginGapEntry)> = entries
            .into_iter()
            .filter_map(|entry| Some((entry.token()?, entry)))
            .collect();
        entries.sort_by(|(a_token, a), (b_token, b)| {
            (a.span.start_byte, a.span.end_byte, a_token).cmp(&(
                b.span.start_byte,
                b.span.end_byte,
                b_token,
            ))
        });
        entries.dedup_by(|(a_token, a), (b_token, b)| a_token == b_token && a.span == b.span);
        self.receiver_origin_gaps = entries.into_iter().map(|(_, entry)| entry).collect();
        self
    }

    /// The certificate entries, in their fixed order.
    pub fn evidence(&self, file_path: &str) -> Vec<RelationEvidence> {
        let mut out = vec![entry(
            file_path,
            PARSE_COMPLETENESS_V1,
            parse_bucket(self.parse.as_ref()).to_string(),
            self.parse_errors,
        )];
        for span in &self.parse_error_spans {
            out.push(spanned(file_path, PARSE_ERROR_SPAN_V1, String::new(), span));
        }
        if let Some(declarations) = &self.declarations {
            out.push(entry(
                file_path,
                DECLARATION_COVERAGE_V1,
                declarations.admitted.to_string(),
                declarations.seen,
            ));
            for declaration in &declarations.unadmitted {
                out.push(spanned(
                    file_path,
                    UNADMITTED_DECLARATION_V1,
                    format!(
                        "{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}",
                        declaration.name,
                        declaration.kind.label(),
                        declaration.reason,
                        declaration.owner.as_deref().unwrap_or_default()
                    ),
                    &declaration.span,
                ));
            }
        }
        if let Some(extraction) = &self.call_extraction {
            out.push(entry(
                file_path,
                CALL_EXTRACTION_CENSUS_V1,
                extraction.census.to_string(),
                extraction.gaps.len(),
            ));
            for gap in &extraction.gaps {
                out.push(spanned(
                    file_path,
                    CALL_EXTRACTION_GAP_V1,
                    format!(
                        "{}{SEPARATOR}{}",
                        gap.reason,
                        gap.owner.as_deref().unwrap_or_default()
                    ),
                    &gap.span,
                ));
            }
        }
        out.push(entry(
            file_path,
            CALL_RECEIVER_UNRESOLVED_V1,
            self.receiver_unresolved.to_string(),
            1,
        ));
        out.push(entry(
            file_path,
            IMPORT_EXTERNAL_COVERAGE_V1,
            self.imports_external.to_string(),
            1,
        ));
        for import in &self.unresolved_imports {
            out.push(spanned(
                file_path,
                UNRESOLVED_IMPORT_V1,
                import.specifier.clone(),
                &import.span,
            ));
        }
        for outside in &self.receiver_outside {
            out.push(spanned(
                file_path,
                RECEIVER_OUTSIDE_V1,
                format!("{}{SEPARATOR}{}", outside.method, outside.chain),
                &outside.span,
            ));
        }
        for read in &self.receiver_outside_reads {
            out.push(entry(
                file_path,
                RECEIVER_OUTSIDE_READ_V1,
                format!(
                    "{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}",
                    read.line, read.member, read.receiver, read.owner, read.chain
                ),
                1,
            ));
        }
        for read in &self.receiver_typed_reads {
            out.push(entry(
                file_path,
                RECEIVER_TYPED_READ_V1,
                format!(
                    "{}{SEPARATOR}{}{SEPARATOR}{}{SEPARATOR}{}",
                    read.line, read.member, read.receiver, read.class
                ),
                1,
            ));
        }
        for gap in &self.receiver_origin_gaps {
            if let Some(token) = gap.token() {
                out.push(spanned(file_path, RECEIVER_ORIGIN_GAP_V1, token, &gap.span));
            }
        }
        out
    }

    /// Read the detail back from a certificate. `None` when the certificate
    /// carries none of it (an earlier format) or carries a malformed entry.
    pub fn from_certificate(relation: &Relation) -> Option<Self> {
        let of = |label: &'static str| {
            relation
                .evidence
                .iter()
                .filter(move |entry| entry.parser_rule.as_deref() == Some(label))
        };
        let single = |label: &'static str| -> Option<Option<&RelationEvidence>> {
            let mut found = of(label);
            let first = found.next();
            if found.next().is_some() {
                return None;
            }
            Some(first)
        };
        let count_token =
            |entry: &RelationEvidence| -> Option<usize> { entry.token.as_deref()?.parse().ok() };
        let parse_entry = single(PARSE_COMPLETENESS_V1)??;
        let parse = parse_from_bucket(parse_entry.token.as_deref()?)?;
        let parse_error_spans = of(PARSE_ERROR_SPAN_V1)
            .map(|entry| entry.source_span.clone())
            .collect::<Option<Vec<_>>>()?;
        let declarations = match single(DECLARATION_COVERAGE_V1)? {
            Some(summary) => Some(DeclarationCoverage {
                seen: summary.occurrence_count as usize,
                admitted: count_token(summary)?,
                unadmitted: of(UNADMITTED_DECLARATION_V1)
                    .map(|entry| {
                        let token = entry.token.as_deref()?;
                        let fields: Vec<&str> = token.split(SEPARATOR).collect();
                        let [name, kind, reason, owner] = fields.as_slice() else {
                            return None;
                        };
                        Some(UnadmittedDeclarationEntry {
                            name: name.to_string(),
                            kind: DeclarationKind::from_label(kind)?,
                            reason: reason.to_string(),
                            owner: (!owner.is_empty()).then(|| owner.to_string()),
                            span: entry.source_span.clone()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            }),
            None => {
                if of(UNADMITTED_DECLARATION_V1).next().is_some() {
                    return None;
                }
                None
            }
        };
        let call_extraction = match single(CALL_EXTRACTION_CENSUS_V1)? {
            Some(summary) => Some(CallExtractionCoverage {
                census: summary.token.as_deref()?.parse().ok()?,
                gaps: of(CALL_EXTRACTION_GAP_V1)
                    .map(|entry| {
                        let token = entry.token.as_deref()?;
                        let (reason, owner) = token.split_once(SEPARATOR)?;
                        Some(CallGapEntry {
                            reason: reason.to_string(),
                            owner: (!owner.is_empty()).then(|| owner.to_string()),
                            span: entry.source_span.clone()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            }),
            None => {
                if of(CALL_EXTRACTION_GAP_V1).next().is_some() {
                    return None;
                }
                None
            }
        };
        Some(Self {
            parse,
            parse_errors: parse_entry.occurrence_count as usize,
            parse_error_spans,
            declarations,
            call_extraction,
            receiver_unresolved: count_token(single(CALL_RECEIVER_UNRESOLVED_V1)??)?,
            imports_external: count_token(single(IMPORT_EXTERNAL_COVERAGE_V1)??)?,
            unresolved_imports: of(UNRESOLVED_IMPORT_V1)
                .map(|entry| {
                    Some(UnresolvedImportEntry {
                        specifier: entry.token.clone()?,
                        span: entry.source_span.clone()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            receiver_outside: of(RECEIVER_OUTSIDE_V1)
                .map(|entry| {
                    let (method, chain) = entry.token.as_deref()?.split_once(SEPARATOR)?;
                    Some(ReceiverOutsideEntry {
                        method: method.to_string(),
                        chain: chain.to_string(),
                        span: entry.source_span.clone()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            receiver_outside_reads: of(RECEIVER_OUTSIDE_READ_V1)
                .map(|entry| {
                    let mut fields = entry.token.as_deref()?.splitn(5, SEPARATOR);
                    Some(ReceiverOutsideReadEntry {
                        line: fields.next()?.parse().ok()?,
                        member: fields.next()?.to_string(),
                        receiver: fields.next()?.to_string(),
                        owner: fields.next()?.to_string(),
                        chain: fields.next()?.to_string(),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            receiver_typed_reads: of(RECEIVER_TYPED_READ_V1)
                .map(|entry| {
                    let mut fields = entry.token.as_deref()?.splitn(4, SEPARATOR);
                    Some(ReceiverTypedReadEntry {
                        line: fields.next()?.parse().ok()?,
                        member: fields.next()?.to_string(),
                        receiver: fields.next()?.to_string(),
                        class: fields.next()?.to_string(),
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            receiver_origin_gaps: of(RECEIVER_ORIGIN_GAP_V1)
                .map(|entry| {
                    ReceiverOriginGapEntry::from_token(
                        entry.token.as_deref()?,
                        entry.source_span.clone()?,
                    )
                })
                .collect::<Option<Vec<_>>>()?,
        })
    }

    /// Whether every call site is carried by a named call relation.
    /// `None` when the adapter tracks no gaps.
    pub fn extraction_complete(&self) -> Option<bool> {
        self.call_extraction
            .as_ref()
            .map(|extraction| extraction.gaps.is_empty())
    }
}

/// Whether an evidence entry is one this module writes.
pub fn is_detail_entry(entry: &RelationEvidence) -> bool {
    entry
        .parser_rule
        .as_deref()
        .is_some_and(|label| DETAIL_LABELS.contains(&label))
}

/// Directory and file names a set of repository paths holds, each file both
/// with and without its extensions: the names an import specifier's root can
/// match inside the repository.
pub fn repository_names<'a, I>(paths: I) -> HashSet<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut names = HashSet::new();
    for path in paths {
        let mut segments = path.split('/').peekable();
        while let Some(segment) = segments.next() {
            if segment.is_empty() {
                continue;
            }
            names.insert(segment.to_string());
            if segments.peek().is_none() {
                if let Some((stem, _)) = segment.split_once('.') {
                    if !stem.is_empty() {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
    }
    names
}

/// Whether an import that reached no repository file is provably external,
/// given the names the repository holds.
pub fn provably_external(root: &ImportSpecifierRoot, names: &HashSet<String>) -> bool {
    match root {
        ImportSpecifierRoot::Builtin => true,
        ImportSpecifierRoot::Names(roots) => {
            !roots.is_empty() && roots.iter().all(|root| !names.contains(root))
        }
        ImportSpecifierRoot::Relative
        | ImportSpecifierRoot::PackageInternal
        | ImportSpecifierRoot::Unknown => false,
    }
}

/// The root of `import`'s specifier, as the importing file's adapter reads it.
pub fn specifier_root(file_path: &str, import: &FileImport) -> ImportSpecifierRoot {
    static REGISTRY: std::sync::OnceLock<kin_parser::AdapterRegistry> = std::sync::OnceLock::new();
    let registry = REGISTRY.get_or_init(kin_parser::AdapterRegistry::new);
    kin_model::call_site_reading::language_of_path(file_path)
        .and_then(|language| registry.get_by_language(language))
        .map_or(ImportSpecifierRoot::Unknown, |adapter| {
            adapter.import_specifier_root(&import.module_path)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_model::{ArtifactId, GraphNodeId, RelationKind, RelationOrigin};

    fn span(start: usize) -> SourceSpan {
        SourceSpan {
            file: FilePathId::new("app.py"),
            start_byte: start,
            end_byte: start + 2,
            start_line: 3,
            start_col: 0,
            end_line: 3,
            end_col: 2,
        }
    }

    #[test]
    fn detail_round_trips_through_its_entries() {
        let detail = CertificateDetail {
            parse: Some(ParseCompleteness::Full),
            parse_errors: 0,
            parse_error_spans: vec![],
            declarations: Some(DeclarationCoverage {
                seen: 4,
                admitted: 3,
                unadmitted: vec![UnadmittedDeclarationEntry {
                    name: "inner".into(),
                    kind: DeclarationKind::Function,
                    reason: "nested_in_callable".into(),
                    owner: Some("outer".into()),
                    span: span(10),
                }],
            }),
            call_extraction: Some(CallExtractionCoverage {
                census: 9,
                gaps: vec![CallGapEntry {
                    reason: "outside_callable".into(),
                    owner: None,
                    span: span(20),
                }],
            }),
            receiver_unresolved: 2,
            imports_external: 3,
            unresolved_imports: vec![UnresolvedImportEntry {
                specifier: "flask.missing".into(),
                span: span(30),
            }],
            receiver_outside: vec![ReceiverOutsideEntry {
                method: "pop".into(),
                chain: "values: dict -> dict.pop is outside the repository".into(),
                span: span(40),
            }],
            receiver_outside_reads: vec![ReceiverOutsideReadEntry {
                line: 459,
                member: "invoke".into(),
                receiver: "runner".into(),
                owner: "click.testing.CliRunner".into(),
                chain: "runner: CliRunner(...) -> CliRunner.invoke is outside the repository"
                    .into(),
            }],
            receiver_typed_reads: vec![ReceiverTypedReadEntry {
                line: 392,
                member: "pop".into(),
                receiver: "flask.session".into(),
                class: "SessionMixinProxy".into(),
            }],
            receiver_origin_gaps: vec![ReceiverOriginGapEntry {
                receiver: "store".into(),
                method: "containsElement".into(),
                steps: vec![crate::receiver_origin::OriginStep {
                    step: crate::receiver_origin::OriginStepKind::Binding,
                    name: "store".into(),
                    via: None,
                    kind: None,
                    entity: Some(kin_model::EntityId::new()),
                    file: Some("app.py".into()),
                }],
                missing: crate::receiver_origin::MissingStep {
                    step: crate::receiver_origin::OriginStepKind::Provider,
                    name: "StoreContext".into(),
                    reason: "provider_value_unknown".into(),
                    kind: Some("react_context".into()),
                    file: Some("app.py".into()),
                },
                span: span(50),
            }],
        };
        let artifact = GraphNodeId::Artifact(ArtifactId::new());
        let relation = Relation {
            id: kin_model::RelationId::new(),
            kind: RelationKind::DependsOn,
            src: artifact,
            dst: artifact,
            confidence: 1.0,
            origin: RelationOrigin::Parsed,
            created_in: None,
            import_source: None,
            evidence: detail.evidence("app.py"),
        };
        assert!(relation.evidence.iter().all(is_detail_entry));
        assert_eq!(CertificateDetail::from_certificate(&relation), Some(detail));
    }

    #[test]
    fn external_needs_every_root_absent_from_the_repository() {
        let names = repository_names([
            "src/flask/app.py",
            "tests/conftest.py",
            "packages/react/index.js",
        ]);
        assert!(names.contains("flask") && names.contains("app") && names.contains("react"));
        let python =
            |path: &str| kin_parser::import_origin::ImportSpecifierRoot::Names(vec![path.into()]);
        assert!(provably_external(&python("os"), &names));
        assert!(!provably_external(&python("flask"), &names));
        assert!(!provably_external(&ImportSpecifierRoot::Relative, &names));
        assert!(provably_external(&ImportSpecifierRoot::Builtin, &names));
    }
}
