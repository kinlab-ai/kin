// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Member reads that hand a value on without calling it, read off a value or
//! a module of a library, over source the pipeline parsed and linked.
//!
//! A census asks whether anything holds a focal as a value, because a call
//! through a variable can reach a function only that way. One read that
//! merely spells the focal's name kept the answer "yes", and with it every
//! call through a variable in the repository stayed a candidate caller. The
//! two reads below are Flask's own, and each provably reads something else:
//!
//! - `partial(runner.invoke, cli)` in `tests/test_cli.py`, where `runner` is
//!   click's `CliRunner`, asked about `FlaskCliRunner.invoke`;
//! - `tomllib.load` in `tests/test_config.py`, where `tomllib` is what
//!   `pytest.importorskip("tomllib")` returns, asked about `flask.json.load`.

use std::collections::HashMap;

use kin_db::call_site_escape::OutsideMemberRead;
use kin_db::{focal_escape_evidence_in, CensusFile, EscapeCensusGraph};
use kin_index::coverage_certificate::CertificateDetail;
use kin_index::{FileParseData, IndexPipeline};
use kin_model::{
    focal_call_names, ArtifactId, Entity, EntityId, FilePathId, FocalEscape, GraphNodeId, Hash256,
    LanguageId, OutsideReceiver, OutsideValueRead, ParseCompleteness, RelationEvidence,
};

/// `src/flask/testing.py`: the runner Flask derives from click's.
const TESTING: &str = r#"from click.testing import CliRunner


class FlaskCliRunner(CliRunner):
    def invoke(self, cli=None, args=None, **kwargs):
        return super().invoke(cli, args, **kwargs)
"#;

/// `tests/test_cli.py`: the `invoke` fixture binds click's own runner.
const TEST_CLI: &str = r#"from functools import partial

import pytest
from click.testing import CliRunner


@pytest.fixture
def runner():
    return CliRunner()


class TestRoutes:
    @pytest.fixture
    def invoke(self, runner):
        return partial(runner.invoke, None)

    def test_simple(self, invoke):
        assert invoke(["routes"]).exit_code == 0
"#;

/// `src/flask/json/__init__.py`: the function a dead-code question named.
const JSON: &str = r#"def load(fp, **kwargs):
    return fp
"#;

/// `tests/test_config.py`: the standard library's loader, passed by name.
const TEST_CONFIG: &str = r#"import pytest


def test_config_from_file_toml(app):
    tomllib = pytest.importorskip("tomllib", reason="tomllib added in 3.11")
    app.config.from_file("config.toml", tomllib.load, text=False)
"#;

struct Parsed {
    files: Vec<CensusFile>,
    entities: HashMap<String, Vec<Entity>>,
    /// What each file's coverage certificate proves, when the graph a census
    /// reads holds certificates at all.
    proven: Option<HashMap<String, Vec<OutsideMemberRead>>>,
}

impl Parsed {
    fn of(sources: &[(&str, &str)], linked: bool) -> Self {
        let pipeline = IndexPipeline::new();
        let mut files = Vec::new();
        let mut entities = HashMap::new();
        let mut parsed = Vec::new();
        for (path, source) in sources {
            let blob = kin_blobs::digest(source.as_bytes());
            let indexed = pipeline
                .index_file_content_with_tests(&FilePathId::new(*path), source.as_bytes(), blob)
                .unwrap()
                .indexed_file;
            files.push(CensusFile {
                path: FilePathId::new(*path),
                language: LanguageId::Python,
                blob: Hash256::from_hex(&blob.to_string()).unwrap(),
                parsed: Some(true),
            });
            entities.insert(path.to_string(), indexed.entities.clone());
            parsed.push(FileParseData {
                file_path: path.to_string(),
                entities: indexed.entities,
                relations: indexed.extracted_relations,
                imports: indexed.imports,
            });
        }
        let proven = linked.then(|| {
            let ids: HashMap<String, ArtifactId> = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ArtifactId::new()))
                .collect();
            let completeness = parsed
                .iter()
                .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
                .collect();
            let relations =
                kin_index::link_cross_file_with_completeness(&parsed, &ids, &completeness).unwrap();
            files
                .iter()
                .map(|file| {
                    let artifact = ids[&file.path.0];
                    let node = GraphNodeId::Artifact(artifact);
                    let reads = relations
                        .iter()
                        .filter(|relation| relation.src == node && relation.dst == node)
                        .find(|relation| CertificateDetail::from_certificate(relation).is_some())
                        .map(|certificate| {
                            kin_index::receiver_outside_reads_of(
                                certificate,
                                &file.path.0,
                                artifact,
                                file.blob,
                            )
                        })
                        .unwrap_or_default()
                        .into_iter()
                        .map(|read| OutsideMemberRead {
                            line: read.line,
                            member: read.member,
                            receiver: read.receiver,
                            owner: read.owner,
                        })
                        .collect();
                    (file.path.0.clone(), reads)
                })
                .collect()
        });
        Self {
            files,
            entities,
            proven,
        }
    }

    fn census(&self, path: &str, name: &str) -> FocalEscape {
        let focal = self.entities[path]
            .iter()
            .find(|entity| entity.name == name)
            .unwrap_or_else(|| panic!("no entity {name} in {path}"));
        focal_escape_evidence_in(self, focal, &focal_call_names(focal))
    }
}

impl EscapeCensusGraph for Parsed {
    fn census_files(&self, languages: &[LanguageId]) -> Option<Vec<CensusFile>> {
        Some(
            self.files
                .iter()
                .filter(|file| languages.contains(&file.language))
                .cloned()
                .collect(),
        )
    }

    fn entities_in(&self, path: &FilePathId) -> Option<Vec<Entity>> {
        Some(self.entities.get(&path.0).cloned().unwrap_or_default())
    }

    fn import_evidence_into(&self, _target: EntityId) -> Option<Vec<RelationEvidence>> {
        Some(Vec::new())
    }

    fn outside_member_reads(&self, file: &CensusFile) -> Vec<OutsideMemberRead> {
        self.proven
            .as_ref()
            .and_then(|proven| proven.get(&file.path.0).cloned())
            .unwrap_or_default()
    }
}

fn line_of(source: &str, text: &str) -> u32 {
    source
        .lines()
        .position(|line| line.contains(text))
        .unwrap_or_else(|| panic!("`{text}` is in the source")) as u32
        + 1
}

/// The value uses a census lists for a focal, as (file, line).
fn values(escape: &FocalEscape) -> Vec<(String, u32)> {
    escape
        .residue()
        .map(|residue| {
            residue
                .values
                .iter()
                .map(|site| (site.file.clone(), site.line))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_member_of_a_library_value_passed_on_does_not_hold_the_repositorys_member() {
    let sources = [
        ("src/flask/testing.py", TESTING),
        ("tests/test_cli.py", TEST_CLI),
    ];
    let focal = ("src/flask/testing.py", "FlaskCliRunner.invoke");

    // A graph with no proof for the read: it spells `invoke`, so it is
    // listed, and the focal counts as held as a value.
    let unlinked = Parsed::of(&sources, false).census(focal.0, focal.1);
    assert!(
        matches!(unlinked, FocalEscape::Escapes { .. }),
        "{unlinked:?}"
    );
    assert_eq!(
        values(&unlinked),
        vec![(
            "tests/test_cli.py".to_string(),
            line_of(TEST_CLI, "partial(runner.invoke")
        )]
    );

    // The linked graph proves `runner` is click's `CliRunner` there. The
    // read gets click's `invoke`, nothing holds the focal as a value, and
    // the read is counted with what it was proved to be.
    let linked = Parsed::of(&sources, true).census(focal.0, focal.1);
    assert!(
        matches!(linked, FocalEscape::Contained { .. }),
        "{linked:?}"
    );
    assert_eq!(values(&linked), Vec::<(String, u32)>::new());
    assert_eq!(
        linked.residue().unwrap().outside_values,
        vec![OutsideValueRead {
            receiver: "runner".into(),
            owner: "click.testing.CliRunner".into(),
            name: "invoke".into(),
            reads: 1,
        }]
    );
}

#[test]
fn a_member_of_a_module_loaded_by_name_is_that_modules() {
    let parsed = Parsed::of(
        &[
            ("src/flask/json/__init__.py", JSON),
            ("tests/test_config.py", TEST_CONFIG),
        ],
        false,
    );
    let escape = parsed.census("src/flask/json/__init__.py", "load");
    assert!(
        matches!(escape, FocalEscape::Contained { .. }),
        "{escape:?}"
    );
    assert_eq!(values(&escape), Vec::<(String, u32)>::new());
    assert_eq!(
        escape.residue().unwrap().outside_receivers,
        vec![OutsideReceiver {
            receiver: "tomllib".into(),
            module: "tomllib".into(),
            name: "load".into(),
            reads: 1,
        }]
    );
}
