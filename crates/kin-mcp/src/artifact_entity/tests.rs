// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_db::InMemoryGraph;
use kin_model::entity::{
    Entity, EntityKind, EntityMetadata, EntityRole, FingerprintAlgorithm, SemanticFingerprint,
    Visibility,
};
use kin_model::graph::EntityStore as _;
use kin_model::ids::{EntityId, FilePathId, Hash256, LanguageId};

use super::*;

fn module(name: &str, origin: &str) -> Entity {
    Entity {
        id: EntityId::new(),
        kind: EntityKind::Module,
        name: name.to_string(),
        language: LanguageId::Python,
        fingerprint: SemanticFingerprint {
            algorithm: FingerprintAlgorithm::V1TreeSitter,
            ast_hash: Hash256::from_bytes([0; 32]),
            signature_hash: Hash256::from_bytes([0; 32]),
            behavior_hash: Hash256::from_bytes([0; 32]),
            equivalence_hash: Hash256::from_bytes([0; 32]),
            stability_score: 1.0,
        },
        file_origin: Some(FilePathId::new(origin)),
        span: None,
        signature: format!("module {origin}"),
        visibility: Visibility::Public,
        role: EntityRole::Source,
        doc_summary: None,
        metadata: EntityMetadata::default(),
        lineage_parent: None,
        created_in: None,
        superseded_by: None,
    }
}

/// A small project: the package `shop` under `src`, with a schema and a logo
/// beside its code, two artifacts that share a name, and four at the root.
fn project() -> (InMemoryGraph, ResolvedTree) {
    let graph = InMemoryGraph::default();
    for (name, origin) in [
        ("shop", "src/shop/__init__.py"),
        ("db", "src/shop/db.py"),
        ("conf", "docs/conf.py"),
    ] {
        graph.upsert_entity(&module(name, origin)).unwrap();
    }
    let tree = ResolvedTree::from_artifacts(
        [
            "LICENSE",
            "README.md",
            "pyproject.toml",
            "src/shop/__init__.py",
            "src/shop/db.py",
            "src/shop/schema.sql",
            "src/shop/static/logo.png",
            "src/shop/README.md",
            "docs/conf.py",
            "docs/index.rst",
        ]
        .into_iter()
        .enumerate()
        .map(|(index, place)| {
            ResolvedArtifact::new(
                ArtifactId(uuid::Uuid::from_u128(
                    0x1000_0000_0000_0000_0000_0000_0000_0000 + ((index as u128) << 96),
                )),
                RepoPath::from_utf8(place.to_string()).unwrap(),
                TreeEntry::blob(Hash256::from_bytes([index as u8; 32]), false),
            )
        }),
    )
    .unwrap();
    (graph, tree)
}

fn label_of(named: Named) -> String {
    match named {
        Named::One(entity) => entity.label(),
        other => panic!("not one artifact: {other:?}"),
    }
}

#[test]
fn an_artifact_is_addressed_by_its_module_and_name_like_code() {
    let (graph, tree) = project();
    let held = Held::load(&graph, &tree, Some("shop".into())).unwrap();
    let labels: Vec<String> = held.artifacts().map(|entity| entity.label()).collect();
    assert_eq!(
        labels,
        [
            "shop :: LICENSE",
            "shop :: README.md",
            "docs :: index.rst",
            "shop :: pyproject.toml",
            "shop :: README.md",
            "shop :: schema.sql",
            "shop.static :: logo.png",
        ]
    );
    // With no declared project name the root has no module.
    let rootless = Held::load(&graph, &tree, None).unwrap();
    assert_eq!(label_of(rootless.named("LICENSE")), "LICENSE");
    assert_eq!(label_of(rootless.named("schema.sql")), "shop :: schema.sql");
}

#[test]
fn every_spelling_a_reply_serves_for_an_artifact_reads_back_as_it() {
    let (graph, tree) = project();
    let held = Held::load(&graph, &tree, Some("shop".into())).unwrap();
    let logo = held
        .artifacts()
        .find(|entity| entity.name == "logo.png")
        .unwrap();
    let id = logo.id().0.to_string();
    let short = logo.short_id(8);
    for spelling in [
        "logo.png".to_string(),
        "shop.static :: logo.png".to_string(),
        "shop.static::logo.png".to_string(),
        id.clone(),
        short.clone(),
        format!("#{short}"),
        format!("logo.png #{short}"),
        format!("shop.static :: logo.png #{short}"),
    ] {
        assert_eq!(
            held.named(&spelling),
            Named::One(logo.clone()),
            "{spelling}"
        );
    }
    // A name two artifacts share names neither; its label, or its id, does.
    let Named::Several(readmes) = held.named("README.md") else {
        panic!("two artifacts are named README.md");
    };
    assert_eq!(readmes.len(), 2);
    let addresses = held.addresses(&readmes);
    assert_eq!(addresses.len(), 2);
    for (address, readme) in addresses.iter().zip(&readmes) {
        assert_eq!(
            held.named(address.trim_matches('`')),
            Named::One(readme.clone()),
            "{address}"
        );
    }
    // Where an artifact is projected is not an address.
    for place in ["src/shop/static/logo.png", "./LICENSE", "static/logo.png"] {
        assert_eq!(held.named(place), Named::Nothing, "{place}");
    }
    // Neither is a module it does not sit in, or a name it only resembles.
    assert_eq!(held.named("docs :: logo.png"), Named::Nothing);
    assert_eq!(held.named("logo"), Named::Nothing);
}

#[test]
fn source_kin_parses_is_named_as_code_and_never_listed_as_an_artifact() {
    let (graph, tree) = project();
    let held = Held::load(&graph, &tree, None).unwrap();
    assert!(held.artifacts().all(|entity| !entity.name.ends_with(".py")));
    let Named::Code(code) = held.named("db.py") else {
        panic!("db.py is source Kin parses");
    };
    assert_eq!(code.name, "db.py");
    assert!(held.named_like("db").is_empty());
    assert!(held.projected_at("src/shop/db.py").is_none());
    let refusal = held_as_code(&graph, &code);
    assert!(refusal.starts_with("db.py is held as code: "), "{refusal}");
    assert!(crate::semantic_surface::is_semantic(&refusal), "{refusal}");
}

#[test]
fn a_name_search_ranks_the_exact_name_then_the_stem_then_a_name_that_holds_it() {
    let (graph, tree) = project();
    let held = Held::load(&graph, &tree, Some("shop".into())).unwrap();
    let names = |query: &str| -> Vec<String> {
        held.named_like(query)
            .into_iter()
            .map(|entity| entity.label())
            .collect()
    };
    assert_eq!(names("license"), ["shop :: LICENSE"]);
    assert_eq!(names("schema"), ["shop :: schema.sql"]);
    assert_eq!(names("logo"), ["shop.static :: logo.png"]);
    assert_eq!(names("readme"), ["shop :: README.md", "shop :: README.md"]);
    assert_eq!(names("e.md"), ["shop :: README.md", "shop :: README.md"]);
    assert_eq!(names("shop.static :: logo"), ["shop.static :: logo.png"]);
    assert!(names("src/shop/schema.sql").is_empty());
    assert!(names("nothing-like-this").is_empty());
}

#[test]
fn a_level_is_text_only_for_utf8_with_no_nul() {
    assert_eq!(Level::of(b"MIT License\n"), Level::Text);
    assert_eq!(Level::of("caf\u{e9}\n".as_bytes()), Level::Text);
    assert_eq!(Level::of(b""), Level::Text);
    assert_eq!(Level::of(b"\x89PNG\r\n\x1a\n"), Level::Bytes);
    assert_eq!(Level::of(b"text\0more"), Level::Bytes);
    for level in [Level::Text, Level::Bytes] {
        assert!(crate::semantic_surface::is_semantic(level.held()));
        assert!(level
            .held()
            .starts_with(&format!("held as {}", level.word())));
    }
}

fn edit(old_text: &str, new_text: &str) -> crate::source_base::EntityTextEdit {
    crate::source_base::EntityTextEdit {
        old_text: old_text.into(),
        new_text: new_text.into(),
    }
}

#[test]
fn a_patch_changes_the_one_place_each_anchor_names_and_nothing_else() {
    let text = "Changes\n=======\n\nUnreleased\n----------\n\n- One.\n- Two.\n";
    assert_eq!(
        apply_edits(
            "CHANGES.rst",
            text,
            &[
                edit("- Two.\n", "- Two.\n- Three.\n"),
                edit(
                    "Unreleased\n----------\n\n",
                    "Unreleased\n----------\n\n- Zero.\n"
                ),
            ],
        )
        .unwrap(),
        "Changes\n=======\n\nUnreleased\n----------\n\n- Zero.\n- One.\n- Two.\n- Three.\n"
    );
    // An artifact with no text takes its first text from an empty anchor.
    assert_eq!(
        apply_edits("NOTICE", "", &[edit("", "Notice.\n")]).unwrap(),
        "Notice.\n"
    );
}

#[test]
fn a_patch_that_names_no_one_place_is_refused_in_a_sentence_that_says_what_to_send() {
    let text = "- One.\n- Two.\n- One more.\n";
    for (edits, refusal) in [
        (
            vec![edit("- Nine.\n", "")],
            "patch edit #0: old_text is not in the text of CHANGES.rst, so copy it exactly from \
             what source returned for CHANGES.rst, or read CHANGES.rst again with source",
        ),
        (
            vec![edit("- One", "- 1")],
            "patch edit #0: old_text occurs 2 times in the text of CHANGES.rst, so extend it \
             with the text beside the one you mean until it occurs once",
        ),
        (
            vec![edit("- One.\n- Two", "x"), edit("- Two.\n", "y")],
            "patch edits #0 and #1 overlap in the text of CHANGES.rst, so send them as one edit",
        ),
        (
            vec![edit("- Two.\n", "- Two.\n")],
            "the patch leaves the text of CHANGES.rst as it is, so nothing was written",
        ),
        (
            vec![edit("", "- Zero.\n")],
            "patch edit #0: old_text is empty, which only gives an entity with no text its \
             first text, and CHANGES.rst holds text, so anchor old_text on text copied from what \
             source returned for CHANGES.rst",
        ),
        (vec![], "a patch needs at least one exact text edit"),
    ] {
        let refused = apply_edits("CHANGES.rst", text, &edits).unwrap_err();
        assert_eq!(refused, refusal);
        assert!(crate::semantic_surface::is_semantic(&refused), "{refused}");
    }
}

#[test]
fn a_description_names_its_kind_with_the_right_article_and_its_size_grouped() {
    for (kind, expected) in [
        ("PNG image", "a"),
        ("SVG image", "an"),
        ("MP3 recording", "an"),
        ("ELF binary", "an"),
        ("icon", "an"),
        ("binary", "a"),
        ("XZ archive", "an"),
        ("SQLite database", "a"),
        ("7z archive", "a"),
    ] {
        assert_eq!(article(kind), expected, "{kind}");
    }
    assert_eq!(grouped(7), "7");
    assert_eq!(grouped(12_480), "12,480");
    assert_eq!(grouped(1_048_576), "1,048,576");

    let reply = serde_json::json!({
        "name": "flask-logo.png", "kind": "PNG image", "held_as": "bytes",
        "artifact": {"level": "bytes", "bytes": 12_480, "detail": "500 by 200 pixels"},
    });
    let lead = plain_lead("flask-logo.png", reply.as_object().unwrap());
    assert_eq!(
        lead,
        "this is the description of flask-logo.png, a PNG image of 500 by 200 pixels and \
         12,480 bytes, held as bytes (Kin knows its name, type, size, history and what refers \
         to it, and never returns its bytes in a reply)"
    );
    assert!(crate::semantic_surface::is_semantic(&lead), "{lead}");
    let refusal = inline_bytes_refusal("flask-logo.png");
    assert!(crate::semantic_surface::is_semantic(&refusal), "{refusal}");
}

/// The kind registry lives in the indexer; what it names reaches an agent
/// here, so it is held to the semantic surface here.
#[test]
fn no_kind_reads_as_a_storage_word() {
    // A row's kind is prose an agent reads, and the semantic surface
    // withholds prose that names storage.
    for place in [
        "LICENSE",
        "a.lock",
        "Dockerfile",
        "Makefile",
        "a.po",
        ".gitmodules",
    ] {
        let name = place.rsplit('/').next().unwrap();
        let kind = kind::describe(place, name, true, b"").kind;
        assert!(crate::semantic_surface::is_semantic(&kind), "{kind}");
    }
    // Every kind any detector can name.
    for name in [
        "a.png",
        "a.jpg",
        "a.gif",
        "a.webp",
        "a.bmp",
        "a.ico",
        "a.tiff",
        "a.pdf",
        "a.zip",
        "a.gz",
        "a.tar",
        "a.whl",
        "a.jar",
        "a.woff",
        "a.mp3",
        "a.mp4",
        "a.wasm",
        "a.so",
        "a.exe",
        "a.pyc",
        "a.class",
        "a.db",
        "a.pkl",
        "a.npy",
        "a.parquet",
        "a.bin",
    ] {
        let kind = kind::describe(name, name, false, &[1, 2, 3, 0]).kind;
        assert!(crate::semantic_surface::is_semantic(&kind), "{kind}");
    }
}
