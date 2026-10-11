// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A receiver read out of a framework context, typed through the context's
//! creation and a default import, batch and incremental: the edge it binds,
//! the steps that edge carries, and the missing step a chain that stops
//! short records instead.

use super::*;
use crate::coverage_certificate::{CertificateDetail, ReceiverOriginGapEntry};
use crate::receiver_origin::{receiver_origin_steps, OriginStep, OriginStepKind};
use crate::resolution::{RelationResolution, RECEIVER_ORIGIN_CONFIDENCE};

fn fixture(path: &str, source: &str) -> FileParseData {
    let indexed = crate::IndexPipeline::new()
        .index_file_content_with_tests(
            &FilePathId::new(path),
            source.as_bytes(),
            kin_blobs::digest(source.as_bytes()),
        )
        .unwrap()
        .indexed_file;
    FileParseData {
        file_path: path.into(),
        entities: indexed.entities,
        relations: indexed.extracted_relations,
        imports: indexed.imports,
    }
}

fn completeness(files: &[FileParseData]) -> FileParseCompletenessMap {
    files
        .iter()
        .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
        .collect()
}

struct Linked {
    label: &'static str,
    ids: ArtifactIdentityMap,
    relations: Vec<Relation>,
}

fn both(files: &[FileParseData]) -> [Linked; 2] {
    let ids: ArtifactIdentityMap = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let batch = link_cross_file_with_completeness(files, &ids, &completeness(files)).unwrap();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    let incremental =
        link_cross_file_incremental_with_completeness(files, &linker, &completeness(files))
            .unwrap();
    [
        Linked {
            label: "batch",
            ids: ids.clone(),
            relations: batch,
        },
        Linked {
            label: "incremental",
            ids,
            relations: incremental,
        },
    ]
}

fn entity<'a>(files: &'a [FileParseData], file: &str, kind: EntityKind, name: &str) -> &'a Entity {
    files
        .iter()
        .find(|parsed| parsed.file_path == file)
        .and_then(|parsed| {
            parsed
                .entities
                .iter()
                .find(|entity| entity.kind == kind && entity.name == name)
        })
        .unwrap_or_else(|| panic!("no {kind:?} {name} in {file}"))
}

/// Every call edge from `caller`, as (target, confidence, steps).
fn calls_from(linked: &Linked, caller: &Entity) -> Vec<(EntityId, f32, Option<Vec<OriginStep>>)> {
    linked
        .relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src == GraphNodeId::Entity(caller.id)
        })
        .filter_map(|relation| {
            Some((
                relation.dst.as_entity()?,
                relation.confidence,
                receiver_origin_steps(relation),
            ))
        })
        .collect()
}

/// The certificate's receiver-origin gap entries for `file`.
fn gaps(linked: &Linked, file: &str) -> Vec<ReceiverOriginGapEntry> {
    let artifact = GraphNodeId::Artifact(linked.ids[file]);
    linked
        .relations
        .iter()
        .filter(|relation| relation.src == artifact && relation.dst == artifact)
        .find_map(CertificateDetail::from_certificate)
        .unwrap_or_else(|| panic!("{}: no certificate for {file}", linked.label))
        .receiver_origin_gaps
}

const STORE: &str = "\
export type Config = {name: string};

export default class Store {
  containsElement(id: number): boolean {
    return id > 0;
  }
}
";

/// A second class of the name, holding a method of the name, so neither the
/// class nor the method is unique in the repository.
const OTHER_STORE: &str = "\
export class Store {
  containsElement(id: number): boolean {
    return false;
  }
}
";

/// A type of the name too, as React's DevTools declare one.
const FUSEBOX: &str = "export type Store = Object;\n";

const CONTEXT: &str = "\
import {createContext} from 'react';
import Store from '../store';

export const StoreContext = createContext<Store>(null as any as Store);
export const LooseContext = createContext(null);
";

const PANEL: &str = "\
import {useContext} from 'react';
import {StoreContext, LooseContext} from './context';

export function Panel({id}: {id: number}) {
  const store = useContext(StoreContext);
  return store.containsElement(id);
}

export function Loose({id}: {id: number}) {
  const store = useContext(LooseContext);
  return store.containsElement(id);
}
";

fn context_files() -> Vec<FileParseData> {
    vec![
        fixture("src/devtools/store.ts", STORE),
        fixture("src/other/Store.ts", OTHER_STORE),
        fixture("src/fusebox/frontend.d.ts", FUSEBOX),
        fixture("src/devtools/views/context.ts", CONTEXT),
        fixture("src/devtools/views/Panel.tsx", PANEL),
    ]
}

#[test]
fn a_use_context_receiver_binds_through_create_context_and_a_default_import() {
    let files = context_files();
    let panel = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Panel",
    );
    let store = entity(&files, "src/devtools/store.ts", EntityKind::Class, "Store");
    let contains = entity(
        &files,
        "src/devtools/store.ts",
        EntityKind::Method,
        "Store.containsElement",
    );
    let decoy = entity(
        &files,
        "src/other/Store.ts",
        EntityKind::Method,
        "Store.containsElement",
    );
    let context = entity(
        &files,
        "src/devtools/views/context.ts",
        EntityKind::Constant,
        "StoreContext",
    );
    for linked in both(&files) {
        let label = linked.label;
        let calls = calls_from(&linked, panel);
        let (_, confidence, steps) = calls
            .iter()
            .find(|(target, _, _)| *target == contains.id)
            .unwrap_or_else(|| panic!("{label}: no edge to the default-exported Store"));
        assert_eq!(*confidence, RECEIVER_ORIGIN_CONFIDENCE, "{label}");
        assert_eq!(
            RelationResolution::from_confidence(*confidence),
            RelationResolution::TypeResolved,
            "{label}"
        );
        assert!(
            !calls.iter().any(|(target, _, _)| *target == decoy.id),
            "{label}: the other Store gains no caller: {calls:?}"
        );
        let steps = steps.as_ref().expect("the edge carries its steps");
        let shape: Vec<(OriginStepKind, &str, Option<EntityId>, Option<&str>)> = steps
            .iter()
            .map(|step| {
                (
                    step.step,
                    step.name.as_str(),
                    step.entity,
                    step.file.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (
                    OriginStepKind::Binding,
                    "store",
                    Some(panel.id),
                    Some("src/devtools/views/Panel.tsx")
                ),
                (
                    OriginStepKind::Provider,
                    "StoreContext",
                    Some(context.id),
                    Some("src/devtools/views/context.ts")
                ),
                (
                    OriginStepKind::Type,
                    "Store",
                    Some(store.id),
                    Some("src/devtools/store.ts")
                ),
                (
                    OriginStepKind::Method,
                    "Store.containsElement",
                    Some(contains.id),
                    Some("src/devtools/store.ts")
                ),
            ],
            "{label}"
        );
        assert_eq!(steps[1].kind.as_deref(), Some("react_context"), "{label}");
        assert_eq!(steps[2].via.as_deref(), Some("declared"), "{label}");
        // A bound call is no gap.
        assert!(
            gaps(&linked, "src/devtools/views/Panel.tsx")
                .iter()
                .all(|gap| gap.steps.first().and_then(|step| step.entity) != Some(panel.id)),
            "{label}"
        );
    }
}

#[test]
fn a_context_created_without_a_type_stays_unbound_and_names_the_missing_step() {
    let files = context_files();
    let loose = entity(
        &files,
        "src/devtools/views/Panel.tsx",
        EntityKind::Function,
        "Loose",
    );
    for linked in both(&files) {
        let label = linked.label;
        let calls = calls_from(&linked, loose);
        assert!(
            calls.iter().all(
                |(_, confidence, steps)| *confidence < RECEIVER_ORIGIN_CONFIDENCE
                    && steps.is_none()
            ),
            "{label}: an untyped context proves nothing: {calls:?}"
        );
        let gaps = gaps(&linked, "src/devtools/views/Panel.tsx");
        let gap = gaps
            .iter()
            .find(|gap| gap.steps.first().and_then(|step| step.entity) == Some(loose.id))
            .unwrap_or_else(|| panic!("{label}: no gap recorded for Loose: {gaps:?}"));
        assert_eq!(gap.receiver, "store", "{label}");
        assert_eq!(gap.method, "containsElement", "{label}");
        assert_eq!(
            gap.steps.len(),
            1,
            "{label}: only the binding is established"
        );
        assert_eq!(gap.steps[0].step, OriginStepKind::Binding, "{label}");
        assert_eq!(gap.missing.step, OriginStepKind::Provider, "{label}");
        assert_eq!(gap.missing.name, "LooseContext", "{label}");
        assert_eq!(
            gap.missing.kind.as_deref(),
            Some("react_context"),
            "{label}"
        );
        assert_eq!(gap.missing.reason, "provider_value_unknown", "{label}");
        assert_eq!(
            &PANEL[gap.span.start_byte..gap.span.end_byte],
            "store.containsElement(id)",
            "{label}: the gap is addressed at its call"
        );
    }
}

#[test]
fn a_default_import_means_what_the_default_export_names_not_the_local_name() {
    // `factory.ts` declares a class `Store`, but its default export is a
    // function. An import that calls its default `Store` names the function,
    // so no class is proven and the chain stops at the type.
    let mut files = context_files();
    files.push(fixture(
        "src/devtools/factory.ts",
        "export class Store {\n  containsElement(id: number): boolean {\n    return true;\n  }\n}\n\nexport default function makeStore() {\n  return null;\n}\n",
    ));
    files.push(fixture(
        "src/devtools/views/factoryContext.ts",
        "import {createContext} from 'react';\nimport Store from '../factory';\n\nexport const FactoryContext = createContext<Store>(null as any as Store);\n",
    ));
    files.push(fixture(
        "src/devtools/views/Factory.tsx",
        "import {useContext} from 'react';\nimport {FactoryContext} from './factoryContext';\n\nexport function Factory({id}: {id: number}) {\n  const store = useContext(FactoryContext);\n  return store.containsElement(id);\n}\n",
    ));
    let factory = entity(
        &files,
        "src/devtools/views/Factory.tsx",
        EntityKind::Function,
        "Factory",
    );
    for linked in both(&files) {
        let label = linked.label;
        let calls = calls_from(&linked, factory);
        assert!(
            calls
                .iter()
                .all(|(_, confidence, _)| *confidence < RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: {calls:?}"
        );
        let gaps = gaps(&linked, "src/devtools/views/Factory.tsx");
        let [gap] = gaps.as_slice() else {
            panic!("{label}: one gap expected: {gaps:?}");
        };
        let established: Vec<OriginStepKind> = gap.steps.iter().map(|step| step.step).collect();
        assert_eq!(
            established,
            vec![OriginStepKind::Binding, OriginStepKind::Provider],
            "{label}"
        );
        assert_eq!(gap.missing.step, OriginStepKind::Type, "{label}");
        assert_eq!(gap.missing.name, "Store", "{label}");
        assert_eq!(gap.missing.reason, "type_not_located", "{label}");
    }
}
