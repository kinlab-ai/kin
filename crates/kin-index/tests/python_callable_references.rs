// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Passing a bound method is a value reference, distinct from invocation.

use kin_index::resolution::{RelationResolution, DISPATCH_CANDIDATE_CONFIDENCE};
use kin_index::FileParseData;
use kin_model::{ArtifactId, Entity, EntityId, FilePathId, Relation, RelationKind};
use kin_parser::{LanguageAdapter, PythonAdapter};

fn parse(path: &str, source: &str) -> FileParseData {
    let adapter = PythonAdapter;
    let file = FilePathId::new(path);
    let bytes = source.as_bytes();
    let tree = adapter.parse(bytes).unwrap();
    let output = adapter.extract(&tree, bytes, &file).unwrap();
    FileParseData {
        file_path: path.into(),
        entities: output
            .entities
            .into_iter()
            .map(|entity| entity.into_entity_with_source(adapter.language_id(), &file, Some(bytes)))
            .collect::<Vec<Entity>>(),
        relations: output.relations,
        imports: output.imports,
    }
}

fn id(files: &[FileParseData], name: &str) -> EntityId {
    files
        .iter()
        .flat_map(|file| &file.entities)
        .find(|entity| entity.name == name)
        .unwrap()
        .id
}

fn batch(files: &[FileParseData]) -> Vec<Relation> {
    let artifacts = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    kin_index::link_cross_file(files, &artifacts).unwrap()
}

fn edge(
    relations: &[Relation],
    kind: RelationKind,
    source: EntityId,
    target: EntityId,
) -> Option<&Relation> {
    relations.iter().find(|relation| {
        relation.kind == kind
            && relation.src.as_entity() == Some(source)
            && relation.dst.as_entity() == Some(target)
    })
}

const CLIENT: &str = "from consumer import Consumer\n\nclass Client:\n    def _process_event(self, event):\n        return event\n    def _new_consumer(self):\n        return Consumer(process_event=self._process_event)\n    def immediate(self, event):\n        return self._process_event(event)\n";
const CONSUMER: &str = "class Consumer:\n    def __init__(self, process_event):\n        self.process_event = process_event\n    async def process(self, event):\n        return await self.process_event(event)\n";

#[test]
fn callback_registration_is_an_exact_reference_and_not_a_call() {
    let files = vec![parse("client.py", CLIENT), parse("consumer.py", CONSUMER)];
    let registration = id(&files, "Client._new_consumer");
    let callback = id(&files, "Client._process_event");
    let relations = batch(&files);
    let reference = edge(&relations, RelationKind::References, registration, callback)
        .expect("passing self._process_event must retain the bound-method reference");
    let source = reference
        .evidence
        .iter()
        .find_map(|record| record.source_span.as_ref())
        .unwrap();
    assert_eq!(
        source.start_line, 6,
        "reference must identify its actual registration line"
    );
    assert_eq!(
        RelationResolution::of(reference),
        RelationResolution::TypeResolved
    );
    assert!(edge(&relations, RelationKind::Calls, registration, callback).is_none());
    assert!(edge(
        &relations,
        RelationKind::Calls,
        id(&files, "Client.immediate"),
        callback
    )
    .is_some());
    assert!(
        edge(
            &relations,
            RelationKind::Calls,
            id(&files, "Consumer.process"),
            callback
        )
        .is_none(),
        "an arbitrary injected callback must not become globally bound to this client"
    );
}

#[test]
fn a_self_reference_to_an_overridden_method_retains_dispatch_uncertainty() {
    let files = vec![parse("client.py", CLIENT), parse("consumer.py", CONSUMER), parse("child.py",
        "from client import Client\nclass Child(Client):\n    def _process_event(self, event):\n        return None\n")];
    let relations = batch(&files);
    let reference = edge(
        &relations,
        RelationKind::References,
        id(&files, "Client._new_consumer"),
        id(&files, "Client._process_event"),
    )
    .expect("reference to the declared base member");
    assert_eq!(reference.confidence, DISPATCH_CANDIDATE_CONFIDENCE);
    assert_ne!(
        RelationResolution::of(reference),
        RelationResolution::TypeResolved
    );
    assert!(kin_index::linker::is_self_dispatch_candidate(reference));
    assert!(
        edge(
            &relations,
            RelationKind::References,
            id(&files, "Client._new_consumer"),
            id(&files, "Child._process_event")
        )
        .is_none(),
        "do not choose a runtime subclass"
    );
}

#[test]
fn an_inherited_callback_reference_reaches_its_defining_method() {
    let files = vec![parse("base.py", "class Base:\n    def process(self, event):\n        return event\n"),
        parse("child.py", "from base import Base\nclass Child(Base):\n    def register(self):\n        return self.process\n")];
    let relations = batch(&files);
    assert!(edge(
        &relations,
        RelationKind::References,
        id(&files, "Child.register"),
        id(&files, "Base.process")
    )
    .is_some());
}

#[test]
fn a_rebound_or_static_self_cannot_name_the_enclosing_class_method() {
    for declaration in [
        "    def register(self, other):\n        self = other\n        return self.process\n",
        "    @staticmethod\n    def register(self):\n        return self.process\n",
        "    @decorate\n    def register(self):\n        return self.process\n",
    ] {
        let source = format!("class Owner:\n    def process(self):\n        pass\n{declaration}");
        let files = vec![parse("owner.py", &source)];
        assert!(edge(
            &batch(&files),
            RelationKind::References,
            id(&files, "Owner.register"),
            id(&files, "Owner.process")
        )
        .is_none());
    }
}

#[test]
fn a_callee_member_is_not_duplicated_as_a_value_reference() {
    let files = vec![parse("client.py", CLIENT)];
    assert!(edge(
        &batch(&files),
        RelationKind::References,
        id(&files, "Client.immediate"),
        id(&files, "Client._process_event")
    )
    .is_none());
}

#[test]
fn instance_data_and_unknown_members_reference_no_method() {
    let source = "class Owner:\n    limit = 3\n    def __init__(self, hook):\n        self.hook = hook\n    def hook(self):\n        pass\n    def run(self):\n        return (self.limit, self.hook, self.missing)\n";
    let files = vec![parse("owner.py", source)];
    let relations = batch(&files);
    let run = id(&files, "Owner.run");
    assert!(
        relations.iter().all(|relation| relation.kind != RelationKind::References
            || relation.src.as_entity() != Some(run)),
        "an instance attribute shadows the method, and data or a missing member is no method: {relations:#?}"
    );
}

#[test]
fn a_member_value_reference_carries_its_exact_site() {
    let files = vec![parse("client.py", CLIENT), parse("consumer.py", CONSUMER)];
    let relations = batch(&files);
    let reference = edge(
        &relations,
        RelationKind::References,
        id(&files, "Client._new_consumer"),
        id(&files, "Client._process_event"),
    )
    .unwrap();
    let span = reference
        .evidence
        .iter()
        .find_map(|record| record.source_span.as_ref())
        .unwrap();
    assert_eq!(
        &CLIENT[span.start_byte..span.end_byte],
        "self._process_event"
    );
    assert!(reference
        .evidence
        .iter()
        .any(|record| record.parser_rule.as_deref()
            == Some(kin_index::linker::SELF_MEMBER_VALUE_RULE)));
}
