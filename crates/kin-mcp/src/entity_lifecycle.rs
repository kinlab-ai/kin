// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Guarded creation and removal of declarations.
//!
//! Creation has two forms. Anchored creation places a declaration relative to
//! a real one, bound to that declaration's source base: a function beside a
//! function, and, where the language places members, a method beside a method or in a class, and a
//! class beside a top-level declaration. Unit-addressed creation names a
//! source unit by language identity and is bound to the repository base the
//! caller last observed; it is how the first declaration of an empty
//! repository, and every Go declaration kind, is created, in every language
//! whose adapter edits units. Neither form
//! accepts a path or an offset.

use serde::{Deserialize, Serialize};

use crate::source_base::EntitySourceBase;
use crate::source_unit::{DeclarationKind, ImportSpec, RepositoryBase, SourceUnit};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityPlacement {
    /// Directly after the anchor, in the scope the anchor is declared in.
    SiblingAfter,
    /// The last member of the anchor, which is a class.
    Member,
    NewSourceUnit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityCreate {
    /// Anchored form: the actual independent anchor's unchanged source base.
    /// This does not grant authority to rewrite its containing artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_base: Option<EntitySourceBase>,
    /// Anchored form: where the function goes relative to its anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<EntityPlacement>,
    /// Unit form: the repository instant the caller last observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_base: Option<RepositoryBase>,
    /// Unit form: the source unit, by language identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<SourceUnit>,
    pub name: String,
    pub kind: DeclarationKind,
    pub body: String,
    /// Unit form: imports the declaration needs, merged into the unit's block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportSpec>,
}

/// Which of the two creation forms a payload takes.
#[derive(Debug, Clone, Copy)]
pub enum CreateForm<'a> {
    Anchored {
        source_base: &'a EntitySourceBase,
        placement: EntityPlacement,
    },
    Unit {
        repository_base: &'a RepositoryBase,
        unit: &'a SourceUnit,
    },
}

const CREATE_FORMS: &str = "EntityCreate takes one of two forms: addressed to a unit, with \
repository_base and unit (any supported declaration kind, and the only form for an empty \
repository), or anchored, with the source_base of a current declaration and placement";

fn is_ascii_identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

impl EntityCreate {
    pub fn form(&self) -> Result<CreateForm<'_>, String> {
        match (
            &self.source_base,
            self.placement,
            &self.repository_base,
            &self.unit,
        ) {
            (Some(source_base), Some(placement), None, None) => Ok(CreateForm::Anchored {
                source_base,
                placement,
            }),
            (None, None, Some(repository_base), Some(unit)) => Ok(CreateForm::Unit {
                repository_base,
                unit,
            }),
            _ => Err(CREATE_FORMS.into()),
        }
    }

    /// The anchor's source base, for the anchored form.
    pub fn anchor(&self) -> Option<(&EntitySourceBase, EntityPlacement)> {
        match self.form() {
            Ok(CreateForm::Anchored {
                source_base,
                placement,
            }) => Some((source_base, placement)),
            _ => None,
        }
    }

    /// The repository base and unit, for the unit form.
    pub fn unit_target(&self) -> Option<(&RepositoryBase, &SourceUnit)> {
        match self.form() {
            Ok(CreateForm::Unit {
                repository_base,
                unit,
            }) => Some((repository_base, unit)),
            _ => None,
        }
    }

    /// The `target` an operation carrying this payload must name: the anchor's
    /// UUID for the anchored form, the declared name for the unit form.
    pub fn expected_target(&self) -> Option<String> {
        match self.form().ok()? {
            CreateForm::Anchored { source_base, .. } => Some(source_base.entity_id.to_string()),
            CreateForm::Unit { .. } => Some(self.name.clone()),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.body.trim().is_empty() {
            return Err("EntityCreate requires the exact nonempty body of one declaration".into());
        }
        match self.form()? {
            CreateForm::Anchored { source_base, .. } => {
                source_base.validate()?;
                if !matches!(
                    self.kind,
                    DeclarationKind::Function | DeclarationKind::Method | DeclarationKind::Class
                ) {
                    return Err(format!(
                        "anchored EntityCreate creates a top-level function in {}, and a method \
                         or a class in {}; create other declaration kinds addressed to a unit, \
                         with repository_base and unit",
                        kin_parser::declaration_lifecycle::lifecycle_languages("and"),
                        kin_parser::member_placement::member_placement_languages("and")
                    ));
                }
                if !self.imports.is_empty() {
                    return Err(
                        "anchored EntityCreate does not manage imports; address the \
                         unit, with repository_base and unit, to add imports with the \
                         declaration"
                            .into(),
                    );
                }
                if !is_ascii_identifier(&self.name) {
                    return Err(
                        "EntityCreate name must be one ASCII identifier, never a path".into(),
                    );
                }
                Ok(())
            }
            CreateForm::Unit {
                repository_base,
                unit,
            } => {
                repository_base.validate()?;
                unit.validate()?;
                crate::source_unit::validate_create_imports(unit, &self.imports)?;
                let editor = crate::source_unit::unit_editor(unit)?;
                let language = unit.language();
                let expected = self.kind.unit();
                // A syntax problem is said by its line in what was sent,
                // before the shape of the body is asked about.
                crate::syntax_gate::refuse_unparseable_code(
                    language,
                    self.body.trim(),
                    &format!("the code sent for {}", self.name),
                )?;
                let parsed = editor
                    .parse_declaration(&self.body)
                    .map_err(|error| format!("EntityCreate body: {error}"))?;
                if parsed.kind != expected {
                    return Err(format!(
                        "EntityCreate kind {} does not match its body, which declares a {} {}",
                        expected.label(),
                        language.profile().display_name,
                        parsed.kind.label()
                    ));
                }
                // A blank var or const (`var _ Getter = (*Store)(nil)`) is
                // named `_` by the caller; its entity takes the derived name
                // the reply reports.
                let blank = self.name == "_"
                    && parsed
                        .names
                        .first()
                        .is_some_and(|name| name.starts_with("_ "));
                if parsed.names.first() != Some(&self.name) && !blank {
                    return Err(format!(
                        "EntityCreate name {:?} must be the body's declared name {:?}: Name, \
                         Receiver.Method for a method, the first name of a grouped const or \
                         var, or _ for a blank var or const",
                        self.name,
                        parsed.names.first().cloned().unwrap_or_default()
                    ));
                }
                Ok(())
            }
        }
    }
}

/// Deterministic projection choice shared with intent admission. Its inputs come
/// from the authoritative anchor, never from a caller-supplied file locator.
pub fn generated_source_path(
    origin: &kin_model::FilePathId,
    language: kin_model::LanguageId,
    name: &str,
) -> Result<kin_model::RepoPath, String> {
    let mut chars = name.bytes();
    if !chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err("new source unit requires one semantic identifier".into());
    }
    let extension = kin_parser::declaration_lifecycle::declaration_lifecycle_for(language)
        .ok_or("unsupported lifecycle language")?
        .new_unit_extension(&origin.0);
    let parent = std::path::Path::new(&origin.0)
        .parent()
        .ok_or("anchor has no owner directory")?;
    let generated = parent.join(format!("{name}.{extension}"));
    let path =
        kin_model::RepoPath::from_utf8(generated.to_str().ok_or("non-UTF8 owner")?.to_owned())
            .map_err(|e| e.to_string())?;
    kin_core::validate_source_paths([&path]).map_err(|e| e.to_string())?;
    Ok(path)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityRemove {
    pub source_base: EntitySourceBase,
}

impl EntityRemove {
    pub fn validate(&self) -> Result<(), String> {
        self.source_base.validate()
    }
}

pub fn entity_create_schema() -> serde_json::Value {
    let languages = kin_parser::declaration_lifecycle::lifecycle_languages("or");
    let members = kin_parser::member_placement::member_placement_languages("or");
    let units = crate::source_unit::unit_language_names();
    serde_json::json!({
        "type":"object",
        "description":format!("Create one declaration in either semantic form. Addressed to a unit (repository_base and unit), in {units}: the form for an empty repository and for every Go declaration kind. Kin resolves the unit, supplies a Go package clause, places the declaration (a Go method directly after its receiver type or that type's last method, anything else at the end of the unit) and merges imports into the unit's import block. The operation's target is the declared name. Anchored (source_base and placement): the anchor's UUID is the target, and its source_base binds the write without its body being resent. One top-level function in {languages} beside a current function. In {members} also a method, with kind method: beside a method of the same class (that method's source_base, placement sibling_after) or as the last member of a class (the class's source_base, placement member); and a class or a function after a top-level class or function (placement sibling_after). The body is the new declaration alone, written at the left margin or as it would sit in the unit: Kin sets it at the indentation of the place it goes. Occupied names, ambiguous placement and stale bases refuse without publication."),
        "properties":{
            "repository_base":crate::source_unit::repository_base_schema(),
            "unit":crate::source_unit::source_unit_schema(),
            "source_base":crate::source_ticket::write_base_schema(),
            "placement":{"type":"string","enum":["sibling_after","member","new_source_unit"],"description":"Where the declaration goes. With sibling_after it is written directly after the anchor, in the anchor's scope. With member it becomes the last member of the anchor, which is a class. With new_source_unit it starts a new unit beside the anchor's, for one top-level function."},
            "name":{"type":"string","minLength":1,"description":"The declared name. Unit form, as the graph names it: Name, Receiver.Method for a method, the first name of a grouped const or var, or _ for a blank var or const such as var _ Getter = (*Store)(nil), whose entity is named after the assertion (_ Getter = (*Store)(nil)). Anchored form: the one identifier the body declares, so a method is named by its own name and not Class.method."},
            "kind":{"type":"string","enum":crate::source_unit::DeclarationKind::WIRE_NAMES,"description":format!("Go admits function, method, struct, interface, type, const and var; anchored creation admits function, and in {members} method and class. The graph kinds class, type_alias, constant and static_var are accepted as synonyms.")},
            "body":{"type":"string","minLength":1,"description":"Exactly the declaration's source, optionally led by its doc comment: no package clause, imports or sibling declarations."},
            "imports":{"type":"array","items":crate::source_unit::import_spec_schema(),"description":"Unit form: import paths the declaration needs, added to the unit's import block."}
        },
        "required":["name","kind","body"],
        "oneOf":[{"required":["repository_base","unit"]},{"required":["source_base","placement"]}],
        "additionalProperties":false
    })
}

pub fn entity_remove_schema() -> serde_json::Value {
    let languages = kin_parser::declaration_lifecycle::lifecycle_languages("or");
    let members = kin_parser::member_placement::member_placement_languages("or");
    serde_json::json!({
        "type":"object",
        "description":format!("Remove one source-bound top-level leaf function in {languages}, with its doc comment, or one method of a class in {members} with whatever is declared inside it, preserving its enclosing unit, its class and its sibling declarations; or one entity Kin holds without parsing it."),
        "properties":{"source_base":crate::source_ticket::write_base_schema()},
        "required":["source_base"],
        "additionalProperties":false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_base::{SourceBaseContext, SourceBaseSchema};

    fn base() -> EntitySourceBase {
        EntitySourceBase {
            schema: SourceBaseSchema::V1,
            context: SourceBaseContext {
                repository_id: "lifecycle-test".into(),
                workspace_id: uuid::Uuid::new_v4().to_string(),
                workspace_generation: 1,
                workspace_head_hash: "a".repeat(64),
                workspace_tree_hash: "b".repeat(64),
            },
            entity_id: kin_model::EntityId::new(),
            artifact_id: kin_model::ArtifactId::new(),
            source_blob_hash: "c".repeat(64),
            start_byte: 0,
            end_byte: 12,
            body_hash: "d".repeat(64),
        }
    }

    fn anchored(body: &str) -> EntityCreate {
        EntityCreate {
            source_base: Some(base()),
            placement: Some(EntityPlacement::SiblingAfter),
            repository_base: None,
            unit: None,
            name: "added".into(),
            kind: DeclarationKind::Function,
            body: body.into(),
            imports: Vec::new(),
        }
    }

    fn repository_base() -> RepositoryBase {
        RepositoryBase {
            schema: crate::source_unit::RepositoryBaseSchema::V1,
            context: base().context,
        }
    }

    /// An operation as a caller sends it: absent fields omitted, not null.
    fn wire(op: &crate::McpMutationOperation) -> serde_json::Value {
        let mut value = serde_json::to_value(op).unwrap();
        let fields = value.as_object_mut().unwrap();
        fields.retain(|_, field| !field.is_null());
        value
    }

    fn unit_create(name: &str, kind: DeclarationKind, body: &str) -> crate::McpMutationOperation {
        crate::McpMutationOperation {
            verb: "create".into(),
            target: name.into(),
            payload: Some(crate::McpMutationPayload::EntityCreate(EntityCreate {
                source_base: None,
                placement: None,
                repository_base: Some(repository_base()),
                unit: Some(SourceUnit::go(
                    "internal/store",
                    "store",
                    crate::source_unit::UnitRole::Source,
                )),
                name: name.into(),
                kind,
                body: body.into(),
                imports: vec![ImportSpec::Path("sync".into())],
            })),
            body: None,
            destination: None,
            description: "create a declaration".into(),
        }
    }

    #[test]
    fn unit_creation_is_addressed_by_language_identity_and_closed() {
        for (name, kind, body) in [
            (
                "Store",
                DeclarationKind::Struct,
                "type Store struct {\n\tmu sync.Mutex\n}",
            ),
            (
                "Store.Len",
                DeclarationKind::Method,
                "func (s *Store) Len() int { return 0 }",
            ),
            (
                "Getter",
                DeclarationKind::Interface,
                "type Getter interface{ Get(string) string }",
            ),
            (
                "Limit",
                DeclarationKind::Const,
                "const (\n\tLimit = 10\n\tFloor = 1\n)",
            ),
            (
                "ErrMissing",
                DeclarationKind::Var,
                "var ErrMissing = errors.New(\"missing\")",
            ),
            ("ID", DeclarationKind::Type, "type ID string"),
            (
                "New",
                DeclarationKind::Function,
                "// New builds one.\nfunc New() *Store { return &Store{} }",
            ),
            ("_", DeclarationKind::Var, "var _ Getter = (*Store)(nil)"),
        ] {
            let op = unit_create(name, kind, body);
            crate::session::validate_semantic_operations(std::slice::from_ref(&op))
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            let wire = wire(&op);
            assert_eq!(
                crate::session::parse_staged_operations(&serde_json::json!([wire]))
                    .unwrap()
                    .len(),
                1
            );
        }
        let refused = |op: crate::McpMutationOperation| {
            crate::session::validate_semantic_operations(&[op]).unwrap_err()
        };
        // The target names the declaration, never a file.
        let mut op = unit_create("Store", DeclarationKind::Struct, "type Store struct{}");
        op.target = "internal/store/store.go".into();
        assert!(refused(op).contains("target"));
        // The name and kind must be the body's own.
        assert!(refused(unit_create(
            "Other",
            DeclarationKind::Struct,
            "type Store struct{}"
        ))
        .contains("declared name"));
        assert!(refused(unit_create(
            "Store",
            DeclarationKind::Interface,
            "type Store struct{}"
        ))
        .contains("does not match"));
        assert!(refused(unit_create(
            "Len",
            DeclarationKind::Method,
            "func (s *Store) Len() int { return 0 }"
        ))
        .contains("Receiver.Method"));
        // Code that does not parse is refused by the line of its problem.
        assert_eq!(
            refused(unit_create(
                "F",
                DeclarationKind::Function,
                "func F() int {\n\treturn g(1\n}"
            )),
            "operation #0 ('create'): the code sent for F is not valid Go at line 2, `return \
             g(1`: `)` is missing"
        );
        // Kin owns the package clause and the import block.
        assert!(refused(unit_create(
            "F",
            DeclarationKind::Function,
            "package store\nfunc F() {}"
        ))
        .contains("package clause"));
        assert!(refused(unit_create(
            "F",
            DeclarationKind::Function,
            "import \"fmt\"\nfunc F() {}"
        ))
        .contains("import"));
        // A path-shaped unit is refused.
        let mut op = unit_create("F", DeclarationKind::Function, "func F() {}");
        let Some(crate::McpMutationPayload::EntityCreate(create)) = op.payload.as_mut() else {
            unreachable!()
        };
        create.unit = Some(SourceUnit::go(
            "internal/store/store.go",
            "store",
            crate::source_unit::UnitRole::Source,
        ));
        assert!(refused(op).contains("file"));
        // Both forms at once, or neither, is refused.
        let mut op = unit_create("F", DeclarationKind::Function, "func F() {}");
        let Some(crate::McpMutationPayload::EntityCreate(create)) = op.payload.as_mut() else {
            unreachable!()
        };
        create.source_base = Some(base());
        assert!(refused(op).contains("two forms"));
        // An unknown kind and a smuggled path field fail to decode.
        let mut bad = wire(&unit_create("F", DeclarationKind::Function, "func F() {}"));
        bad["payload"]["EntityCreate"]["kind"] = "module".into();
        assert!(crate::session::parse_staged_operations(&serde_json::json!([bad])).is_err());
        let mut bad = wire(&unit_create("F", DeclarationKind::Function, "func F() {}"));
        bad["payload"]["EntityCreate"]["path"] = "main.go".into();
        assert!(crate::session::parse_staged_operations(&serde_json::json!([bad])).is_err());
        // Anchored creation stays function-only and import-free.
        let mut create = anchored("fn added() {}");
        create.kind = DeclarationKind::Struct;
        assert!(create
            .validate()
            .unwrap_err()
            .contains("addressed to a unit"));
        // A method and a class are anchored kinds too, with a placement of
        // their own for a class named as the container.
        for (kind, placement) in [
            (DeclarationKind::Method, EntityPlacement::SiblingAfter),
            (DeclarationKind::Method, EntityPlacement::Member),
            (DeclarationKind::Class, EntityPlacement::SiblingAfter),
        ] {
            let mut create = anchored("def added(self):\n    pass");
            create.kind = kind;
            create.placement = Some(placement);
            create.validate().unwrap();
        }
        assert_eq!(
            serde_json::to_value(EntityPlacement::Member).unwrap(),
            "member"
        );
        let mut create = anchored("fn added() {}");
        create.imports = vec![ImportSpec::Path("std".into())];
        assert!(create.validate().unwrap_err().contains("imports"));
    }

    #[test]
    fn unit_imports_are_closed_and_refuse_ambiguous_changes() {
        let unit = SourceUnit::go(".", "main", crate::source_unit::UnitRole::Source);
        let imports = |add: Vec<&str>, remove: Vec<&str>| crate::source_unit::UnitImports {
            repository_base: repository_base(),
            unit: unit.clone(),
            add: add
                .into_iter()
                .map(|p| ImportSpec::Path(p.into()))
                .collect(),
            remove: remove
                .into_iter()
                .map(|p| ImportSpec::Path(p.into()))
                .collect(),
        };
        let op = |payload: crate::source_unit::UnitImports| crate::McpMutationOperation {
            verb: "update".into(),
            target: "main".into(),
            payload: Some(crate::McpMutationPayload::UnitImports(payload)),
            body: None,
            destination: None,
            description: "manage imports".into(),
        };
        crate::session::validate_semantic_operations(&[op(imports(vec!["fmt"], vec!["os"]))])
            .unwrap();
        for bad in [
            imports(vec![], vec![]),
            imports(vec!["fmt"], vec!["fmt"]),
            imports(vec!["fmt", "fmt"], vec![]),
            imports(vec!["../x"], vec![]),
            imports(vec!["\"fmt\""], vec![]),
        ] {
            assert!(crate::session::validate_semantic_operations(&[op(bad)]).is_err());
        }
        let mut wrong_target = op(imports(vec!["fmt"], vec![]));
        wrong_target.target = "main.go".into();
        assert!(crate::session::validate_semantic_operations(&[wrong_target]).is_err());
        let mut body = op(imports(vec!["fmt"], vec![]));
        body.body = Some("import \"fmt\"".into());
        assert!(crate::session::validate_semantic_operations(&[body]).is_err());
    }

    #[test]
    fn entity_lifecycle_shapes_are_closed_and_source_bound() {
        let create = anchored("fn added() {}");
        let anchor = create.source_base.clone().unwrap();
        let mut operation = crate::McpMutationOperation {
            verb: "create".into(),
            target: anchor.entity_id.to_string(),
            payload: Some(crate::McpMutationPayload::EntityCreate(create.clone())),
            body: None,
            destination: None,
            description: "one declaration".into(),
        };
        crate::session::validate_semantic_operations(&[operation.clone()]).unwrap();
        operation.target = "src/lib.rs".into();
        assert!(crate::session::validate_semantic_operations(&[operation.clone()]).is_err());
        operation.target = anchor.entity_id.to_string();
        for forbidden in ["body", "destination"] {
            let mut value = serde_json::to_value(&operation).unwrap();
            value.as_object_mut().unwrap().remove("body");
            value.as_object_mut().unwrap().remove("destination");
            value[forbidden] = serde_json::Value::Null;
            assert!(crate::session::parse_staged_operations(&serde_json::json!([value])).is_err());
        }
        let mut value = serde_json::to_value(&create).unwrap();
        value["path"] = "evil.py".into();
        assert!(serde_json::from_value::<EntityCreate>(value).is_err());
        operation.verb = "remove".into();
        operation.payload = Some(crate::McpMutationPayload::EntityRemove(EntityRemove {
            source_base: anchor,
        }));
        crate::session::validate_semantic_operations(&[operation.clone()]).unwrap();
        operation.body = Some("unrequested source".into());
        assert!(crate::session::validate_semantic_operations(&[operation]).is_err());
    }

    #[test]
    fn entity_lifecycle_creation_retains_truncated_body_refusal() {
        let mut create = anchored("def added():\n    pass # ... [truncated]");
        let op = |create: EntityCreate| crate::McpMutationOperation {
            verb: "create".into(),
            target: create.expected_target().unwrap(),
            payload: Some(crate::McpMutationPayload::EntityCreate(create)),
            body: None,
            destination: None,
            description: "one declaration".into(),
        };
        assert!(crate::handlers::sessions::reject_truncated_bodies(&[op(create.clone())]).is_err());
        create.body = "def added():\n    return '[truncated]'".into();
        crate::handlers::sessions::reject_truncated_bodies(&[op(create)]).unwrap();
    }

    #[test]
    fn entity_lifecycle_projection_names_cannot_supply_paths() {
        let origin = kin_model::FilePathId::new("pkg/source.py");
        assert_eq!(
            generated_source_path(&origin, kin_model::LanguageId::Python, "added")
                .unwrap()
                .to_string(),
            "pkg/added.py"
        );
        for name in ["../escape", "foo/bar", "", "a.py", "a\\b"] {
            assert!(generated_source_path(&origin, kin_model::LanguageId::Python, name).is_err());
        }
        // A unit beside a TypeScript or JavaScript one takes its owner's
        // extension; a language without a lifecycle creates no unit.
        for (owner, language, expected) in [
            (
                "src/Button.tsx",
                kin_model::LanguageId::TypeScript,
                "src/added.tsx",
            ),
            (
                "src/run.ts",
                kin_model::LanguageId::TypeScript,
                "src/added.ts",
            ),
            (
                "lib/run.mjs",
                kin_model::LanguageId::JavaScript,
                "lib/added.mjs",
            ),
            ("pkg/run.go", kin_model::LanguageId::Go, "pkg/added.go"),
            ("src/lib.rs", kin_model::LanguageId::Rust, "src/added.rs"),
        ] {
            assert_eq!(
                generated_source_path(&kin_model::FilePathId::new(owner), language, "added")
                    .unwrap()
                    .to_string(),
                expected
            );
        }
        assert!(generated_source_path(
            &kin_model::FilePathId::new("src/Run.java"),
            kin_model::LanguageId::Java,
            "added"
        )
        .is_err());
    }

    /// The languages a lifecycle sentence names come from the registry of
    /// adapter hooks, in registration order, never from a list here.
    #[test]
    fn lifecycle_text_names_the_languages_whose_adapters_implement_each_form() {
        let create = entity_create_schema()["description"]
            .as_str()
            .unwrap()
            .to_string();
        for said in [
            "Addressed to a unit (repository_base and unit), in typescript, javascript, \
             python, go, rust:",
            "One top-level function in TypeScript, JavaScript, Python, Go or Rust beside a \
             current function.",
            "In TypeScript, JavaScript, Python, Go or Rust also a method",
        ] {
            assert!(create.contains(said), "{said}: {create}");
        }
        let remove = entity_remove_schema()["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            remove.contains(
                "top-level leaf function in TypeScript, JavaScript, Python, Go or Rust, with its \
                 doc comment, or one method of a class in TypeScript, JavaScript, Python, Go or \
                 Rust"
            ),
            "{remove}"
        );
        let mut create = anchored("fn added() {}");
        create.kind = DeclarationKind::Struct;
        assert_eq!(
            create.validate().unwrap_err(),
            "anchored EntityCreate creates a top-level function in TypeScript, JavaScript, \
             Python, Go and Rust, and a method or a class in TypeScript, JavaScript, Python, Go \
             and Rust; create other declaration kinds addressed to a unit, with repository_base \
             and unit"
        );
        assert!(crate::session::ACCEPTED_OPERATION_SHAPES
            .contains(r#""language": "typescript" | "javascript" | "python" | "go" | "rust","#));
    }
}
