// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Source-owned obligations left by withdrawing a previously local binding.
//! These facts are independent of the parser's whole-file coverage certificate.

use crate::{IndexedFile, RelationResolution};
use kin_model::{
    ArtifactId, Entity, EntityId, FilePathId, GraphNodeId, GraphStore, Hash256, Relation,
    RelationKind, RelationOrigin,
};
use kin_parser::ExtractedRelation;

// Keep the public indexing API while sharing the exact model-owned wire codec.
pub use kin_model::binding_debt::{
    build_local_binding_debt, claims_local_binding_debt, decode_local_binding_debt,
    local_binding_debt_id, LocalBindingDebt, LocalBindingObligation, LOCAL_BINDING_DEBT_V1,
    LOCAL_BINDING_DEBT_V2,
};

/// Move the current location while retaining every historical occurrence byte.
/// The caller publishes the returned replacement with the exact artifact move.
pub fn relocate_local_binding_debt(
    from: &FilePathId,
    to: &FilePathId,
    artifact: ArtifactId,
    current_digest: Hash256,
    relation: &Relation,
) -> Result<Option<Relation>, String> {
    let Some(mut debt) = decode_local_binding_debt(from, artifact, relation)? else {
        return Ok(None);
    };
    if debt.observed_source_digest != current_digest {
        return Err("binding debt source digest differs from relocation source".into());
    }
    for obligation in &mut debt.obligations {
        obligation
            .prior_source_file
            .get_or_insert_with(|| from.clone());
    }
    debt.source_file = to.clone();
    let mut relocated = build_local_binding_debt(artifact, debt)?;
    relocated.created_in = relation.created_in;
    Ok(Some(relocated))
}

/// A consumer must treat Some and Err as incomplete binding knowledge. This
/// does not change the independent body, parse, or extraction observations.
pub fn inspect_local_binding_debt(
    file: &FilePathId,
    artifact: ArtifactId,
    current_digest: Hash256,
    relations: &[&Relation],
) -> Result<Option<LocalBindingDebt>, String> {
    let mut found = None;
    for relation in relations {
        if let Some(debt) = decode_local_binding_debt(file, artifact, relation)? {
            if found.is_some() {
                return Err("multiple binding debt records claim one source".into());
            }
            if debt.observed_source_digest != current_digest {
                return Err("binding debt requires revalidation against current source".into());
            }
            found = Some(debt);
        }
    }
    Ok(found)
}

/// Normalize a local import spelling to its exported binding. Moving an alias
/// does not remove the dependency, while changing the module/export actually
/// replaces it. Ambiguous local import maps cannot prove removal.
fn reference_identity(
    raw: &ExtractedRelation,
    file: &IndexedFile,
) -> Option<(RelationKind, String, Option<String>, Option<String>)> {
    let normalize = |name: &str| -> Option<String> {
        let (root, tail) = name.split_once('.').unwrap_or((name, ""));
        let mut bindings = std::collections::BTreeSet::new();
        for import in &file.imports {
            if raw
                .import_source
                .as_deref()
                .is_some_and(|module| module != import.module_path)
            {
                continue;
            }
            for specifier in &import.specifiers {
                if specifier.local_name == root {
                    bindings.insert((
                        import.module_path.as_str(),
                        specifier
                            .original_name
                            .as_deref()
                            .unwrap_or(&specifier.local_name),
                        specifier.is_default,
                    ));
                }
            }
        }
        if bindings.len() > 1 {
            return None;
        }
        let Some((_, exported, default)) = bindings.into_iter().next() else {
            return Some(name.to_owned());
        };
        let exported = if default { "default" } else { exported };
        Some(if tail.is_empty() {
            exported.to_owned()
        } else {
            format!("{exported}.{tail}")
        })
    };
    Some((
        raw.kind,
        normalize(&raw.dst_name)?,
        raw.import_source.clone(),
        match &raw.receiver {
            Some(receiver) => Some(normalize(receiver)?),
            None => None,
        },
    ))
}

fn import_binding<'a>(
    import: &'a kin_parser::FileImport,
    specifier: &'a kin_parser::ImportedName,
) -> (&'a str, &'a str, bool) {
    (
        &import.module_path,
        if specifier.is_default {
            "default"
        } else {
            specifier
                .original_name
                .as_deref()
                .unwrap_or(&specifier.local_name)
        },
        specifier.is_default,
    )
}

/// Whether a relation is one the parser owns. Its evidence then cites the
/// parser's own record of the occurrence, and the proof below compares that
/// record directly.
pub fn parser_owns_binding(relation: &Relation) -> bool {
    matches!(
        relation.origin,
        RelationOrigin::Parsed | RelationOrigin::Inferred
    )
}

/// `old_source` is the prior body `old` was parsed from. A language-server or
/// manual relation cites a name token rather than a site the parser records,
/// and the token's text is what places it inside a parser occurrence.
pub fn obligation_is_satisfied<G: GraphStore>(
    graph: &G,
    source_artifact: kin_model::ArtifactId,
    obligation: &LocalBindingObligation,
    old: &IndexedFile,
    old_source: &[u8],
    current: &IndexedFile,
    current_entities: &[Entity],
    produced: &[Relation],
    target: &mut impl FnMut(EntityId) -> std::result::Result<Option<Entity>, String>,
) -> std::result::Result<bool, String> {
    let held = &obligation.retired_relation;
    if !parser_owns_binding(held) {
        return annotated_occurrence_is_settled(
            graph,
            source_artifact,
            obligation,
            old,
            old_source,
            current,
            current_entities,
            produced,
            target,
        );
    }
    let source_file = &current.file_id;
    let prior_source_file = &old.file_id;
    if held.kind == RelationKind::Imports {
        let Some(module) = held.import_source.as_deref() else {
            return Ok(false);
        };
        let names: Vec<_> = held
            .evidence
            .iter()
            .filter_map(|e| e.token.as_deref())
            .collect();
        if names.is_empty() {
            return Ok(false);
        }
        // Matched per SPECIFIER first, then per statement. An entity-level
        // import edge cites the specifier's own bytes, so the span that
        // identifies the import this obligation was minted from is
        // `FileImport::evidence_site`. Comparing against `FileImport::site`
        // outright matched nothing once an edge started naming the line that
        // carries the name, and an obligation that matches nothing is never
        // discharged: the caller keeps binding debt forever.
        //
        // The statement fallback is the UPGRADE path, and it is not optional.
        // `held` comes out of the store, where an edge minted by an older
        // binary carries the statement's span, while `old` is re-parsed here
        // and now by the new one. Without the fallback every obligation a
        // pre-upgrade store already holds stops matching on the first
        // reconcile after the upgrade, on single-line imports as much as on
        // multi-line ones, and that debt never clears. Matching the statement
        // reproduces the older binary's own behaviour exactly: it admits every
        // named specifier under that statement, which is the set the older
        // binary would have admitted.
        let matches_held = |import: &kin_parser::FileImport,
                            specifier: &kin_parser::ImportedName| {
            held.evidence.iter().any(|e| {
                e.source_span.as_ref()
                    == Some(
                        &import
                            .evidence_site(specifier)
                            .to_source_span(prior_source_file),
                    )
                    || e.source_span.as_ref()
                        == Some(&import.site.to_source_span(prior_source_file))
            })
        };
        let prior: std::collections::BTreeSet<_> = old
            .imports
            .iter()
            .filter(|import| import.module_path == module)
            .flat_map(|import| {
                import
                    .specifiers
                    .iter()
                    .filter(|specifier| names.contains(&specifier.local_name.as_str()))
                    .filter(|specifier| matches_held(import, specifier))
                    .map(move |specifier| import_binding(import, specifier))
            })
            .collect();
        if prior.is_empty() {
            return Ok(false);
        }
        for binding in prior {
            let current_names: Vec<_> = current
                .imports
                .iter()
                .flat_map(|import| {
                    import
                        .specifiers
                        .iter()
                        .filter(move |specifier| import_binding(import, specifier) == binding)
                        .map(|specifier| specifier.local_name.as_str())
                })
                .collect();
            if current_names.is_empty() {
                continue;
            }
            let mut resolved = false;
            for relation in produced.iter().filter(|relation| {
                relation.kind == held.kind
                    && relation.import_source.as_deref() == Some(module)
                    && relation.evidence.iter().any(|e| {
                        e.token
                            .as_deref()
                            .is_some_and(|token| current_names.contains(&token))
                    })
                    && current_entities
                        .iter()
                        .any(|e| relation.src.as_entity() == Some(e.id))
            }) {
                if matches_target(
                    graph,
                    relation,
                    obligation,
                    source_artifact,
                    Some(module),
                    produced,
                    target,
                )? {
                    resolved = true;
                    break;
                }
            }
            if !resolved {
                return Ok(false);
            }
        }
        return Ok(true);
    }

    let Some(prior) = prior_occurrences(obligation, old) else {
        return Ok(false);
    };
    // An inferred target is a guess. A current, source-sealed proof of the
    // exact callee may correct that guess, including to an external symbol.
    // Stronger origins and unplaced/ambiguous answers keep their obligations.
    if held.origin == RelationOrigin::Inferred
        && held.kind == RelationKind::Calls
        && inferred_calls_are_refined(
            graph,
            obligation,
            old,
            old_source,
            current,
            current_entities,
            produced,
            &prior,
            ParseRequirement::Valid,
        )?
    {
        return Ok(true);
    }
    let Some(prior_keys) = prior
        .iter()
        .map(|raw| reference_identity(raw, old))
        .collect::<Option<Vec<_>>>()
    else {
        return Ok(false);
    };
    let mut matching = Vec::new();
    for raw in current
        .extracted_relations
        .iter()
        .filter(|raw| raw.kind == held.kind)
    {
        let Some(key) = reference_identity(raw, current) else {
            return Ok(false);
        };
        if prior_keys.contains(&key) {
            matching.push(raw);
        }
    }
    if matching.is_empty() {
        return Ok(!current
            .extracted_relations
            .iter()
            .any(kin_parser::is_call_extraction_incomplete_marker));
    }
    for raw in matching {
        let Some(site) = raw
            .site
            .as_ref()
            .map(|site| site.to_source_span(source_file))
        else {
            return Ok(false);
        };
        let sources: Vec<_> = current_entities
            .iter()
            .filter(|entity| entity.name == raw.src_name)
            .collect();
        let [source] = sources.as_slice() else {
            return Ok(false);
        };
        if kin_model::is_derived_member(source) {
            return Ok(false);
        }
        let mut resolved = false;
        for relation in produced.iter().filter(|relation| {
            relation.kind == held.kind
                && relation.src.as_entity() == Some(source.id)
                && relation
                    .evidence
                    .iter()
                    .any(|e| e.source_span.as_ref() == Some(&site))
        }) {
            if matches_target(
                graph,
                relation,
                obligation,
                source_artifact,
                import_module(raw, current).as_deref(),
                produced,
                target,
            )? {
                resolved = true;
                break;
            }
        }
        if !resolved {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The parser occurrences in the prior body that minted `obligation`'s
/// retired relation: one for every site its evidence cites, all by the
/// obligation's own source. `None` when a site has none, or none is cited.
fn prior_occurrences<'a>(
    obligation: &LocalBindingObligation,
    old: &'a IndexedFile,
) -> Option<Vec<&'a ExtractedRelation>> {
    let held = &obligation.retired_relation;
    let prior_source_file = &old.file_id;
    let sites: Vec<_> = held
        .evidence
        .iter()
        .filter_map(|e| e.source_span.as_ref())
        .collect();
    if sites.is_empty() {
        return None;
    }
    let prior: Vec<_> =
        old.extracted_relations
            .iter()
            .filter(|raw| {
                raw.kind == held.kind
                    && raw.src_name == obligation.source_name
                    && raw.site.as_ref().is_some_and(|site| {
                        sites.contains(&&site.to_source_span(prior_source_file))
                    })
            })
            .collect();
    let every_site_found = sites.iter().all(|site| {
        prior.iter().any(|raw| {
            raw.site
                .as_ref()
                .is_some_and(|raw_site| raw_site.to_source_span(prior_source_file) == **site)
        })
    });
    (!prior.is_empty() && every_site_found).then_some(prior)
}

/// Settle an inferred call guess in a source whose parse recovered from a
/// syntax error.
///
/// A recovered parse cannot support a proof by absence, since the region it
/// skipped could hold an occurrence it never recorded, so every other proof
/// in [`obligation_is_satisfied`] refuses it. This one argues from presence
/// alone: the body is byte-identical to the one the guess was made from, each
/// prior occurrence is re-found at the same bytes, its callee token is read
/// off a callee holding no error, and a current language-server proof at
/// that token refutes the guess. Only a name-only (`Inferred`) `Calls` guess
/// is settled this way.
#[allow(clippy::too_many_arguments)]
pub fn inferred_guess_is_refuted_under_recovered_parse<G: GraphStore>(
    graph: &G,
    obligation: &LocalBindingObligation,
    old: &IndexedFile,
    old_source: &[u8],
    current: &IndexedFile,
    current_entities: &[Entity],
    produced: &[Relation],
) -> std::result::Result<bool, String> {
    let held = &obligation.retired_relation;
    if held.origin != RelationOrigin::Inferred || held.kind != RelationKind::Calls {
        return Ok(false);
    }
    let Some(prior) = prior_occurrences(obligation, old) else {
        return Ok(false);
    };
    inferred_calls_are_refined(
        graph,
        obligation,
        old,
        old_source,
        current,
        current_entities,
        produced,
        &prior,
        ParseRequirement::Recovered,
    )
}

/// Project a parser-recorded call expression to its actual callee token.
/// The syntax tree, not containment in an argument list, owns this mapping.
/// Unsupported call forms remain unproved.
fn exact_callee_token<'t>(
    tree: &'t tree_sitter::Tree,
    site: &kin_model::SourceSpan,
) -> Option<tree_sitter::Node<'t>> {
    let call = tree
        .root_node()
        .named_descendant_for_byte_range(site.start_byte, site.end_byte)?;
    if call.start_byte() != site.start_byte
        || call.end_byte() != site.end_byte
        || !matches!(call.kind(), "call" | "call_expression")
    {
        return None;
    }
    let function = call.child_by_field_name("function")?;
    // A syntax error inside the arguments, such as an annotation the grammar
    // does not read, leaves the callee as it was parsed. One inside the
    // callee leaves nothing certain about which name is called.
    if function.has_error() {
        return None;
    }
    // A turbofish call names its callee inside the generic, and a path call
    // (`module::f(..)`, `Type::new(..)`) in the path's last segment: the same
    // token the language server's call-site ledger is keyed by.
    let function = match function.kind() {
        "generic_function" => function.child_by_field_name("function")?,
        _ => function,
    };
    let token = match function.kind() {
        "identifier" | "field_identifier" | "property_identifier" => function,
        "scoped_identifier" => function.child_by_field_name("name")?,
        "attribute" => function.child_by_field_name("attribute")?,
        "member_expression" => function.child_by_field_name("property")?,
        "field_expression" | "selector_expression" => function.child_by_field_name("field")?,
        _ => return None,
    };
    (matches!(
        token.kind(),
        "identifier" | "field_identifier" | "property_identifier"
    ) && !token.is_missing())
    .then_some(token)
}

/// How much of a file's parse a refinement proof may stand on.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ParseRequirement {
    /// Both parses must be complete.
    Valid,
    /// A recovered parse is enough: the proof re-finds each prior occurrence
    /// in the same bytes and never argues from the absence of one.
    Recovered,
}

#[allow(clippy::too_many_arguments)]
fn inferred_calls_are_refined<G: GraphStore>(
    graph: &G,
    obligation: &LocalBindingObligation,
    old: &IndexedFile,
    old_source: &[u8],
    current: &IndexedFile,
    current_entities: &[Entity],
    produced: &[Relation],
    prior: &[&ExtractedRelation],
    parse: ParseRequirement,
) -> Result<bool, String> {
    use kin_model::{CallSiteState, ContextValidationState, ResolutionRecord, ResolutionRecordId};
    let digest = obligation.source_digest;
    // A rederived weak guess must be retired by the resolver first. Otherwise
    // clearing its debt would also erase the scheduler's correction signal
    // while the contradictory inferred occurrence still lives in this view.
    let held = &obligation.retired_relation;
    if produced.iter().any(|relation| {
        relation.kind == RelationKind::Calls
            && relation.origin == RelationOrigin::Inferred
            && relation.src == held.src
            && relation.dst == held.dst
            && relation.evidence.iter().any(|evidence| {
                evidence.source_span.as_ref().is_some_and(|span| {
                    held.evidence
                        .iter()
                        .any(|old| old.source_span.as_ref() == Some(span))
                })
            })
    }) {
        return Ok(false);
    }
    if old.file_id != current.file_id
        || old.blob_hash.0 != digest.0
        || current.blob_hash.0 != digest.0
        || kin_blobs::digest(old_source).0 != digest.0
        || (parse == ParseRequirement::Valid
            && (!matches!(old.parse_state, kin_model::ParseState::Valid)
                || !matches!(current.parse_state, kin_model::ParseState::Valid)))
    {
        return Ok(false);
    }
    let registry = kin_parser::AdapterRegistry::default();
    let Some(adapter) = registry.get_by_language(current.language) else {
        return Ok(false);
    };
    let tree = adapter
        .parse(old_source)
        .map_err(|error| error.to_string())?;
    let source_owners = crate::RelationSourceIndex::new(current_entities);
    for raw in prior {
        let Some(site) = raw
            .site
            .as_ref()
            .map(|site| site.to_source_span(&current.file_id))
        else {
            return Ok(false);
        };
        // Source equality alone does not let a caller substitute a different
        // parser occurrence or an entity with a stale behavior fingerprint.
        if !current.extracted_relations.iter().any(|candidate| {
            candidate.kind == RelationKind::Calls
                && candidate.src_name == raw.src_name
                && candidate.dst_name == raw.dst_name
                && candidate.receiver == raw.receiver
                && candidate.site == raw.site
        }) {
            return Ok(false);
        }
        let Some(token) = exact_callee_token(&tree, &site) else {
            return Ok(false);
        };
        if token.utf8_text(old_source).ok() != raw.dst_name.rsplit(['.', ':']).next() {
            return Ok(false);
        }
        let Some(source) = source_owners.resolve(raw) else {
            return Ok(false);
        };
        let Some(span) = source.span.as_ref() else {
            return Ok(false);
        };
        if kin_model::is_derived_member(source)
            || source.language != current.language
            || source.file_origin.as_ref() != Some(&current.file_id)
            || span.file != current.file_id
            || !within(span, &site)
            || source
                .metadata
                .extra
                .get("blob_hash")
                .and_then(|v| v.as_str())
                != Some(digest.to_string().as_str())
            || !current.entities.iter().any(|parsed| {
                parsed.name == source.name
                    && parsed.span == source.span
                    && parsed.fingerprint.behavior_hash == source.fingerprint.behavior_hash
            })
        {
            return Ok(false);
        }
        let ledger_id = ResolutionRecordId::call_sites(source.id);
        let Some(record @ ResolutionRecord::CallSites(_)) = graph
            .lookup_resolution_record(&ledger_id)
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        if kin_model::validate_keyed_record(&ledger_id, &record).is_err() {
            return Ok(false);
        }
        let ResolutionRecord::CallSites(ledger) = record else {
            unreachable!()
        };
        if ledger.body_hash != digest || ledger.behavior_hash != source.fingerprint.behavior_hash {
            return Ok(false);
        }
        let validation_id = ResolutionRecordId::context_validation(source.language);
        let Some(validation_record @ ResolutionRecord::ContextValidation(_)) = graph
            .lookup_resolution_record(&validation_id)
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        if kin_model::validate_keyed_record(&validation_id, &validation_record).is_err() {
            return Ok(false);
        }
        let ResolutionRecord::ContextValidation(validation) = validation_record else {
            unreachable!()
        };
        let ContextValidationState::Validated { context } = validation.state else {
            return Ok(false);
        };
        if !context.resolver.starts_with("lsp:")
            || ResolutionRecordId::proof_context(&context) != ledger.context
        {
            return Ok(false);
        }
        let Some(context_record) = graph
            .lookup_resolution_record(&ledger.context)
            .map_err(|e| e.to_string())?
        else {
            return Ok(false);
        };
        if context_record != ResolutionRecord::ProofContext(context)
            || kin_model::validate_keyed_record(&ledger.context, &context_record).is_err()
            || ledger
                .validate_backing(span.start_byte, &current.file_id.0, produced)
                .is_err()
        {
            return Ok(false);
        }
        let Some((offset, length)) =
            kin_model::site_key(span.start_byte, token.start_byte(), token.end_byte())
        else {
            return Ok(false);
        };
        let Some(answer) = ledger.site(offset, length) else {
            return Ok(false);
        };
        let destination = match answer.state {
            // The resolver proved the call leaves the repository without
            // naming the symbol, which refutes the in-repository guess and
            // leaves no edge to require.
            CallSiteState::ProvenOutside => continue,
            // The resolver proved the call reaches a declaration inside the
            // caller itself, a nested function or the caller recursing, which
            // refutes a guess naming any other declaration. No edge carries a
            // proof onto its own caller, as `validate_backing` above accepts.
            CallSiteState::ProvenTarget { target }
                if target == source.id && held.dst != GraphNodeId::Entity(target) =>
            {
                continue
            }
            CallSiteState::ProvenTarget { target }
                if graph
                    .get_entity(&target)
                    .map_err(|e| e.to_string())?
                    .is_some() =>
            {
                GraphNodeId::Entity(target)
            }
            CallSiteState::ProvenExternal { target }
                if graph
                    .lookup_external_reference(&target)
                    .map_err(|e| e.to_string())?
                    .is_some_and(|reference| {
                        reference.id == target && reference.validate().is_ok()
                    }) =>
            {
                GraphNodeId::ExternalReference(target)
            }
            _ => return Ok(false),
        };
        // Every other proof names a node, and its language-server edge must
        // carry this exact callee token.
        let context_token = ledger.context.context_token();
        if !produced.iter().any(|edge| {
            edge.kind == RelationKind::Calls
                && edge.origin == RelationOrigin::Lsp
                && edge.src == GraphNodeId::Entity(source.id)
                && edge.dst == destination
                && edge.evidence.iter().any(|e| {
                    e.token.as_deref() == Some(context_token.as_str())
                        && e.source_span.as_ref().is_some_and(|proof| {
                            proof.file == current.file_id
                                && proof.start_byte == token.start_byte()
                                && proof.end_byte == token.end_byte()
                        })
                })
        }) {
            return Ok(false);
        }
    }
    Ok(!prior.is_empty())
}

/// Where a language-server or manual relation's cited token sits in the
/// parser's record of the prior body.
enum AnnotatedOccurrence<'a> {
    /// The imported binding a specifier token names, as `import_binding`
    /// identifies it.
    Binding((&'a str, &'a str, bool)),
    /// The module an import statement names, cited at its module token.
    Module(&'a str),
    /// An extracted relation whose site is the token, or a call whose callee
    /// the token names.
    Extracted(&'a ExtractedRelation),
    /// A parser-recorded call whose target remains unknown. This establishes
    /// syntax for retaining/removing debt, never a resolved relationship.
    IncompleteCall(&'a ExtractedRelation),
}

fn within(outer: &kin_model::SourceSpan, inner: &kin_model::SourceSpan) -> bool {
    outer.file == inner.file
        && outer.start_byte <= inner.start_byte
        && inner.end_byte <= outer.end_byte
}

/// The parser occurrences a cited token belongs to. Empty when the token is
/// not one the parser recorded, and then nothing can prove the obligation
/// either way.
fn annotated_occurrences<'a>(
    obligation: &LocalBindingObligation,
    old: &'a IndexedFile,
    old_source: &[u8],
    cited: &kin_model::SourceSpan,
) -> Vec<AnnotatedOccurrence<'a>> {
    let file = &old.file_id;
    let Some(token) = old_source
        .get(cited.start_byte..cited.end_byte)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    else {
        return vec![];
    };
    if cited.file != *file || token.is_empty() {
        return vec![];
    }
    let mut found = Vec::new();
    for import in &old.imports {
        if !within(&import.site.to_source_span(file), cited) {
            continue;
        }
        let named: Vec<_> = import
            .specifiers
            .iter()
            .filter(|specifier| match &specifier.site {
                Some(site) => within(&site.to_source_span(file), cited),
                None => {
                    specifier.local_name == token
                        || specifier.original_name.as_deref() == Some(token)
                }
            })
            .collect();
        let module = import
            .module_path
            .trim_matches(|c| matches!(c, '"' | '\'' | '`'));
        let unquoted = token.trim_matches(|c| matches!(c, '"' | '\'' | '`'));
        if !named.is_empty() {
            found.extend(
                named.into_iter().map(|specifier| {
                    AnnotatedOccurrence::Binding(import_binding(import, specifier))
                }),
            );
        } else if module == unquoted || module.rsplit(['.', '/']).next() == Some(unquoted) {
            found.push(AnnotatedOccurrence::Module(&import.module_path));
        }
    }
    for raw in &old.extracted_relations {
        if raw.src_name != obligation.source_name {
            continue;
        }
        let Some(site) = raw.site.as_ref().map(|site| site.to_source_span(file)) else {
            continue;
        };
        if kin_parser::is_scoped_call_extraction_incomplete_marker(raw)
            && raw
                .receiver
                .as_deref()
                .is_some_and(|callee| callee.rsplit(['.', ':']).next() == Some(token))
            && within(&site, cited)
            && old_source
                .get(site.start_byte..cited.start_byte)
                .is_some_and(|ahead| !ahead.contains(&b'('))
        {
            found.push(AnnotatedOccurrence::IncompleteCall(raw));
            continue;
        }
        // A call's site is its whole expression, arguments included, so the
        // token has to be the callee: the called name, ahead of the argument
        // list. A name read inside the arguments is not this call.
        let names_callee = || {
            raw.kind == RelationKind::Calls
                && within(&site, cited)
                && raw.dst_name.rsplit(['.', ':']).next() == Some(token)
                && old_source
                    .get(site.start_byte..cited.start_byte)
                    .is_some_and(|ahead| !ahead.contains(&b'('))
        };
        let same_bytes = within(&site, cited) && within(cited, &site);
        if same_bytes || names_callee() {
            found.push(AnnotatedOccurrence::Extracted(raw));
        }
    }
    found
}

/// Settles an obligation minted from a language-server or manual relation.
///
/// Its evidence cites a name token, and the proof runs through the parser
/// occurrence that token belongs to: an imported binding, the module an import
/// names, or an extracted relation. The obligation settles only when the
/// current body no longer has any such occurrence, by the same identities the
/// parser proof uses, or when a relation of the obligation's own kind at the
/// retained occurrence resolves to the same target, which is what an accepted
/// language-server pass supplies. A token the parser never recorded, or
/// evidence with no span, proves nothing, and the obligation stays.
#[allow(clippy::too_many_arguments)]
fn annotated_occurrence_is_settled<G: GraphStore>(
    graph: &G,
    source_artifact: kin_model::ArtifactId,
    obligation: &LocalBindingObligation,
    old: &IndexedFile,
    old_source: &[u8],
    current: &IndexedFile,
    current_entities: &[Entity],
    produced: &[Relation],
    target: &mut impl FnMut(EntityId) -> std::result::Result<Option<Entity>, String>,
) -> std::result::Result<bool, String> {
    let cited: Vec<_> = obligation
        .retired_relation
        .evidence
        .iter()
        .filter_map(|e| e.source_span.as_ref())
        .collect();
    if cited.is_empty() {
        return Ok(false);
    }
    let source_file = &current.file_id;
    for span in cited {
        let occurrences = annotated_occurrences(obligation, old, old_source, span);
        if occurrences.is_empty() {
            return Ok(false);
        }
        for occurrence in occurrences {
            let settled = match occurrence {
                AnnotatedOccurrence::Binding(binding) => {
                    let statements: Vec<_> = current
                        .imports
                        .iter()
                        .filter(|import| {
                            import
                                .specifiers
                                .iter()
                                .any(|specifier| import_binding(import, specifier) == binding)
                        })
                        .map(|import| import.site.to_source_span(source_file))
                        .collect();
                    statements.is_empty()
                        || re_resolved_at(
                            graph,
                            source_artifact,
                            obligation,
                            &obligation.source_name,
                            &statements,
                            Some(binding.0),
                            current_entities,
                            produced,
                            target,
                        )?
                }
                AnnotatedOccurrence::Module(module) => {
                    let statements: Vec<_> = current
                        .imports
                        .iter()
                        .filter(|import| import.module_path == module)
                        .map(|import| import.site.to_source_span(source_file))
                        .collect();
                    statements.is_empty()
                        || re_resolved_at(
                            graph,
                            source_artifact,
                            obligation,
                            &obligation.source_name,
                            &statements,
                            Some(module),
                            current_entities,
                            produced,
                            target,
                        )?
                }
                AnnotatedOccurrence::Extracted(raw) => {
                    let Some(key) = reference_identity(raw, old) else {
                        return Ok(false);
                    };
                    let mut matching = Vec::new();
                    for candidate in current
                        .extracted_relations
                        .iter()
                        .filter(|candidate| candidate.kind == raw.kind)
                    {
                        let Some(candidate_key) = reference_identity(candidate, current) else {
                            return Ok(false);
                        };
                        if candidate_key == key {
                            matching.push(candidate);
                        }
                    }
                    if matching.is_empty() {
                        !current
                            .extracted_relations
                            .iter()
                            .any(kin_parser::is_call_extraction_incomplete_marker)
                    } else {
                        let mut every = true;
                        for candidate in matching {
                            let Some(site) = candidate
                                .site
                                .as_ref()
                                .map(|site| site.to_source_span(source_file))
                            else {
                                return Ok(false);
                            };
                            if !re_resolved_at(
                                graph,
                                source_artifact,
                                obligation,
                                &candidate.src_name,
                                &[site],
                                import_module(candidate, current).as_deref(),
                                current_entities,
                                produced,
                                target,
                            )? {
                                every = false;
                                break;
                            }
                        }
                        every
                    }
                }
                AnnotatedOccurrence::IncompleteCall(raw) => {
                    let callee = raw.receiver.as_deref().expect("known scoped callee");
                    let matching: Vec<_> = current
                        .extracted_relations
                        .iter()
                        .filter(|candidate| {
                            (candidate.kind == RelationKind::Calls
                                && candidate.dst_name.rsplit(['.', ':']).next()
                                    == callee.rsplit(['.', ':']).next())
                                || (kin_parser::is_scoped_call_extraction_incomplete_marker(
                                    candidate,
                                ) && candidate.receiver.as_deref() == Some(callee))
                        })
                        .collect();
                    if matching.is_empty() {
                        !current
                            .extracted_relations
                            .iter()
                            .any(kin_parser::is_call_extraction_incomplete_marker)
                    } else {
                        let mut every = true;
                        for candidate in matching {
                            let Some(site) = candidate
                                .site
                                .as_ref()
                                .map(|site| site.to_source_span(source_file))
                            else {
                                return Ok(false);
                            };
                            // Negative syntax cannot settle itself. Only an
                            // accepted proof of the obligation's actual kind
                            // and target at this current site can do so.
                            if !re_resolved_at(
                                graph,
                                source_artifact,
                                obligation,
                                &candidate.src_name,
                                &[site],
                                import_module(candidate, current).as_deref(),
                                current_entities,
                                produced,
                                target,
                            )? {
                                every = false;
                                break;
                            }
                        }
                        every
                    }
                }
            };
            if !settled {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// Whether a relation of the obligation's own kind, from the retained
/// occurrence's source and cited inside one of `regions`, resolves to the
/// obligation's target again.
#[allow(clippy::too_many_arguments)]
fn re_resolved_at<G: GraphStore>(
    graph: &G,
    source_artifact: kin_model::ArtifactId,
    obligation: &LocalBindingObligation,
    source_name: &str,
    regions: &[kin_model::SourceSpan],
    module: Option<&str>,
    current_entities: &[Entity],
    produced: &[Relation],
    target: &mut impl FnMut(EntityId) -> std::result::Result<Option<Entity>, String>,
) -> std::result::Result<bool, String> {
    let sources: Vec<_> = current_entities
        .iter()
        .filter(|entity| entity.name == source_name)
        .collect();
    let [source] = sources.as_slice() else {
        return Ok(false);
    };
    if kin_model::is_derived_member(source) {
        return Ok(false);
    }
    for relation in produced.iter().filter(|relation| {
        relation.kind == obligation.retired_relation.kind
            && relation.src.as_entity() == Some(source.id)
            && relation.evidence.iter().any(|e| {
                e.source_span
                    .as_ref()
                    .is_some_and(|span| regions.iter().any(|region| within(region, span)))
            })
    }) {
        if matches_target(
            graph,
            relation,
            obligation,
            source_artifact,
            module,
            produced,
            target,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A parser-recorded module wins. Otherwise only one exact imported local
/// binding may identify the module; an ambiguous spelling supplies no proof.
fn import_module(raw: &ExtractedRelation, file: &IndexedFile) -> Option<String> {
    if let Some(module) = raw.import_source.as_ref() {
        return file
            .imports
            .iter()
            .any(|i| &i.module_path == module)
            .then(|| module.clone());
    }
    let roots: Vec<_> = std::iter::once(raw.dst_name.as_str())
        .chain(raw.receiver.as_deref())
        .map(|name| name.split('.').next().unwrap_or(name))
        .collect();
    let modules: std::collections::BTreeSet<_> = file
        .imports
        .iter()
        .filter(|i| {
            i.specifiers
                .iter()
                .any(|s| roots.contains(&s.local_name.as_str()))
        })
        .map(|i| i.module_path.as_str())
        .collect();
    (modules.len() == 1).then(|| modules.into_iter().next().unwrap().to_owned())
}

fn matches_target<G: GraphStore>(
    graph: &G,
    relation: &Relation,
    obligation: &LocalBindingObligation,
    source_artifact: kin_model::ArtifactId,
    current_module: Option<&str>,
    produced: &[Relation],
    target: &mut impl FnMut(EntityId) -> std::result::Result<Option<Entity>, String>,
) -> std::result::Result<bool, String> {
    if !RelationResolution::of(relation).is_proven() {
        return Ok(false);
    }
    let Some(id) = relation.dst.as_entity() else {
        return Ok(false);
    };
    let Some(entity) = target(id)? else {
        return Ok(false);
    };
    let Some(file) = entity.file_origin.as_ref() else {
        return Ok(false);
    };
    if entity.name != obligation.target_name || kin_model::is_derived_member(&entity) {
        return Ok(false);
    }
    if obligation.prior_source_file.is_none() {
        return Ok(file == &obligation.target_file);
    }
    // A relocated relative import may now reach a different admitted artifact.
    // The old location stays immutable provenance; current module authority is
    // a separate exact fact, never a same-name target shortcut.
    let Some(module) = current_module else {
        return Ok(false);
    };
    let path = kin_model::RepoPath::from_utf8(file.0.clone()).map_err(|error| error.to_string())?;
    let Some(artifact) = graph.artifact_id_at_path(&path) else {
        return Ok(false);
    };
    Ok(produced.iter().any(|import| {
        import.src == GraphNodeId::Artifact(source_artifact)
            && import.dst == GraphNodeId::Artifact(artifact)
            && matches!(import.kind, RelationKind::Imports | RelationKind::Includes)
            && import.origin == kin_model::RelationOrigin::Parsed
            && import.confidence.to_bits() == 1.0_f32.to_bits()
            && RelationResolution::of(import).is_proven()
            && import.import_source.as_deref() == Some(module)
            && import.evidence.iter().any(|e| {
                matches!(
                    e.parser_rule.as_deref(),
                    Some("import_declaration" | "include_directive")
                ) && e.source_path.as_deref() == Some(module)
                    && e.resolved_path.as_deref() == Some(file.0.as_str())
            })
    }))
}

#[cfg(test)]
mod callee_token_tests {
    use super::exact_callee_token;

    /// Each call expression's callee token in `source`, read the way the
    /// refinement proof reads it.
    fn callee_tokens(language: kin_model::LanguageId, source: &str) -> Vec<Option<String>> {
        let registry = kin_parser::AdapterRegistry::default();
        let tree = registry
            .get_by_language(language)
            .unwrap()
            .parse(source.as_bytes())
            .unwrap();
        let file = kin_model::FilePathId::new("src/lib.rs");
        let mut calls = Vec::new();
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "call_expression" {
                calls.push(node);
            }
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        calls.sort_by_key(|node| node.start_byte());
        calls
            .into_iter()
            .map(|call| {
                let span = kin_parser::adapter::span_from_node(&call, &file);
                exact_callee_token(&tree, &span)
                    .map(|token| token.utf8_text(source.as_bytes()).unwrap().to_string())
            })
            .collect()
    }

    #[test]
    fn an_error_in_the_arguments_leaves_the_callee_token() {
        // The JavaScript grammar does not read these Flow annotations, and
        // its recovery puts an error inside the call's arguments while the
        // callee parses cleanly.
        let registry = kin_parser::AdapterRegistry::default();
        let adapter = registry
            .get_by_language(kin_model::LanguageId::JavaScript)
            .unwrap();
        let file = kin_model::FilePathId::new("src/handle.js");
        for source in [
            "setTimeout(error: any, 0);\n",
            "setTimeout(() => { throw error; }: any);\n",
        ] {
            let tree = adapter.parse(source.as_bytes()).unwrap();
            let end = source.rfind(')').unwrap() + 1;
            let call = tree
                .root_node()
                .named_descendant_for_byte_range(0, end)
                .unwrap();
            assert_eq!(
                call.kind(),
                "call_expression",
                "{}",
                tree.root_node().to_sexp()
            );
            assert!(call.has_error(), "{source}: {}", tree.root_node().to_sexp());
            assert!(
                !call.child_by_field_name("function").unwrap().has_error(),
                "{}",
                tree.root_node().to_sexp()
            );
            let span = kin_parser::adapter::span_from_node(&call, &file);
            assert_eq!(
                super::exact_callee_token(&tree, &span)
                    .map(|token| token.utf8_text(source.as_bytes()).unwrap()),
                Some("setTimeout"),
                "{source}"
            );
        }
    }

    #[test]
    fn rust_path_and_turbofish_calls_name_their_last_segment() {
        let source = "fn run() {\n    crate::util::helper(1);\n    Vec::<u8>::new();\n    parse::<u8>(\"2\");\n    local(3);\n    value.method(4);\n}\n";
        assert_eq!(
            callee_tokens(kin_model::LanguageId::Rust, source),
            vec![
                Some("helper".to_string()),
                Some("new".to_string()),
                Some("parse".to_string()),
                Some("local".to_string()),
                Some("method".to_string()),
            ]
        );
    }
}
