// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! A type an import binds to a package outside the repository never resolves
//! to a repository class that happens to share its simple name.
//!
//! Each fixture holds a repository class and an external type of one simple
//! name. A receiver declared as the external type binds to nothing, while a
//! receiver declared as the repository class, imported from its own package,
//! still binds to it.

use std::collections::HashMap;

use kin_index::resolution::RECEIVER_ORIGIN_CONFIDENCE;
use kin_index::{link_cross_file, link_cross_file_incremental};
use kin_index::{FileParseData, IncrementalLinker, IndexPipeline};
use kin_model::{ArtifactId, EntityId, FilePathId, Relation, RelationKind};

fn fixture(path: &str, source: &str) -> FileParseData {
    let indexed = IndexPipeline::new()
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

fn links(files: &[FileParseData]) -> [(&'static str, Vec<Relation>); 2] {
    let ids: HashMap<String, ArtifactId> = files
        .iter()
        .map(|file| (file.file_path.clone(), ArtifactId::new()))
        .collect();
    let mut linker = IncrementalLinker::new();
    for file in files {
        linker.add_file(&file.file_path, ids[&file.file_path], &file.entities);
    }
    [
        ("batch", link_cross_file(files, &ids).unwrap()),
        (
            "incremental",
            link_cross_file_incremental(files, &linker).unwrap(),
        ),
    ]
}

/// Every call edge from the entity named `caller`, as (target name,
/// confidence).
fn calls_from(relations: &[Relation], files: &[FileParseData], caller: &str) -> Vec<(String, f32)> {
    let names: HashMap<EntityId, &str> = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .map(|entity| (entity.id, entity.name.as_str()))
        .collect();
    let caller = files
        .iter()
        .flat_map(|file| file.entities.iter())
        .find(|entity| entity.name == caller)
        .unwrap_or_else(|| panic!("no entity {caller}"))
        .id;
    relations
        .iter()
        .filter(|relation| {
            relation.kind == RelationKind::Calls && relation.src.as_entity() == Some(caller)
        })
        .filter_map(|relation| {
            Some((
                names.get(&relation.dst.as_entity()?)?.to_string(),
                relation.confidence,
            ))
        })
        .collect()
}

#[test]
fn a_java_field_of_an_imported_external_logger_binds_no_repository_logger() {
    let files = vec![
        fixture(
            "src/main/java/app/util/Logger.java",
            "package app.util;\n\npublic class Logger {\n    public void info(String message) {}\n}\n",
        ),
        fixture(
            "src/main/java/app/audit/Audit.java",
            "package app.audit;\n\npublic class Audit {\n    public void info(String message) {}\n}\n",
        ),
        fixture(
            "src/main/java/app/Service.java",
            "package app;\n\nimport org.slf4j.Logger;\n\npublic class Service {\n    \
             private Logger log;\n\n    public void run() {\n        log.info(\"x\");\n    }\n}\n",
        ),
        fixture(
            "src/main/java/app/Worker.java",
            "package app;\n\nimport app.util.Logger;\n\npublic class Worker {\n    \
             private Logger log;\n\n    public void run() {\n        log.info(\"x\");\n    }\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let service = calls_from(&relations, &files, "Service.run");
        assert!(
            !service.iter().any(|(name, _)| name == "Logger.info"),
            "{label}: org.slf4j.Logger is not app.util.Logger: {service:?}"
        );
        let worker = calls_from(&relations, &files, "Worker.run");
        assert!(
            worker.iter().any(|(name, confidence)| name == "Logger.info"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: the repository's own Logger, imported from its package, still binds: {worker:?}"
        );
    }
}

#[test]
fn a_python_annotation_of_an_imported_external_class_binds_no_repository_class() {
    let files = vec![
        fixture(
            "src/app/wrappers.py",
            "class Response:\n    def close(self) -> None:\n        pass\n",
        ),
        fixture(
            "src/app/other.py",
            "class Stream:\n    def close(self) -> None:\n        pass\n",
        ),
        fixture(
            "src/app/views.py",
            "from werkzeug.wrappers import Response\n\n\
             def finish(response: Response) -> None:\n    response.close()\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let finish = calls_from(&relations, &files, "finish");
        assert!(
            !finish.iter().any(|(name, _)| name == "Response.close"),
            "{label}: werkzeug's Response is not the repository's: {finish:?}"
        );
    }
}

#[test]
fn a_php_type_a_vendor_namespace_imports_binds_no_repository_class() {
    let files = vec![
        fixture(
            "app/Support/Collection.php",
            "<?php\nnamespace App\\Support;\n\nclass Collection {\n    public function first() { return null; }\n}\n",
        ),
        fixture(
            "app/Models/Registry.php",
            "<?php\nnamespace App\\Models;\n\nclass Registry {\n    public function first() { return null; }\n}\n",
        ),
        fixture(
            "app/Services/Checkout.php",
            "<?php\nnamespace App\\Services;\n\nuse Illuminate\\Support\\Collection;\nuse App\\Models\\Registry;\n\n\
             class Checkout {\n    public function vendor(Collection $items) { return $items->first(); }\n    \
             public function own(Registry $registry) { return $registry->first(); }\n}\n",
        ),
    ];
    for (label, relations) in links(&files) {
        let vendor = calls_from(&relations, &files, "Checkout.vendor");
        assert!(
            !vendor.iter().any(|(name, _)| name == "Collection.first"),
            "{label}: Illuminate's Collection is not the repository's: {vendor:?}"
        );
        let own = calls_from(&relations, &files, "Checkout.own");
        assert!(
            own.iter().any(|(name, confidence)| name == "Registry.first"
                && *confidence == RECEIVER_ORIGIN_CONFIDENCE),
            "{label}: App\\Models\\Registry is the repository's app/Models/Registry: {own:?}"
        );
    }
}
