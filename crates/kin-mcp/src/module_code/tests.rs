// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use super::*;
use kin_model::{FilePathId, LanguageId};

/// A Python file as the graph holds it: its module surface and every entity
/// the real extractor derives from it.
fn parsed(path: &str, source: &str) -> (Entity, Vec<Entity>) {
    let adapters = kin_parser::AdapterRegistry::new();
    let adapter = adapters.get_by_language(LanguageId::Python).unwrap();
    let file = FilePathId::new(path);
    let tree = adapter.parse(source.as_bytes()).unwrap();
    let entities: Vec<Entity> = adapter
        .extract(&tree, source.as_bytes(), &file)
        .unwrap()
        .entities
        .into_iter()
        .map(|entity| {
            entity.into_entity_with_source(LanguageId::Python, &file, Some(source.as_bytes()))
        })
        .collect();
    let module = entities
        .iter()
        .find(|entity| kin_model::is_file_module_surface(entity))
        .cloned()
        .expect("every Python file has a module surface");
    (module, entities)
}

fn code(path: &str, source: &str) -> ModuleCode {
    let (module, entities) = parsed(path, source);
    ModuleCode::build(&module, source.as_bytes(), &entities).unwrap()
}

fn edit(old: &str, new: &str) -> EntityTextEdit {
    EntityTextEdit {
        old_text: old.into(),
        new_text: new.into(),
    }
}

fn apply(
    code: &ModuleCode,
    source: &str,
    edits: &[(&str, &str)],
) -> std::result::Result<String, String> {
    let edits: Vec<_> = edits.iter().map(|(old, new)| edit(old, new)).collect();
    code.apply(source.as_bytes(), &edits)
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

/// The marker line of the entity named `name`.
fn marker_of<'a>(code: &'a ModuleCode, name: &str) -> &'a str {
    code.held()
        .find(|held| held.name == name)
        .map(|held| held.marker.as_str())
        .unwrap_or_else(|| panic!("no marker for {name} in:\n{}", code.text()))
}

const HELPERS: &str = r#""""Helpers for the app."""
from __future__ import annotations

import os
import typing as t

from .globals import request

if t.TYPE_CHECKING:
    from .wrappers import Response

LIMIT = 3
__all__ = ["get_debug_flag", "Cache"]
_missing = object()


def get_debug_flag() -> bool:
    val = os.environ.get("APP_DEBUG")
    return bool(val and val.lower() not in {"0", "false", "no"})


@t.final
class Cache:
    """Holds values."""

    size = LIMIT

    def get(self, key: str) -> t.Any:
        return _missing

    def set(self, key: str, value: t.Any) -> None:
        self.value = value


try:
    import ujson as json
except ImportError:
    def loads(text: str) -> t.Any:
        return None


if __name__ == "__main__":
    print(get_debug_flag())
"#;

#[test]
fn module_code_is_every_top_level_statement_with_each_definition_as_one_marker() {
    let code = code("src/app/helpers.py", HELPERS);
    let text = code.text();
    let names: Vec<&str> = code.held().map(|held| held.name.as_str()).collect();
    assert_eq!(names, ["get_debug_flag", "Cache", "loads"], "{text}");
    let expected = format!(
        r#""""Helpers for the app."""
from __future__ import annotations

import os
import typing as t

from .globals import request

if t.TYPE_CHECKING:
    from .wrappers import Response

LIMIT = 3
__all__ = ["get_debug_flag", "Cache"]
_missing = object()


{}


{}


try:
    import ujson as json
except ImportError:
    {}


if __name__ == "__main__":
    print(get_debug_flag())
"#,
        marker_of(&code, "get_debug_flag"),
        marker_of(&code, "Cache"),
        marker_of(&code, "loads"),
    );
    assert_eq!(text, expected);
    // No line of any entity's body is served: a body is read by its own id.
    for inside in [
        "os.environ.get",
        "Holds values",
        "size = LIMIT",
        "def get(self",
        "self.value = value",
        "return None",
    ] {
        assert!(!text.contains(inside), "{inside} leaked into:\n{text}");
    }
    // A marker names the entity's kind, its name and the id `source` reads
    // it by, on one comment line, and nothing that its body could change.
    let marker = marker_of(&code, "get_debug_flag");
    let id = code.held().next().unwrap().id.to_string();
    assert_eq!(
        marker,
        format!("# kin:entity function get_debug_flag #{}", &id[..8])
    );
    // A decorator is part of the entity it decorates, so it is behind the
    // marker with the class.
    let cache = marker_of(&code, "Cache");
    assert!(cache.starts_with("# kin:entity class Cache #"), "{cache}");
    assert!(!text.contains("@t.final\n"), "{text}");
    assert!(!text
        .lines()
        .any(|line| line.contains(MARKER_WORD) && !line.trim_start().starts_with("# kin:entity ")));
}

#[test]
fn a_patch_adds_and_removes_imports_and_changes_module_statements_byte_exactly() {
    let code = code("src/app/helpers.py", HELPERS);
    let patched = apply(
        &code,
        HELPERS,
        &[
            ("import os\n", "import os\nimport re\n"),
            ("from .globals import request\n\n", ""),
            ("LIMIT = 3", "LIMIT = 5"),
            (
                "__all__ = [\"get_debug_flag\", \"Cache\"]",
                "__all__ = [\"get_debug_flag\", \"Cache\", \"loads\"]",
            ),
        ],
    )
    .unwrap();
    let expected = HELPERS
        .replace("import os\n", "import os\nimport re\n")
        .replace("from .globals import request\n\n", "")
        .replace("LIMIT = 3", "LIMIT = 5")
        .replace("\"Cache\"]", "\"Cache\", \"loads\"]");
    // Every byte of every entity is the byte it was, and `size = LIMIT`
    // inside the class is not the module's `LIMIT = 3`.
    assert_eq!(patched, expected);
    assert!(patched.contains("    size = LIMIT\n"));
}

#[test]
fn a_patch_inserts_code_beside_an_entity_by_keeping_its_marker() {
    let source = "def first():\n    return 1\n\n\ndef second():\n    return first()\n";
    let code = code("pkg/only_defs.py", source);
    assert_eq!(
        code.text(),
        format!(
            "{}\n\n\n{}\n",
            marker_of(&code, "first"),
            marker_of(&code, "second")
        )
    );
    // The file opens with a function, so the only anchor is its marker. The
    // marker is kept, and the import lands above the function it stands for.
    let first = marker_of(&code, "first").to_string();
    let anchor = "# kin:entity function first";
    let patched = apply(
        &code,
        source,
        &[(anchor, &format!("import re\n\n\n{anchor}"))],
    )
    .unwrap();
    assert_eq!(patched, format!("import re\n\n\n{source}"));
    // Between two entities, with both markers kept.
    let second = marker_of(&code, "second").to_string();
    let patched = apply(
        &code,
        source,
        &[(
            &format!("{first}\n\n\n{second}"),
            &format!("{first}\n\n\nRETRIES = 2\n\n\n{second}"),
        )],
    )
    .unwrap();
    assert_eq!(
        patched,
        "def first():\n    return 1\n\n\nRETRIES = 2\n\n\ndef second():\n    return first()\n"
    );
}

#[test]
fn each_refusal_is_one_sentence_that_says_what_to_send_next() {
    let code = code("src/app/helpers.py", HELPERS);
    let cache_id = code.held().find(|held| held.name == "Cache").unwrap().id;
    let first = marker_of(&code, "get_debug_flag").to_string();
    let cache = marker_of(&code, "Cache").to_string();
    let refused = |edits: &[(&str, &str)]| apply(&code, HELPERS, edits).unwrap_err();

    // An anchor copied from an entity's body names that entity and its id.
    assert_eq!(
        refused(&[("return _missing", "return None")]),
        format!(
            "patch edit #0: old_text is inside class Cache, which the module-level code of \
             helpers shows only as a kin:entity line, so read Cache with source (entity_id \
             {cache_id}) and patch it with the source_base that read returns"
        )
    );
    // So does one that starts in the module's code and runs into an entity.
    assert!(
        refused(&[("\n\ndef get_debug_flag() -> bool:\n    val", "x")])
            .starts_with("patch edit #0: old_text is inside function get_debug_flag, ")
    );
    assert_eq!(
        refused(&[("import sys", "import re")]),
        "patch edit #0: old_text is not in the module-level code of helpers, so copy it exactly \
         from the body source returned for helpers, or read helpers again with source"
    );
    assert_eq!(
        refused(&[("import os", "import re"), ("import", "from")]),
        "patch edit #1: old_text occurs 6 times in the module-level code of helpers, so extend \
         it with the text beside the one you mean until it occurs once"
    );
    assert_eq!(
        refused(&[("import os\nimport", "x"), ("os\nimport typing", "y")]),
        "patch edits #0 and #1 overlap in the module-level code of helpers, so send them as \
         one edit"
    );
    // A marker is not code: it cannot be rewritten, dropped, doubled or moved.
    let changed = refused(&[(
        "kin:entity function get_debug_flag",
        "kin:entity function renamed_flag",
    )]);
    assert!(
        changed.starts_with(
            "the patch changes or removes the kin:entity line that stands for function \
             get_debug_flag, which is not code of helpers, so keep that line exactly as source \
             returned it and change get_debug_flag by its own id (entity_id "
        ),
        "{changed}"
    );
    assert!(refused(&[(&format!("{first}\n"), "")]).starts_with(
        "the patch changes or removes the kin:entity line that stands for function get_debug_flag"
    ));
    assert!(
        refused(&[("LIMIT = 3", &format!("LIMIT = 3\n{first}"))]).starts_with(
            "the patch repeats the kin:entity line that stands for function get_debug_flag"
        )
    );
    assert!(refused(&[
        (&first, "PLACEHOLDER = 1"),
        (&cache, &format!("{cache}\n\n\n{first}"))
    ])
    .starts_with("the patch moves class Cache past another entity"));
    assert!(refused(&[(
        "LIMIT = 3",
        "LIMIT = 3\n# kin:entity invented #00000000 (function, 1 line)"
    )])
    .starts_with("new_text writes a kin:entity line"));
    // Edits that cancel each other change nothing, and say so.
    assert_eq!(
        refused(&[("LIMIT ", "LIMIT ="), ("= 3", " 3")]),
        "the patch leaves the module-level code of helpers as it is, so nothing was written"
    );
    for refusal in [
        refused(&[("return _missing", "return None")]),
        refused(&[("import sys", "import re")]),
        refused(&[("import", "from")]),
        changed,
    ] {
        assert_eq!(
            refusal.matches(". ").count(),
            0,
            "one sentence, not several: {refusal}"
        );
    }
}

#[test]
fn a_small_value_is_code_and_a_large_one_is_a_marker() {
    let table: String = (0..120)
        .map(|index| format!("    \"key_{index:03}\": {index},\n"))
        .collect();
    let source =
        format!("TIMEOUT = 30\nTABLE = {{\n{table}}}\n\n\ndef read():\n    return TABLE\n");
    let code = code("pkg/settings.py", &source);
    let text = code.text();
    assert!(
        text.starts_with("TIMEOUT = 30\n# kin:entity constant TABLE #"),
        "{text}"
    );
    assert!(!text.contains("key_050"), "{text}");
    assert!(text.len() < 512, "{} bytes:\n{text}", text.len());
    // The small value is patched as module code; the large one only by id.
    let patched = apply(&code, &source, &[("TIMEOUT = 30", "TIMEOUT = 60")]).unwrap();
    assert_eq!(patched, source.replace("TIMEOUT = 30", "TIMEOUT = 60"));
    assert!(
        apply(&code, &source, &[("\"key_050\": 50", "\"key_050\": 51")])
            .unwrap_err()
            .starts_with("patch edit #0: old_text is inside constant TABLE, ")
    );
}

/// A generated module of 180 handlers, each registered at module level: over
/// 60 KB of file, nearly all of it inside entities.
fn large_module() -> String {
    let mut source =
        String::from("\"\"\"Generated handlers.\"\"\"\nimport os\nimport sys\n\nREGISTRY = {}\n\n");
    for index in 0..180 {
        source.push_str(&format!(
            "\ndef handler_{index:03}(request):\n    payload_{index:03} = request.json()\n    \
             if payload_{index:03} is None:\n        raise ValueError(\"handler {index:03} needs \
             a body, and this sentence pads the function to a realistic length\")\n    for \
             attempt_{index:03} in range(3):\n        os.write(1, b\"body-{index:03}\")\n    \
             return {{\"handler\": {index}, \"ok\": True, \"body_marker\": \"BODY-{index:03}\"}}\n\n\
             REGISTRY[\"{index:03}\"] = handler_{index:03}\n"
        ));
    }
    source.push_str(
        "\n\nclass Dispatcher:\n    def dispatch(self, name, request):\n        return \
         REGISTRY[name](request)\n",
    );
    assert!(source.len() > 60_000, "{} bytes", source.len());
    source
}

/// What only an entity's body says in [`large_module`].
const LARGE_MODULE_BODIES: [&str; 5] = [
    "request.json()",
    "BODY-",
    "raise ValueError",
    "REGISTRY[name](request)",
    "os.write",
];

/// A file far larger than one reply serves, most of it inside entities, is
/// served as a few kilobytes of module-level code, and no entity body is in
/// them.
#[test]
fn a_large_module_serves_its_own_code_and_no_entity_body() {
    let source = large_module();
    let code = code("pkg/handlers.py", &source);
    let text = code.text();
    assert_eq!(code.held().count(), 181);
    for inside in LARGE_MODULE_BODIES {
        assert!(!text.contains(inside), "an entity body leaked: {inside}");
    }
    // Every registration between the functions is the module's own code.
    assert_eq!(text.matches("REGISTRY[\"").count(), 180);
    assert!(
        text.len() < source.len() / 3,
        "{} of {}",
        text.len(),
        source.len()
    );
    // A change at the top and one between two functions leave every body exact.
    let patched = apply(
        &code,
        &source,
        &[
            ("import sys\n", "import sys\nimport json\n"),
            (
                "REGISTRY[\"074\"] = handler_074",
                "REGISTRY[\"seventy-four\"] = handler_074",
            ),
        ],
    )
    .unwrap();
    assert_eq!(
        patched,
        source
            .replace("import sys\n", "import sys\nimport json\n")
            .replace(
                "REGISTRY[\"074\"] = handler_074",
                "REGISTRY[\"seventy-four\"] = handler_074"
            )
    );
}

/// Module-level code over one page is served the way any long body is: as
/// excerpts that join into the code, each without an edit base and none
/// holding an entity body. `full_body` serves it whole, with its base.
#[test]
fn module_level_code_over_one_page_is_served_as_excerpts_that_join() {
    let source = large_module();
    let (module, entities) = parsed("pkg/handlers.py", &source);
    let code = ModuleCode::build(&module, source.as_bytes(), &entities).unwrap();
    assert!(code.text().len() > crate::source_pages::SOURCE_PAGE_BYTES);
    let base = json!({
        "schema": "kin.entity.source_base.v1",
        "context": {
            "repository_id": "module-code-pages",
            "workspace_id": uuid::Uuid::new_v4().to_string(),
            "workspace_generation": 4,
            "workspace_head_hash": "a".repeat(64),
            "workspace_tree_hash": "b".repeat(64),
        },
        "entity_id": module.id,
        "artifact_id": kin_model::ArtifactId::new(),
        "source_blob_hash": "c".repeat(64),
        "start_byte": 0,
        "end_byte": source.len(),
        "body_hash": kin_blobs::digest(source.as_bytes()).to_string(),
    });
    let reply = json!({
        "source_base": base, "id": module.id, "name": module.name, "kind": module.kind,
        "body": code.text(), REPLY_KEY: {"entities": code.held().count(), "marker": MARKER_WORD},
    });
    let read = |arguments: Value| {
        let arguments: std::collections::HashMap<String, Value> =
            serde_json::from_value(arguments).unwrap();
        let mut result = crate::types::ToolCallResult::text(reply.to_string());
        crate::source_pages::present("get_entity_source", &arguments, &mut result);
        assert_ne!(result.is_error, Some(true));
        let crate::types::ContentBlock::Text { text } = &result.content[0];
        serde_json::from_str::<Value>(text).unwrap()
    };
    let mut page = read(json!({"entity_id": module.id}));
    let mut joined = String::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(
            page.get("source_base").is_none(),
            "an excerpt is no edit base: {page}"
        );
        assert_eq!(page["body_complete"], false);
        let served = page.to_string();
        for inside in LARGE_MODULE_BODIES {
            assert!(
                !served.contains(inside),
                "page {pages} holds an entity body: {inside}"
            );
        }
        assert!(
            served.len() < 16 * 1024,
            "page {pages} is {} bytes",
            served.len()
        );
        joined.push_str(page["body"].as_str().unwrap());
        let Some(cursor) = page["next_cursor"].as_str().map(str::to_owned) else {
            break;
        };
        page = read(json!({"entity_id": module.id, "cursor": cursor}));
    }
    assert!(pages > 1);
    assert_eq!(joined, code.text());
    let whole = read(json!({"entity_id": module.id, "full_body": true}));
    assert_eq!(whole["body"], code.text());
    assert_eq!(whole["source_base"], base);
}

/// A module with no code has no text to anchor on. One edit with an empty
/// anchor gives it its first code, and an empty anchor is refused everywhere
/// a module holds anything.
#[test]
fn an_empty_module_takes_its_first_code_through_one_empty_anchor() {
    let (module, entities) = parsed("pkg/__init__.py", "");
    let empty = ModuleCode::build(&module, b"", &entities).unwrap();
    assert_eq!(empty.text(), "");
    assert_eq!(empty.held().count(), 0);
    assert_eq!(
        apply(&empty, "", &[("", "from .core import run\n")]).unwrap(),
        "from .core import run\n"
    );
    assert_eq!(
        apply(&empty, "", &[("", "")]).unwrap_err(),
        "the patch leaves the module-level code of pkg as it is, so nothing was written"
    );
    assert!(apply(
        &empty,
        "",
        &[("", "# kin:entity run #00000000 (function, 1 line)\n")]
    )
    .unwrap_err()
    .starts_with("new_text writes a kin:entity line"));
    assert!(apply(&empty, "", &[("import os", "import re")])
        .unwrap_err()
        .starts_with("patch edit #0: old_text is not in the module-level code of pkg"));

    // A module that holds code is anchored on that code.
    let helpers = code("src/app/helpers.py", HELPERS);
    assert_eq!(
        apply(&helpers, HELPERS, &[("", "import re\n")]).unwrap_err(),
        "patch edit #0: old_text is empty, which only gives a module with no code its first \
         code, and helpers holds code, so anchor old_text on text copied from the body source \
         returned for helpers"
    );
    assert_eq!(
        apply(
            &helpers,
            HELPERS,
            &[("", "import re\n"), ("LIMIT = 3", "LIMIT = 4")]
        )
        .unwrap_err(),
        "patch edit #0: old_text must be nonempty"
    );
    // So is a module that holds only entities: its markers are its anchors.
    let source = "def first():\n    return 1\n";
    let only_entities = code("pkg/only_defs.py", source);
    assert!(apply(&only_entities, source, &[("", "import re\n")])
        .unwrap_err()
        .starts_with("patch edit #0: old_text is empty, which only gives a module with no code"));
}

#[test]
fn spans_that_cross_or_leave_the_module_refuse_the_whole_view() {
    let source = "import os\n\n\ndef first():\n    return 1\n\n\ndef second():\n    return 2\n";
    let (module, mut entities) = parsed("pkg/crossing.py", source);
    let second_start = source.find("def second").unwrap();
    let first = entities
        .iter_mut()
        .find(|entity| entity.name == "first")
        .unwrap();
    first.span.as_mut().unwrap().end_byte = second_start + 4;
    assert!(ModuleCode::build(&module, source.as_bytes(), &entities)
        .unwrap_err()
        .contains("overlaps another entity"));
    let (module, mut entities) = parsed("pkg/crossing.py", source);
    let first = entities
        .iter_mut()
        .find(|entity| entity.name == "first")
        .unwrap();
    first.span.as_mut().unwrap().end_byte = source.len() + 10;
    assert!(ModuleCode::build(&module, source.as_bytes(), &entities)
        .unwrap_err()
        .contains("outside module"));
    // A language whose module code is not served has no view at all.
    let (mut module, entities) = parsed("pkg/crossing.py", source);
    module.language = LanguageId::Go;
    assert!(ModuleCode::build(&module, source.as_bytes(), &entities).is_err());
}

#[test]
fn the_plain_lead_says_what_the_markers_stand_for() {
    let reply = json!({"name": "helpers", REPLY_KEY: {"entities": 3, "marker": MARKER_WORD}});
    let reply = reply.as_object().unwrap();
    assert!(is_module_code_reply(reply));
    assert_eq!(
        plain_lead("helpers", reply),
        "this is the module-level code of helpers, its imports and top-level statements (each \
         of the 3 kin:entity lines stands for one entity it holds, read with source and the #id \
         on that line)"
    );
    assert!(!is_module_code_reply(
        json!({"name": "helpers"}).as_object().unwrap()
    ));
    // A reply that carries the module's dotted name is led with it.
    let named = json!({"name": "helpers", "module": "flask.helpers",
        REPLY_KEY: {"entities": 1, "marker": MARKER_WORD}});
    assert_eq!(
        plain_lead("helpers", named.as_object().unwrap()),
        "this is the module-level code of flask.helpers, its imports and top-level statements \
         (the kin:entity line stands for the one entity it holds, read with source and the #id \
         on that line)"
    );
    let empty =
        json!({"name": "pkg", REPLY_KEY: {"entities": 0, "marker": MARKER_WORD, "empty": true}});
    assert_eq!(
        plain_lead("pkg", empty.as_object().unwrap()),
        "pkg is a module that holds no code yet (give it its first code with a patch on this \
         source_base whose one edit has an empty old_text and the code as new_text)"
    );
}

/// A committed module patch reads as a change to the module-level code of
/// the module, in the sentence every write is answered with.
#[test]
fn a_committed_module_patch_names_the_module_level_code_it_changed() {
    let receipt = json!({
        "state": "committed", "ops_applied": 1,
        crate::write_reply::UNITS_WRITTEN_KEY: 1,
        crate::write_reply::ENTITY_CHANGES_KEY: [{
            "entity_id": "00000000-0000-4000-8000-000000000001",
            "name": "app", "kind": "module", "module": "flask.app",
            "change": crate::write_reply::CHANGED,
        }],
    });
    let plain = crate::write_reply::plain_receipt(receipt.as_object().unwrap()).unwrap();
    assert_eq!(
        plain["_kin"]["meaning"],
        "Changed the module-level code of flask.app; 1 source unit written; the graph now holds \
         it."
    );
    // A row about any other entity keeps its own address.
    assert!(
        change_label(&json!({"name": "run", "kind": "function", "module": "flask.app"})).is_none()
    );
    assert_eq!(
        change_label(&json!({"name": "pkg", "kind": "module"})).unwrap(),
        "the module-level code of pkg"
    );
}

const SHAPES: &str = r#""""Shapes."""
import math

SIDES = 0


def area(radius):
    return math.pi * radius * radius


class Outer:
    """Holds a class."""

    class Circle:
        """A circle.

        With a longer docstring.
        """

        #: How many sides.
        sides = SIDES
        register(SIDES)

        def __init__(self, radius):
            self.radius = radius

        scale = 2
        set_scale(2)

        @property
        def area(self):
            def squared(value):
                return value * value

            return math.pi * squared(self.radius)

        class Unit:
            radius = 1

            def area(self):
                return math.pi

        last = "last"
        finish("last")

    def after(self):
        return 1
"#;

/// The class named `name` in `source`, and every entity of its file.
fn class_of(path: &str, source: &str, name: &str) -> (Entity, Vec<Entity>) {
    let (_, entities) = parsed(path, source);
    let class = entities
        .iter()
        .find(|entity| entity.name == name && entity.kind == EntityKind::Class)
        .cloned()
        .unwrap_or_else(|| panic!("no class {name}"));
    (class, entities)
}

/// A class's own code is its header, its docstring and every statement of
/// its body, with each member one marker at the member's own indentation:
/// a field, a method with whatever is nested in it, or a nested class.
/// Bare class-level calls remain the class's own statements.
/// Nothing beside the class and nothing it stands in is its code.
#[test]
fn class_code_is_the_class_with_each_member_as_one_marker() {
    let (class, entities) = class_of("src/shapes.py", SHAPES, "Circle");
    let code = ModuleCode::build(&class, SHAPES.as_bytes(), &entities).unwrap();
    assert_eq!(code.noun(), "class");
    assert_eq!(code.label(), "the class-level code of Circle");
    let names: Vec<&str> = code.held().map(|held| held.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Circle.sides",
            "Circle.__init__",
            "Circle.scale",
            "Circle.area",
            "Unit",
            "Circle.last"
        ],
        "{}",
        code.text()
    );
    assert_eq!(
        code.text(),
        format!(
            "class Circle:\n        \"\"\"A circle.\n\n        With a longer docstring.\n        \"\"\"\n\n        #: How many sides.\n        {}\n        register(SIDES)\n\n        {}\n\n        {}\n        set_scale(2)\n\n        {}\n\n        {}\n\n        {}\n        finish(\"last\")",
            marker_of(&code, "Circle.sides"),
            marker_of(&code, "Circle.__init__"),
            marker_of(&code, "Circle.scale"),
            marker_of(&code, "Circle.area"),
            marker_of(&code, "Unit"),
            marker_of(&code, "Circle.last"),
        )
    );
    assert!(marker_of(&code, "Circle.sides").starts_with("# kin:entity field Circle.sides #"));
    assert!(marker_of(&code, "Circle.area").starts_with("# kin:entity method Circle.area #"));
    assert!(marker_of(&code, "Unit").starts_with("# kin:entity class Unit #"));
}

/// A patch to a class's own code lands in the class and puts every member
/// back as its exact bytes; the rest of the source is as it was.
#[test]
fn a_class_patch_changes_its_statements_and_keeps_every_member_and_the_source_around_it() {
    let (class, entities) = class_of("src/shapes.py", SHAPES, "Circle");
    let code = ModuleCode::build(&class, SHAPES.as_bytes(), &entities).unwrap();
    let patched = apply(
        &code,
        SHAPES,
        &[
            ("register(SIDES)", "register(SIDES + 1)"),
            (
                "        set_scale(2)\n",
                "        set_scale(3)\n        set_ratio(0.5)\n",
            ),
            ("finish(\"last\")", "finish(\"changed\")"),
        ],
    )
    .unwrap();
    assert_eq!(
        patched,
        SHAPES
            .replace("register(SIDES)", "register(SIDES + 1)")
            .replace(
                "        set_scale(2)\n",
                "        set_scale(3)\n        set_ratio(0.5)\n"
            )
            .replace("finish(\"last\")", "finish(\"changed\")")
    );
}

/// What a class patch may not do, each in one sentence that names the call
/// that does it.
#[test]
fn each_class_refusal_is_one_sentence_that_says_what_to_send_next() {
    let (class, entities) = class_of("src/shapes.py", SHAPES, "Circle");
    let code = ModuleCode::build(&class, SHAPES.as_bytes(), &entities).unwrap();
    let init = entities
        .iter()
        .find(|entity| entity.name == "Circle.__init__")
        .unwrap();
    let field = entities
        .iter()
        .find(|entity| entity.name == "Circle.scale")
        .unwrap();
    let marker = marker_of(&code, "Circle.__init__").to_string();
    for (edits, expected) in [
        (
            vec![("scale = 2", "scale = 3")],
            format!(
                "patch edit #0: old_text is inside field Circle.scale, which the \
                class-level code of Circle shows only as a kin:entity line, so read \
                Circle.scale with source (entity_id {}) and patch it with the \
                source_base that read returns",
                field.id
            ),
        ),
        (
            vec![("self.radius = radius", "self.radius = 2 * radius")],
            format!(
                "patch edit #0: old_text is inside method Circle.__init__, which the \
                 class-level code of Circle shows only as a kin:entity line, so read \
                 Circle.__init__ with source (entity_id {}) and patch it with the \
                 source_base that read returns",
                init.id
            ),
        ),
        (
            vec![("SIDES = 0", "SIDES = 1")],
            "patch edit #0: old_text is not in the class-level code of Circle, so copy it \
             exactly from the body source returned for Circle, or read Circle again \
             with source"
                .to_string(),
        ),
        (
            vec![(
                "set_scale(2)",
                "set_scale(2)\n        # kin:entity method extra #00000000",
            )],
            "new_text writes a kin:entity line, and those lines stand for entities Kin already \
             holds and are never written as code, so leave it out and create a method with \
             EntityCreate and placement member"
                .to_string(),
        ),
        (
            vec![(marker.as_str(), "")],
            format!(
                "the patch changes or removes the kin:entity line that stands for method \
                 Circle.__init__, which is not code of Circle, so keep that line \
                 exactly as source returned it and change Circle.__init__ by its own id \
                 (entity_id {}): source it and patch it, or remove it with EntityRemove",
                init.id
            ),
        ),
        (
            vec![("set_scale(2)", "set_scale(2)")],
            "the patch leaves the class-level code of Circle as it is, so nothing was \
             written"
                .to_string(),
        ),
    ] {
        assert_eq!(
            apply(&code, SHAPES, &edits).unwrap_err(),
            expected,
            "{edits:?}"
        );
    }
}

/// Only a class no reply holds whole, with a member in it, is served as its
/// own code; every other class is one entity, read and written whole.
#[test]
fn only_a_class_too_large_to_serve_whole_is_served_as_its_own_code() {
    use kin_model::EntityStore;
    let graph = kin_db::InMemoryGraph::new();
    let mut big = String::from("class Big:\n    limit = 1\n");
    for index in 0..80 {
        big.push_str(&format!(
            "\n    def method_{index:02}(self, value):\n        \"\"\"A method long enough that eighty of them outgrow a reply.\"\"\"\n        total = value + {index}\n        if total > self.limit:\n            total = total - self.limit\n        return total\n"
        ));
    }
    let mut flat = String::from("class Flat:\n");
    for index in 0..1200 {
        flat.push_str(&format!("    value_{index:04} = {index}\n"));
    }
    let mut plain = String::from("class Plain:\n");
    for index in 0..1200 {
        plain.push_str(&format!("    register({index})\n"));
    }
    let source = format!(
        "{big}\n\n{flat}\n\n{plain}\n\nclass Small:\n    def get(self):\n        return 1\n"
    );
    let (module, entities) = parsed("src/classes.py", &source);
    for entity in &entities {
        graph.upsert_entity(entity).unwrap();
    }
    let class = |name: &str| {
        entities
            .iter()
            .find(|entity| entity.name == name && entity.kind == EntityKind::Class)
            .unwrap()
    };
    let bytes = |entity: &Entity| {
        let span = entity.span.as_ref().unwrap();
        span.end_byte - span.start_byte
    };
    assert!(bytes(class("Big")) > 16 * 1024 && bytes(class("Flat")) > 16 * 1024);
    assert!(serves_class_code(&graph, class("Big")));
    // Independently held fields are members too, even without any method.
    assert!(serves_class_code(&graph, class("Flat")));
    // Bare calls have no member declaration; own code is the whole class.
    assert!(bytes(class("Plain")) > 16 * 1024);
    assert!(!serves_class_code(&graph, class("Plain")));
    assert!(!serves_class_code(&graph, class("Small")));
    assert!(!serves_class_code(&graph, &module));
}

#[test]
fn the_plain_lead_of_a_class_says_what_the_markers_stand_for() {
    let reply = |members: u64| {
        let value = json!({"class_code": {"members": members, "marker": MARKER_WORD}});
        value.as_object().unwrap().clone()
    };
    assert!(is_module_code_reply(&reply(2)) && is_class_code_reply(&reply(2)));
    assert_eq!(
        plain_lead("Flask", &reply(35)),
        "this is the class-level code of Flask, its header, docstring and the statements of its \
         body outside every member (each of the 35 kin:entity lines stands for one member it \
         holds, read and changed with source and the #id on that line)"
    );
    assert_eq!(
        plain_lead("Flask", &reply(1)),
        "this is the class-level code of Flask, its header, docstring and the statements of its \
         body outside every member (the kin:entity line stands for the one member it holds, \
         read and changed with source and the #id on that line)"
    );
}
