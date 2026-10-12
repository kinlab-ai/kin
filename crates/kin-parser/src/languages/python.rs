// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{EntityKind, FilePathId, LanguageId, ParseState, Visibility};
use tree_sitter::Tree;

use crate::adapter::{
    collect_error_ranges, compute_fingerprint, make_parser, site_from_node, span_from_node,
    LanguageAdapter,
};
use crate::error::Result;
use crate::extract::{
    call_extraction_incomplete_marker, scoped_call_extraction_incomplete_marker, CallArgShape,
    ExtractedEntity, ExtractedRelation, ExtractedTest, ExtractedTestKind, FileImport, ImportedName,
    ParseOutput, RelationSite,
};
use crate::nested::{
    is_local_scope_name, local_scope_name, LocalScopeNames, NestedDefinitionCensus,
    UnadmittedNestedDefinition,
};

#[path = "python_fields.rs"]
mod fields;
#[path = "python_guards.rs"]
mod guards;
#[path = "python_member_values.rs"]
mod member_values;

/// Every public name CPython's `builtins` module binds, sorted so
/// [`is_python_builtin_name`] can binary-search it.
///
/// Read off `sorted(n for n in dir(builtins) if not n.startswith("_"))` on
/// CPython 3.13, plus `WindowsError`, which the language reference documents as
/// a built-in exception that only a Windows interpreter binds. Transcribing a
/// remembered dozen would leave the rest of the table resolving by name, and the
/// names left out are not the obvious ones: `format`, `id`, `filter`, `next` and
/// `exit` all read like ordinary repository functions.
///
/// This is a complete list rather than a heuristic because Python makes it one.
/// A module-level name is reachable inside a module only when that module
/// defines it or imports it, so a bare call this file does neither for is a call
/// into the interpreter, whatever a same-named symbol elsewhere in the
/// repository looks like.
pub const PYTHON_BUILTIN_NAMES: &[&str] = &[
    "ArithmeticError",
    "AssertionError",
    "AttributeError",
    "BaseException",
    "BaseExceptionGroup",
    "BlockingIOError",
    "BrokenPipeError",
    "BufferError",
    "BytesWarning",
    "ChildProcessError",
    "ConnectionAbortedError",
    "ConnectionError",
    "ConnectionRefusedError",
    "ConnectionResetError",
    "DeprecationWarning",
    "EOFError",
    "Ellipsis",
    "EncodingWarning",
    "EnvironmentError",
    "Exception",
    "ExceptionGroup",
    "False",
    "FileExistsError",
    "FileNotFoundError",
    "FloatingPointError",
    "FutureWarning",
    "GeneratorExit",
    "IOError",
    "ImportError",
    "ImportWarning",
    "IndentationError",
    "IndexError",
    "InterruptedError",
    "IsADirectoryError",
    "KeyError",
    "KeyboardInterrupt",
    "LookupError",
    "MemoryError",
    "ModuleNotFoundError",
    "NameError",
    "None",
    "NotADirectoryError",
    "NotImplemented",
    "NotImplementedError",
    "OSError",
    "OverflowError",
    "PendingDeprecationWarning",
    "PermissionError",
    "ProcessLookupError",
    "PythonFinalizationError",
    "RecursionError",
    "ReferenceError",
    "ResourceWarning",
    "RuntimeError",
    "RuntimeWarning",
    "StopAsyncIteration",
    "StopIteration",
    "SyntaxError",
    "SyntaxWarning",
    "SystemError",
    "SystemExit",
    "TabError",
    "TimeoutError",
    "True",
    "TypeError",
    "UnboundLocalError",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
    "UnicodeError",
    "UnicodeTranslateError",
    "UnicodeWarning",
    "UserWarning",
    "ValueError",
    "Warning",
    "WindowsError",
    "ZeroDivisionError",
    "abs",
    "aiter",
    "all",
    "anext",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "copyright",
    "credits",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "exit",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "license",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "quit",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

/// Whether a bare Python name is bound by the interpreter itself.
pub fn is_python_builtin_name(name: &str) -> bool {
    PYTHON_BUILTIN_NAMES.binary_search(&name).is_ok()
}

pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    fn language_id(&self) -> LanguageId {
        LanguageId::Python
    }

    fn declaration_lifecycle(
        &self,
    ) -> Option<&dyn crate::declaration_lifecycle::DeclarationLifecycle> {
        Some(&super::python_edit::PythonLifecycle)
    }

    fn member_placement(&self) -> Option<&dyn crate::member_placement::MemberPlacement> {
        Some(&super::python_edit::PythonPlacement)
    }

    fn unit_editor(&self) -> Option<&dyn crate::unit_editor::UnitEditor> {
        Some(&super::python_edit::PYTHON_UNITS)
    }

    fn layout_problems(
        &self,
        root: &tree_sitter::Node,
        source: &[u8],
        out: &mut Vec<crate::syntax_reading::SyntaxProblem>,
    ) {
        super::python_layout::python_layout_problems(root, source, out);
    }

    fn is_docstring(&self, node: tree_sitter::Node) -> bool {
        super::python_layout::is_python_docstring(node)
    }

    /// An `@overload` stub. `typing.overload` declares one signature of a
    /// function whose implementation follows it under the same name, and the
    /// stub's own body never runs. Extraction writes each decorator's
    /// trailing identifier at the head of the signature, so `@t.overload` and
    /// `@typing.overload` both read `@overload` there.
    fn declares_only(&self, kind: EntityKind, signature: &str) -> bool {
        matches!(kind, EntityKind::Function | EntityKind::Method)
            && crate::declaration_only::leading_decorators(signature)
                .any(|decorator| decorator == "overload")
    }

    fn site_syntax(&self) -> Option<crate::site_syntax::SiteSyntax> {
        Some(crate::site_syntax::PYTHON)
    }

    fn body_syntax(&self) -> Option<crate::body_syntax::BodySyntax> {
        Some(crate::body_syntax::PYTHON)
    }

    fn name_syntax(&self) -> Option<crate::name_syntax::NameSyntax> {
        Some(crate::name_syntax::PYTHON)
    }

    fn file_extensions(&self) -> &[&str] {
        &["py", "pyi"]
    }

    fn project_manifest(&self) -> Option<crate::project_manifest::ProjectManifest> {
        Some(crate::project_manifest::PYPROJECT)
    }

    fn declaration_census(
        &self,
        tree: &Tree,
        source: &[u8],
    ) -> Option<Vec<crate::declaration_census::SeenDeclaration>> {
        Some(super::python_declarations::python_declaration_census(
            tree, source,
        ))
    }

    fn literal_members<'tree>(
        &self,
        literal: tree_sitter::Node<'tree>,
        source: &[u8],
    ) -> Vec<crate::literal_members::LiteralMember<'tree>> {
        fields::literal_members(literal, source)
    }

    fn import_specifier_root(
        &self,
        module_path: &str,
    ) -> crate::import_origin::ImportSpecifierRoot {
        crate::import_origin::python_specifier_root(module_path)
    }

    fn parse(&self, source: &[u8]) -> Result<Tree> {
        let mut parser = make_parser(&tree_sitter_python::LANGUAGE)?;
        parser
            .parse(source, None)
            .ok_or_else(|| crate::error::ParseError::ParseFailed {
                file: String::new(),
                reason: "tree-sitter returned None".into(),
            })
    }

    fn extract(&self, tree: &Tree, source: &[u8], file_id: &FilePathId) -> Result<ParseOutput> {
        let error_ranges = collect_error_ranges(tree);
        let parse_state = if error_ranges.is_empty() {
            ParseState::Valid
        } else {
            ParseState::Incomplete { error_ranges }
        };

        let mut imports = Vec::new();
        // Names a module-scope guard binds. Not `FileImport`s: see
        // [`collect_python_guarded_imports`] for why admitting them there cost
        // measured recall on a real repository.
        let mut guarded_imports: Vec<FileImport> = Vec::new();
        let mut out = PyExtraction::default();
        let root = tree.root_node();
        out.module_names = guards::ModuleNames::of(root, source);
        let mut cursor = root.walk();

        // In Python, each `.py` file IS a module (PEP 328); `__init__.py`
        // additionally marks its containing directory as a *package*. The name
        // is resolved before the walk because module-scope statements are
        // sourced to the module entity.
        let (module_name, is_package) = python_module_identity(file_id);
        let module_scope = PyScope {
            enclosing: (!module_name.is_empty()).then(|| module_name.clone()),
            ..PyScope::default()
        };
        prepare_python_block_class_collisions(&root, source, &module_scope, &mut out);
        prepare_python_conditional_ambiguities(&root, source, &module_scope, &mut out);

        for child in root.children(&mut cursor) {
            extract_py_node(&child, source, file_id, &module_scope, &mut out);
            let nested = admit_python_statement_definitions(
                &child,
                source,
                file_id,
                &module_scope,
                &mut out,
            );
            // Extract imports at top level, plus the guarded ones that only
            // bind names for annotations.
            match child.kind() {
                "import_statement" => {
                    imports.extend(extract_py_imports(&child, source));
                }
                "import_from_statement" => {
                    if let Some(import) = extract_py_from_import(&child, source) {
                        imports.push(import);
                    }
                }
                "function_definition" | "class_definition" | "decorated_definition" => {}
                _ => {
                    // Module-scope statements. `HANDLERS = {"ingest": cmd_ingest}`
                    // at module level names its target exactly as a body statement
                    // would, so the module entity sources those references.
                    collect_python_guarded_imports(&child, source, &mut guarded_imports);
                    if !module_name.is_empty() {
                        collect_python_value_refs(
                            &child,
                            source,
                            &module_name,
                            &PyRefScope::within(&[], Some(&nested)),
                            &mut out.value_refs,
                        );
                    }
                }
            }
        }

        // Keep the full-file census and its broad completeness meaning. Scoped
        // negative records below distinguish a known but unresolved callee from
        // syntax that extraction could not name; neither makes the file complete.
        let mut census = PythonCallSiteCensus::default();
        census_python_call_sites(&root, source, &mut census);

        // Build import lookup: local_name -> module_path
        let import_map: std::collections::HashMap<&str, &str> = imports
            .iter()
            .chain(guarded_imports.iter())
            .flat_map(|imp| {
                imp.specifiers
                    .iter()
                    .map(move |spec| (spec.local_name.as_str(), imp.module_path.as_str()))
            })
            .collect();

        // Emit a Module entity for every Python source file. Previously only
        // package init files produced Module entities, leaving regular modules
        // without a node, which broke per-file graph queries on Python repos.
        if !module_name.is_empty() {
            out.entities.push(ExtractedEntity {
                kind: EntityKind::Module,
                name: module_name,
                signature: if is_package {
                    format!("package {}", file_id.0)
                } else {
                    format!("module {}", file_id.0)
                },
                visibility: Visibility::Public,
                doc_summary: extract_module_docstring(&root, source),
                fingerprint: compute_fingerprint(&root, source),
                span: span_from_node(&root, file_id),
                declaration_line: None,
            });
        }

        // Calls no declaration walk reached (module-scope code, a lambda's
        // body there) go to the innermost entity holding each one, the
        // module for module scope, which is where module-scope references are
        // already sourced. The walk runs with no enclosing frame, so a name
        // resolves the way module scope binds it. Records the walk leaves on a
        // nested scope rather than on a call are not calls and stay out; the
        // calls it adopts count as walked, so the census below reads a named
        // one as carried and an unnamed one as not nameable.
        {
            let mut accounted = crate::call_owner::accounted_call_sites(&out.relations);
            accounted.extend(out.call_audit.seen_calls.iter().copied());
            super::python_declarations::carry_decorator_call_sites(&root, &mut accounted);
            // A function whose calls are all accounted for is not read twice.
            let mut walked = walked_python_functions(&root, &out.entities, &accounted);
            walked.extend(out.ambiguous_definitions.iter().copied());
            let mut swept = Vec::new();
            let mut swept_audit = PythonCallExtractionAudit::default();
            extract_calls_from_context(
                &root,
                source,
                &PyCallScope {
                    context_name: "",
                    self_class: None,
                    receiver_types: &PythonReceiverTypes::new(),
                    frames: &[],
                    skip: &walked,
                    module_names: &out.module_names,
                },
                &mut swept,
                &mut swept_audit,
            );
            swept.retain(|relation| {
                relation.site.as_ref().is_some_and(|site| {
                    swept_audit
                        .seen_calls
                        .contains(&crate::call_owner::site_key(site))
                })
            });
            let sites: Vec<_> = swept
                .iter()
                .filter_map(|relation| relation.site.as_ref().map(crate::call_owner::site_key))
                .collect();
            for index in crate::call_owner::adopt_unowned_calls(
                &out.entities,
                &accounted,
                swept,
                &mut out.relations,
            ) {
                out.call_audit.seen_calls.insert(sites[index]);
            }
        }

        // Calls outside the callable walk still count as gaps. Assign them only
        // to a unique innermost declaration. Ambiguous ownership remains global.
        for site in &census.sites {
            if out
                .call_audit
                .seen_calls
                .contains(&(site.start_byte, site.end_byte))
            {
                continue;
            }
            let mut owners: Vec<_> = out
                .entities
                .iter()
                .filter(|entity| {
                    entity.span.start_byte <= site.start_byte
                        && site.end_byte <= entity.span.end_byte
                })
                .collect();
            owners.sort_by_key(|entity| entity.span.end_byte - entity.span.start_byte);
            let owner = owners.first().filter(|first| {
                owners.get(1).is_none_or(|second| {
                    first.span.end_byte - first.span.start_byte
                        < second.span.end_byte - second.span.start_byte
                })
            });
            out.relations.push(match owner {
                Some(owner) => {
                    scoped_call_extraction_incomplete_marker(owner.name.clone(), site.clone(), None)
                }
                None => call_extraction_incomplete_marker(),
            });
        }

        // An ambiguous conditional binding is not the first declaration, nor
        // a same-named import. Preserve its call site as negative evidence.
        // Explicit function-local imports already carry their own binding and
        // are independent of a module-level collision.
        for relation in &mut out.relations {
            if relation.kind == kin_model::RelationKind::Calls
                && relation.import_source.is_none()
                && ((relation.receiver.is_none()
                    && out.ambiguous_names.contains(&relation.dst_name))
                    || (relation.receiver.is_none()
                        && out
                            .ambiguous_class_bindings
                            .get(&relation.src_name)
                            .is_some_and(|bindings| bindings.contains(&relation.dst_name)))
                    || out
                        .ambiguous_names
                        .iter()
                        .any(|name| relation.dst_name.starts_with(&format!("{name}."))))
            {
                if let Some(site) = relation.site.clone() {
                    *relation = scoped_call_extraction_incomplete_marker(
                        relation.src_name.clone(),
                        site,
                        Some(relation.dst_name.clone()),
                    );
                }
            }
        }
        out.value_refs.retain(|reference| {
            !out.ambiguous_names.contains(&reference.root)
                && !out
                    .ambiguous_class_bindings
                    .get(&reference.src_name)
                    .is_some_and(|bindings| bindings.contains(&reference.root))
        });
        // The lexical walks above resolve names once in their actual Python
        // scope. Move each owned occurrence to its innermost field after all
        // declarations are known, without walking the initializer twice.
        fields::assign_field_sites(&mut out);
        emit_python_value_references(
            std::mem::take(&mut out.value_refs),
            &out.entities,
            &imports,
            &guarded_imports,
            &mut out.relations,
        );

        // Annotate Calls/References relations with import_source.
        //
        // A receiver-bearing CALL is skipped: its `dst_name` is an attribute
        // read off some object, not the local binding an import introduced, so
        // matching it against the import map pins `obj.get(...)` to whatever
        // module happened to export a name called `get`. A receiver on a
        // `References` edge means something else entirely, the class attribute
        // whose type the edge declares, and its `dst_name` IS the local binding
        // the import introduced, so skipping it there dropped the pin that
        // tells the linker which module the annotated type came from.
        for rel in &mut out.relations {
            if rel.kind == kin_model::RelationKind::Calls && rel.receiver.is_some() {
                continue;
            }
            // A call already carrying the module an enclosing function body
            // imported its name from keeps it: that local import, not the
            // file's own, is the binding the call reads.
            if rel.import_source.is_some() {
                continue;
            }
            if matches!(
                rel.kind,
                kin_model::RelationKind::Calls | kin_model::RelationKind::References
            ) {
                if let Some(&module) = import_map.get(rel.dst_name.as_str()) {
                    rel.import_source = Some(module.to_string());
                }
            }
        }

        // Detect test functions (pytest: def test_*). Only a module-level
        // function is one: pytest collects `test_*` from a module's namespace
        // and a class's, never from inside a function body, so a nested
        // `test_*` closure, or anything nested in a test and so named
        // `test_x.<locals>...`, is not a test the runner will ever call.
        let mut tests = Vec::new();
        for ent in &out.entities {
            if ent.kind == EntityKind::Function
                && ent.name.starts_with("test_")
                && !is_local_scope_name(&ent.name)
            {
                tests.push(ExtractedTest {
                    name: ent.name.clone(),
                    kind: ExtractedTestKind::Unit,
                    runner: "pytest".to_string(),
                });
            }
        }

        crate::import_witness::append_python_import_witness(
            tree,
            source,
            file_id,
            &imports,
            &mut out.relations,
        );
        super::python_declarations::push_call_extraction_gaps(
            tree,
            census.total(),
            &census.sites,
            &out.call_audit.seen_calls,
            &out.entities,
            &mut out.relations,
        );
        Ok(ParseOutput {
            derived_members: Vec::new(),
            entities: out.entities,
            relations: out.relations,
            imports,
            tests,
            parse_state,
            parsed_call_sites: Some(census.total()),
            nested_definitions: Some(out.census),
        })
    }
}

/// Everything one Python file's walk accumulates.
#[derive(Default)]
struct PyExtraction {
    entities: Vec<ExtractedEntity>,
    relations: Vec<ExtractedRelation>,
    call_audit: PythonCallExtractionAudit,
    value_refs: Vec<PythonValueRef>,
    /// Definitions found in a position the walk does not admit.
    census: NestedDefinitionCensus,
    /// Local-scope names already admitted, so a second definition under one
    /// name becomes a census entry rather than a duplicate entity.
    local_names: LocalScopeNames,
    /// Every declaration in a conditional same-binding collision, including
    /// unconditional declarations and imported bindings it can replace.
    ambiguous_definitions: std::collections::HashSet<usize>,
    ambiguous_names: std::collections::HashSet<String>,
    ambiguous_class_bindings: std::collections::HashMap<String, std::collections::HashSet<String>>,
    /// How the module binds its names, read once before the walk.
    module_names: guards::ModuleNames,
    /// The names each class binds as data, by class node, read once per class.
    class_data: std::collections::HashMap<usize, std::collections::HashSet<String>>,
}

/// Where in the file's lexical structure a node sits.
///
/// Python gives a definition a different name and a different reach depending
/// on what holds it. A `def` in a class body is a method, one in a function
/// body is a local of that function, and a class body is not a scope that the
/// functions inside it can see. The walk carries all three facts together so
/// every arm names and binds a definition the same way.
#[derive(Clone, Default)]
struct PyScope {
    /// The class whose body directly holds the node, which makes a `def` there
    /// a method named `Class.method`.
    class_ctx: Option<String>,
    /// The function scopes enclosing the node, outermost first. A class body
    /// adds none: its methods see the enclosing functions, not the class.
    frames: Vec<std::rc::Rc<PyFrame>>,
    /// The class `self` and `cls` name at this point. A closure inside a
    /// method keeps its method's class, because `self` is the same object
    /// there; a method of a class nested in that closure names its own.
    self_class: Option<String>,
    /// The innermost admitted entity holding the node, which a definition the
    /// walk records but does not admit is listed under.
    enclosing: Option<String>,
}

/// One function scope, as the definitions and names inside it see it.
struct PyFrame {
    /// The entity name of the function that opens this scope.
    entity: String,
    /// Every name this scope binds itself, from its parameters and its own
    /// statements, not counting names bound only inside a nested scope. A name
    /// in this set stops a lookup here, because Python makes a name bound
    /// anywhere in a function body local to the whole body.
    bound: std::collections::HashSet<String>,
    /// Names this scope declares `global`, which skip every enclosing function
    /// scope and reach the module.
    globals: std::collections::HashSet<String>,
    /// Names this scope binds only through a plain `from module import name`,
    /// each mapped to the module as written. A name bound here in any other
    /// way as well is a value this scope assigns, and is left out.
    imports: std::collections::HashMap<String, String>,
    /// The `def` and `class` statements of this scope, by the name they bind,
    /// each mapped to the entity name it is admitted under. A redefinition
    /// maps to the first, which is the one admitted.
    definitions: std::collections::HashMap<String, String>,
    /// The shadow set the reference walk has always applied to this
    /// function: every name bound anywhere in it, nested scopes included.
    shadowed: std::collections::HashSet<String>,
    /// The receiver types a call written in this scope dispatches through,
    /// the enclosing scopes' included, since a closure reads its enclosing
    /// function's annotated parameters exactly as its own.
    receiver_types: PythonReceiverTypes,
}

impl PyFrame {
    fn for_function(
        node: &tree_sitter::Node,
        source: &[u8],
        entity: &str,
        parent: Option<&PyFrame>,
    ) -> Self {
        let (bound, globals, imports) = collect_python_frame_bindings(node, source);
        let mut definitions = std::collections::HashMap::new();
        if let Some(body) = node.child_by_field_name("body") {
            for_each_python_definition(&body, &mut |definition| {
                if let Some(raw) = python_definition_raw_name(&definition, source) {
                    definitions
                        .entry(raw.clone())
                        .or_insert_with(|| local_scope_name(entity, &raw));
                }
            });
        }
        // A name this scope binds again is its own, so an enclosing scope's
        // declaration of it no longer describes it. That covers a method's
        // `self`, which names the method's own class rather than whatever an
        // enclosing method's `self` was.
        let mut receiver_types: PythonReceiverTypes = parent
            .map(|parent| {
                parent
                    .receiver_types
                    .iter()
                    .filter(|(receiver, _)| {
                        let root = receiver.split('.').next().unwrap_or(receiver);
                        !bound.contains(root)
                    })
                    .map(|(receiver, owner)| (receiver.clone(), owner.clone()))
                    .collect()
            })
            .unwrap_or_default();
        receiver_types.extend(python_receiver_types(node, source));
        PyFrame {
            entity: entity.to_string(),
            bound,
            globals,
            imports,
            definitions,
            shadowed: collect_python_local_bindings(node, source),
            receiver_types,
        }
    }
}

/// The entity name a bare `name` read in the innermost of `frames` refers to,
/// when it is a definition nested in one of those function scopes.
///
/// Python resolves a name in the innermost function scope that binds it, then
/// outward through the enclosing function scopes, then the module. The walk
/// stops at the first scope that binds the name at all: a local variable
/// called `index` hides an enclosing function's nested `def index`, and so
/// does a `global index`, which sends the name straight to the module. Only a
/// name bound by a nested `def` or `class` is rewritten; every other name is
/// left exactly as the source spells it.
fn resolve_python_local_definition<'a>(
    frames: &'a [std::rc::Rc<PyFrame>],
    name: &str,
) -> Option<&'a str> {
    match resolve_python_scoped_name(frames, name)? {
        PythonScopedName::Definition(entity) => Some(entity),
        PythonScopedName::Imported(_) | PythonScopedName::Value => None,
    }
}

/// What an enclosing function scope binds a bare name to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PythonScopedName<'a> {
    /// A `def` or `class` the scope nests, by its entity name.
    Definition(&'a str),
    /// A plain `from module import name` in the scope's body, by the module
    /// as written.
    Imported(&'a str),
    /// Any other binding: a parameter, an assignment, a loop, `with` or
    /// `except` target, an aliased import or an `import module`. A call
    /// through it reaches whatever value the name holds when it runs, never a
    /// module-level declaration that happens to share the name.
    Value,
}

/// The binding a bare `name` read in the innermost of `frames` resolves to,
/// following Python's scoping rule for function bodies.
///
/// The innermost function scope that binds the name decides, then each
/// enclosing function scope outward. A `global` declaration sends the name
/// straight to the module. `None` means no function scope binds the name, so
/// it is a module global or a builtin, which the linker settles against the
/// module's own declarations, its imports and the builtin table.
fn resolve_python_scoped_name<'a>(
    frames: &'a [std::rc::Rc<PyFrame>],
    name: &str,
) -> Option<PythonScopedName<'a>> {
    for frame in frames.iter().rev() {
        if frame.globals.contains(name) {
            return None;
        }
        if let Some(entity) = frame.definitions.get(name) {
            return Some(PythonScopedName::Definition(entity.as_str()));
        }
        if frame.bound.contains(name) {
            return Some(match frame.imports.get(name) {
                Some(module) => PythonScopedName::Imported(module.as_str()),
                None => PythonScopedName::Value,
            });
        }
    }
    None
}

/// The local-scope name of whatever an enclosing function scope binds `name`
/// to, for a class's base list.
///
/// A nested `def` or `class` resolves to its entity, exactly as
/// [`resolve_python_local_definition`] does. A plain local, such as the
/// parameter in `def t(base): class C(base): ...`, is a runtime value rather
/// than a class anything declares, and is spelled `t.<locals>.base`. Left
/// bare, the linker would read `base` as a module-level or repository-wide
/// class name and derive a hierarchy through an unrelated class that happens
/// to share it; spelled this way it names no entity, so the base resolves to
/// nothing and is disclosed as unresolved.
fn python_local_binding_name(frames: &[std::rc::Rc<PyFrame>], name: &str) -> Option<String> {
    for frame in frames.iter().rev() {
        if frame.globals.contains(name) {
            return None;
        }
        if let Some(entity) = frame.definitions.get(name) {
            return Some(entity.clone());
        }
        if frame.bound.contains(name) {
            return Some(local_scope_name(&frame.entity, name));
        }
    }
    None
}

/// What a reference walk needs to decide which name a read binds to.
struct PyRefScope<'a> {
    /// The enclosing function scopes, whose nested definitions a bare read
    /// resolves to.
    frames: &'a [std::rc::Rc<PyFrame>],
    /// Names a read here must not bind to a module-level entity.
    shadowed: std::collections::HashSet<String>,
    /// Nested definitions the walk must leave to their own entities.
    skip: Option<&'a std::collections::HashSet<usize>>,
}

impl<'a> PyRefScope<'a> {
    /// Inside `frames`, with the nested definitions in `skip` left out.
    fn within(
        frames: &'a [std::rc::Rc<PyFrame>],
        skip: Option<&'a std::collections::HashSet<usize>>,
    ) -> Self {
        PyRefScope {
            frames,
            shadowed: frames
                .iter()
                .flat_map(|frame| frame.shadowed.iter().cloned())
                .collect(),
            skip,
        }
    }

    /// The name a read of `name` records: the nested definition's entity name
    /// when it is one, the name itself when it may be module-level, and
    /// `None` when a local binding hides it.
    fn root_for(&self, name: &str) -> Option<String> {
        if let Some(entity) = resolve_python_local_definition(self.frames, name) {
            return Some(entity.to_string());
        }
        (!self.shadowed.contains(name)).then(|| name.to_string())
    }

    fn skips(&self, node: &tree_sitter::Node) -> bool {
        self.skip.is_some_and(|skip| skip.contains(&node.id()))
    }
}

/// What one function body's call walk needs besides the syntax.
struct PyCallScope<'a> {
    /// The entity the calls are attributed to.
    context_name: &'a str,
    /// The class `self` and `cls` name here.
    self_class: Option<&'a str>,
    receiver_types: &'a PythonReceiverTypes,
    frames: &'a [std::rc::Rc<PyFrame>],
    /// Nested definitions whose calls belong to their own entities.
    skip: &'a std::collections::HashSet<usize>,
    /// How the module binds its names, which decides whether an
    /// `isinstance` guard is the builtin and whether its classes are grounded.
    module_names: &'a guards::ModuleNames,
}

/// The name a `def` or `class` statement binds, looking through a decorator
/// wrapper.
fn python_definition_raw_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let definition = python_unwrap_decorated(node)?;
    let name = definition
        .child_by_field_name("name")?
        .utf8_text(source)
        .ok()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The `function_definition` or `class_definition` a statement declares, with
/// a `decorated_definition` wrapper looked through.
fn python_unwrap_decorated<'tree>(
    node: &tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    match node.kind() {
        "function_definition" | "class_definition" => Some(*node),
        "decorated_definition" => node.child_by_field_name("definition").or_else(|| {
            let mut cursor = node.walk();
            let found = node
                .named_children(&mut cursor)
                .find(|child| matches!(child.kind(), "function_definition" | "class_definition"));
            found
        }),
        _ => None,
    }
}

/// Visit every definition statement under `node` that belongs to the scope
/// `node` is part of: through `if`, `for`, `while`, `with`, `try` and `match`
/// blocks, but not into another definition, whose own body is its own scope,
/// nor into a lambda, which holds no statements.
fn for_each_python_definition<'tree>(
    node: &tree_sitter::Node<'tree>,
    visit: &mut dyn FnMut(tree_sitter::Node<'tree>),
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" | "class_definition" | "decorated_definition" => visit(child),
            "lambda" => {}
            _ => for_each_python_definition(&child, visit),
        }
    }
}

/// The entity kind and name a definition gets where `scope` places it, and
/// the entity that `Contains` it, when one does.
///
/// A module-level definition keeps its bare name and a class-body `def` is
/// `Class.method`, exactly as before definitions nested in functions were
/// admitted. A definition in a function body is `F.<locals>.name`, and a class
/// nested in such a class keeps the qualified owner (`F.<locals>.C.D`), which
/// is CPython's `__qualname__` throughout. A class nested in a module-level
/// class keeps its bare name, which is what it has always been called.
fn python_definition_name(
    scope: &PyScope,
    raw: &str,
    is_class: bool,
) -> (EntityKind, String, Option<String>) {
    match (&scope.class_ctx, scope.frames.last()) {
        (Some(cls), _) if !is_class => (
            EntityKind::Method,
            format!("{cls}.{raw}"),
            Some(cls.clone()),
        ),
        (Some(cls), _) if is_local_scope_name(cls) => {
            (EntityKind::Class, format!("{cls}.{raw}"), Some(cls.clone()))
        }
        (Some(cls), _) => (EntityKind::Class, raw.to_string(), Some(cls.clone())),
        (None, Some(frame)) => (
            if is_class {
                EntityKind::Class
            } else {
                EntityKind::Function
            },
            local_scope_name(&frame.entity, raw),
            Some(frame.entity.clone()),
        ),
        (None, None) => (
            if is_class {
                EntityKind::Class
            } else {
                EntityKind::Function
            },
            raw.to_string(),
            scope.enclosing.clone(),
        ),
    }
}

/// The class a base-list entry names: `Base` for `Base[T]` and `mod.Base`
/// for `mod.Base[K, V]`, however deeply subscripted; any other entry as it is.
/// The one reading of a base both the recorded `Extends` relation and the
/// value-origin facts key a base by, so the two always name it alike.
pub(crate) fn python_base_class_node(mut node: tree_sitter::Node<'_>) -> tree_sitter::Node<'_> {
    while node.kind() == "subscript" {
        match node.child_by_field_name("value") {
            Some(value) => node = value,
            None => break,
        }
    }
    node
}

/// The base classes a class statement names, as written, keyword arguments
/// (`metaclass=...`) left out and type arguments (`Base[T]`) dropped.
fn python_class_bases_as_written(node: &tree_sitter::Node, source: &[u8]) -> Vec<String> {
    let Some(args) = node.child_by_field_name("superclasses") else {
        return Vec::new();
    };
    let mut cursor = args.walk();
    let bases = args
        .named_children(&mut cursor)
        .filter(|arg| arg.kind() != "keyword_argument" && arg.kind() != "comment")
        .map(python_base_class_node)
        .filter_map(|arg| arg.utf8_text(source).ok().map(str::to_string))
        .filter(|base| !base.is_empty())
        .collect();
    bases
}

/// Class names nested at module/class scope retain their historical bare
/// display name. Do not admit a new block class under a name another lexical
/// owner already holds; that would create an arbitrary linker slot. Function
/// locals already carry their qualified owner and cannot collide this way.
fn prepare_python_block_class_collisions(
    root: &tree_sitter::Node,
    source: &[u8],
    module_scope: &PyScope,
    out: &mut PyExtraction,
) {
    struct Candidate {
        nodes: [usize; 2],
        name: String,
        raw: String,
        owner: Option<String>,
        owner_scope: usize,
        class_owner: bool,
        guarded_class: bool,
    }
    fn walk(
        body: &tree_sitter::Node,
        source: &[u8],
        scope: &PyScope,
        candidates: &mut Vec<Candidate>,
    ) {
        for_each_python_definition(body, &mut |definition| {
            let Some(declaration) = python_unwrap_decorated(&definition) else {
                return;
            };
            let Some(raw) = python_definition_raw_name(&definition, source) else {
                return;
            };
            let is_class = declaration.kind() == "class_definition";
            let (_, name, _) = python_definition_name(scope, &raw, is_class);
            candidates.push(Candidate {
                nodes: [definition.id(), declaration.id()],
                name: name.clone(),
                raw,
                owner: scope.enclosing.clone(),
                owner_scope: body.id(),
                class_owner: scope.class_ctx.is_some(),
                guarded_class: is_class
                    && definition
                        .parent()
                        .is_some_and(|parent| parent.id() != body.id()),
            });
            if is_class {
                if let Some(body) = declaration.child_by_field_name("body") {
                    walk(
                        &body,
                        source,
                        &PyScope {
                            class_ctx: Some(name.clone()),
                            enclosing: Some(name),
                            ..scope.clone()
                        },
                        candidates,
                    );
                }
            }
        });
    }
    let mut candidates = Vec::new();
    walk(root, source, module_scope, &mut candidates);
    let mut owners: std::collections::HashMap<&str, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for candidate in &candidates {
        owners
            .entry(&candidate.name)
            .or_default()
            .insert(candidate.owner_scope);
    }
    for candidate in &candidates {
        if candidate.guarded_class
            && owners
                .get(candidate.name.as_str())
                .is_some_and(|owners| owners.len() > 1)
        {
            out.ambiguous_definitions.extend(candidate.nodes);
            if candidate.class_owner {
                if let Some(owner) = &candidate.owner {
                    out.ambiguous_class_bindings
                        .entry(owner.clone())
                        .or_default()
                        .insert(candidate.raw.clone());
                }
            } else {
                out.ambiguous_names.insert(candidate.name.clone());
            }
        }
    }
}

/// Find same-binding collisions before admitting any declaration in a lexical
/// body. A structural block creates no Python scope, but its declaration may
/// replace another declaration or an import depending on which branch runs.
/// The linker's name slot cannot represent those alternatives faithfully.
fn prepare_python_conditional_ambiguities(
    body: &tree_sitter::Node,
    source: &[u8],
    scope: &PyScope,
    out: &mut PyExtraction,
) {
    let mut definitions: std::collections::HashMap<String, Vec<tree_sitter::Node>> =
        std::collections::HashMap::new();
    for_each_python_definition(body, &mut |definition| {
        if let Some(raw) = python_definition_raw_name(&definition, source) {
            definitions.entry(raw).or_default().push(definition);
        }
    });
    let mut imported = std::collections::HashSet::new();
    collect_python_scope_import_names(body, source, &mut imported);
    let (globals, nonlocals) = python_outward_bindings(body, source);
    if let Some(parameters) = body
        .parent()
        .filter(|parent| parent.kind() == "function_definition")
        .and_then(|parent| parent.child_by_field_name("parameters"))
    {
        collect_python_binding_targets(&parameters, source, &mut imported);
    }
    let mut classes = Vec::new();
    for (raw, declarations) in &definitions {
        for definition in declarations {
            if let Some(declaration) = python_unwrap_decorated(definition)
                .filter(|declaration| declaration.kind() == "class_definition")
            {
                classes.push((*definition, declaration, raw.clone()));
            }
        }
        let guarded = declarations.iter().any(|declaration| {
            declaration
                .parent()
                .is_none_or(|parent| parent.id() != body.id())
        });
        let outward_global =
            globals.contains(raw) && (scope.class_ctx.is_some() || !scope.frames.is_empty());
        let outward_nonlocal = nonlocals.contains(raw);
        if !guarded
            || (declarations.len() == 1
                && !imported.contains(raw)
                && !outward_global
                && !outward_nonlocal)
        {
            continue;
        }
        // These definitions retain a lexical qualname but write an outward
        // binding. A conditional write cannot certify its old target.
        if outward_global {
            out.ambiguous_names.insert(raw.clone());
        }
        if outward_nonlocal {
            let target = scope
                .frames
                .iter()
                .rev()
                .skip(usize::from(scope.class_ctx.is_none()))
                .filter(|frame| !frame.globals.contains(raw))
                .find_map(|frame| {
                    frame.definitions.get(raw).cloned().or_else(|| {
                        frame
                            .bound
                            .contains(raw)
                            .then(|| local_scope_name(&frame.entity, raw))
                    })
                })
                .unwrap_or_else(|| raw.clone());
            out.ambiguous_names.insert(target);
        }
        for definition in declarations {
            let Some(declaration) = python_unwrap_decorated(definition) else {
                continue;
            };
            let (_, name, _) =
                python_definition_name(scope, raw, declaration.kind() == "class_definition");
            out.ambiguous_names.insert(name);
            out.ambiguous_definitions.insert(definition.id());
            out.ambiguous_definitions.insert(declaration.id());
        }
    }
    // A class derived from one of these bindings cannot choose a base from a
    // repository-wide namesake. Retain its written base census instead. Repeat
    // so source order cannot decide which dependent class is withheld.
    loop {
        let mut changed = false;
        for (definition, declaration, raw) in &classes {
            if out.ambiguous_definitions.contains(&definition.id()) {
                continue;
            }
            let ambiguous_base = python_class_bases_as_written(declaration, source)
                .iter()
                .any(|written| {
                    let base = python_local_binding_name(&scope.frames, written)
                        .unwrap_or_else(|| written.clone());
                    out.ambiguous_names.contains(&base)
                        || out
                            .ambiguous_names
                            .iter()
                            .any(|name| base.starts_with(&format!("{name}.")))
                        || scope
                            .class_ctx
                            .as_ref()
                            .and_then(|owner| out.ambiguous_class_bindings.get(owner))
                            .is_some_and(|bindings| bindings.contains(written))
                });
            if ambiguous_base {
                let (_, name, _) = python_definition_name(scope, raw, true);
                out.ambiguous_names.insert(name);
                out.ambiguous_definitions.insert(definition.id());
                out.ambiguous_definitions.insert(declaration.id());
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// Explicit outward binding statements in this lexical body. Nested scopes
/// declare their own globals/nonlocals and cannot change this body's map.
fn python_outward_bindings(
    body: &tree_sitter::Node,
    source: &[u8],
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    fn walk(
        node: &tree_sitter::Node,
        source: &[u8],
        globals: &mut std::collections::HashSet<String>,
        nonlocals: &mut std::collections::HashSet<String>,
    ) {
        match node.kind() {
            "function_definition" | "class_definition" | "decorated_definition" | "lambda" => {
                return
            }
            "global_statement" => {
                collect_python_binding_targets(node, source, globals);
                return;
            }
            "nonlocal_statement" => {
                collect_python_binding_targets(node, source, nonlocals);
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(&child, source, globals, nonlocals);
        }
    }
    let mut globals = std::collections::HashSet::new();
    let mut nonlocals = std::collections::HashSet::new();
    walk(body, source, &mut globals, &mut nonlocals);
    (globals, nonlocals)
}

/// Collect imports and other value bindings for collision detection only,
/// without promoting guarded imports into the runtime list or entering a
/// nested scope. A conditional declaration can also replace an assigned value.
fn collect_python_scope_import_names(
    node: &tree_sitter::Node,
    source: &[u8],
    out: &mut std::collections::HashSet<String>,
) {
    match node.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_python_binding_targets(&left, source, out);
            }
        }
        "named_expression" => {
            if let Some(name) = node.child_by_field_name("name") {
                collect_python_binding_targets(&name, source, out);
            }
        }
        "as_pattern" | "except_clause" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                collect_python_binding_targets(&alias, source, out);
            }
        }
        _ => {}
    }
    let imports = match node.kind() {
        "function_definition" | "class_definition" | "decorated_definition" | "lambda" => return,
        "import_statement" => extract_py_imports(node, source),
        "import_from_statement" => extract_py_from_import(node, source).into_iter().collect(),
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_python_scope_import_names(&child, source, out);
            }
            return;
        }
    };
    for import in imports {
        for specifier in import.specifiers {
            let name = if node.kind() == "import_statement" {
                specifier.local_name.split('.').next().unwrap_or("")
            } else {
                &specifier.local_name
            };
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
}

/// Admit definitions through ordinary statements without entering another
/// definition's lexical body. Returned nodes belong to their own entity, or
/// are ambiguous bodies whose calls must remain unknown rather than acquire
/// an invented enclosing caller.
fn admit_python_statement_definitions(
    statement: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    scope: &PyScope,
    out: &mut PyExtraction,
) -> std::collections::HashSet<usize> {
    let mut skipped = std::collections::HashSet::new();
    if python_unwrap_decorated(statement).is_some() {
        return skipped;
    }
    let mut definitions = Vec::new();
    for_each_python_definition(statement, &mut |definition| definitions.push(definition));
    for definition in definitions {
        if extract_py_node(&definition, source, file_id, scope, out)
            || out.ambiguous_definitions.contains(&definition.id())
        {
            skipped.insert(definition.id());
        }
    }
    skipped
}

/// Admit every definition in a function body while retaining its lexical scope.
fn admit_python_body_definitions(
    function: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    scope: &PyScope,
    out: &mut PyExtraction,
) -> std::collections::HashSet<usize> {
    let mut admitted = std::collections::HashSet::new();
    let Some(body) = function.child_by_field_name("body") else {
        return admitted;
    };
    prepare_python_conditional_ambiguities(&body, source, scope, out);
    let mut definitions = Vec::new();
    for_each_python_definition(&body, &mut |definition| definitions.push(definition));
    for definition in definitions {
        if extract_py_node(&definition, source, file_id, scope, out)
            || out.ambiguous_definitions.contains(&definition.id())
        {
            admitted.insert(definition.id());
        }
    }
    admitted
}

/// Walk one statement, admitting the definition it declares under `scope`.
///
/// Returns whether a definition was admitted, which a function body's walk
/// reads to know which nested bodies now belong to an entity of their own.
fn extract_py_node(
    node: &tree_sitter::Node,
    source: &[u8],
    file_id: &FilePathId,
    scope: &PyScope,
    out: &mut PyExtraction,
) -> bool {
    match node.kind() {
        "function_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return false;
            };
            let raw_name = name_node.utf8_text(source).unwrap_or("").to_string();
            let (kind, name, container) = python_definition_name(scope, &raw_name, false);
            if out.ambiguous_definitions.contains(&node.id()) {
                out.census.record(UnadmittedNestedDefinition {
                    name,
                    kind,
                    start_line: node.start_position().row as u32,
                    enclosing: scope.enclosing.clone(),
                    bases: Vec::new(),
                });
                return false;
            }
            // A definition inside a function body is new to the graph, and
            // its name has to be the only one of its kind in the file.
            if is_local_scope_name(&name) && !out.local_names.claim(&name) {
                out.census.record(UnadmittedNestedDefinition {
                    name,
                    kind,
                    start_line: node.start_position().row as u32,
                    enclosing: scope.enclosing.clone(),
                    bases: Vec::new(),
                });
                return false;
            }
            let vis = if raw_name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };
            out.entities.push(ExtractedEntity {
                kind,
                name: name.clone(),
                signature: node_signature(node, source),
                visibility: vis,
                doc_summary: extract_docstring(node, source),
                fingerprint: compute_fingerprint(node, source),
                span: span_from_node(node, file_id),
                declaration_line: None,
            });

            let frame = std::rc::Rc::new(PyFrame::for_function(
                node,
                source,
                &name,
                scope.frames.last().map(|frame| frame.as_ref()),
            ));
            let mut frames = scope.frames.clone();
            frames.push(frame.clone());
            let body_scope = PyScope {
                class_ctx: None,
                frames,
                self_class: if kind == EntityKind::Method {
                    scope.class_ctx.clone()
                } else {
                    scope.self_class.clone()
                },
                enclosing: Some(name.clone()),
            };
            // Definitions in the body first, so the calls and reads written
            // inside them are known to be theirs before this function's own
            // walks run.
            let nested = admit_python_body_definitions(node, source, file_id, &body_scope, out);
            extract_calls_from_context(
                node,
                source,
                &PyCallScope {
                    context_name: &name,
                    self_class: body_scope.self_class.as_deref(),
                    receiver_types: &frame.receiver_types,
                    frames: &body_scope.frames,
                    skip: &nested,
                    module_names: &out.module_names,
                },
                &mut out.relations,
                &mut out.call_audit,
            );
            if kind == EntityKind::Method {
                if let Some(class) = scope.class_ctx.as_deref() {
                    member_values::push_self_member_values(
                        node,
                        source,
                        &name,
                        class,
                        &nested,
                        &mut out.class_data,
                        &mut out.relations,
                    );
                }
            }
            extract_value_refs_from_definition(
                node,
                source,
                &name,
                &PyRefScope::within(&body_scope.frames, Some(&nested)),
                &mut out.value_refs,
            );
            if let Some(container) = container {
                out.relations.push(ExtractedRelation {
                    site: Some(python_contains_site(
                        node,
                        scope.class_ctx.is_none() && scope.frames.is_empty(),
                    )),
                    receiver: None,
                    call_shape: None,
                    kind: kin_model::RelationKind::Contains,
                    src_name: container,
                    dst_name: name,
                    import_source: None,
                });
            }
            true
        }
        "class_definition" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return false;
            };
            let raw_name = name_node.utf8_text(source).unwrap_or("").to_string();
            let (_, name, container) = python_definition_name(scope, &raw_name, true);
            if out.ambiguous_definitions.contains(&node.id()) {
                out.census.record(UnadmittedNestedDefinition {
                    name,
                    kind: EntityKind::Class,
                    start_line: node.start_position().row as u32,
                    enclosing: scope.enclosing.clone(),
                    bases: python_class_bases_as_written(node, source),
                });
                return false;
            }
            if is_local_scope_name(&name) && !out.local_names.claim(&name) {
                out.census.record(UnadmittedNestedDefinition {
                    name,
                    kind: EntityKind::Class,
                    start_line: node.start_position().row as u32,
                    enclosing: scope.enclosing.clone(),
                    bases: python_class_bases_as_written(node, source),
                });
                return false;
            }
            out.entities.push(ExtractedEntity {
                kind: EntityKind::Class,
                name: name.clone(),
                signature: node_signature(node, source),
                visibility: if raw_name.starts_with('_') {
                    Visibility::Private
                } else {
                    Visibility::Public
                },
                doc_summary: extract_docstring(node, source),
                fingerprint: compute_fingerprint(node, source),
                span: span_from_node(node, file_id),
                declaration_line: None,
            });
            let class_index = out.entities.len() - 1;

            // Extract base classes
            if let Some(args) = node.child_by_field_name("superclasses") {
                let mut is_enum = false;
                let mut arg_cursor = args.walk();
                for arg in args.children(&mut arg_cursor) {
                    // `metaclass=Meta` and other keywords configure the
                    // class; they are not bases.
                    if arg.is_named() && !matches!(arg.kind(), "keyword_argument" | "comment") {
                        // `Base[T]` extends `Base`: the subscript chooses type
                        // arguments, the class it extends is its value.
                        let arg = python_base_class_node(arg);
                        let written = arg.utf8_text(source).unwrap_or("").to_string();
                        if !written.is_empty() {
                            const ENUM_BASES: &[&str] = &[
                                "Enum",
                                "IntEnum",
                                "StrEnum",
                                "Flag",
                                "IntFlag",
                                "enum.Enum",
                                "enum.IntEnum",
                                "enum.StrEnum",
                                "enum.Flag",
                                "enum.IntFlag",
                            ];
                            if ENUM_BASES.contains(&written.as_str()) {
                                is_enum = true;
                            }
                            // A base list is evaluated in the scope around the
                            // class, so `class B(A)` beside a local `class A`
                            // extends that local class and not a module-level
                            // `A`, and a base read from a local variable is
                            // that variable, not a class of the same name.
                            let base = if arg.kind() == "identifier" {
                                python_local_binding_name(&scope.frames, &written)
                                    .unwrap_or(written)
                            } else {
                                written
                            };
                            out.relations.push(ExtractedRelation {
                                site: None,
                                receiver: None,
                                call_shape: None,
                                kind: kin_model::RelationKind::Extends,
                                src_name: name.clone(),
                                dst_name: base,
                                import_source: None,
                            });
                        }
                    }
                }
                if is_enum {
                    out.entities[class_index].kind = EntityKind::EnumDef;
                }
            }
            if let Some(container) = container {
                out.relations.push(ExtractedRelation {
                    site: Some(python_contains_site(
                        node,
                        scope.class_ctx.is_none() && scope.frames.is_empty(),
                    )),
                    receiver: None,
                    call_shape: None,
                    kind: kin_model::RelationKind::Contains,
                    src_name: container,
                    dst_name: name.clone(),
                    import_source: None,
                });
            }

            // Recurse into class body. The body is not a scope its methods
            // can see, so they keep the enclosing function scopes, and its own
            // statements read those scopes too.
            let class_scope = PyScope {
                class_ctx: Some(name.clone()),
                frames: scope.frames.clone(),
                self_class: scope.self_class.clone(),
                enclosing: Some(name.clone()),
            };
            if let Some(body) = node.child_by_field_name("body") {
                prepare_python_conditional_ambiguities(&body, source, &class_scope, out);
                fields::admit_class_fields(body, source, file_id, &name, out);
                let mut body_cursor = body.walk();
                for member in body.children(&mut body_cursor) {
                    extract_py_node(&member, source, file_id, &class_scope, out);
                    let nested = admit_python_statement_definitions(
                        &member,
                        source,
                        file_id,
                        &class_scope,
                        out,
                    );
                    // Class-scope statements that define no entity of their
                    // own still name values: `handler = cmd_ingest` in a
                    // class body is a use of `cmd_ingest`, sourced from the
                    // class.
                    if !matches!(
                        member.kind(),
                        "function_definition" | "class_definition" | "decorated_definition"
                    ) {
                        let before = out.value_refs.len();
                        collect_python_value_refs(
                            &member,
                            source,
                            &name,
                            &PyRefScope::within(&scope.frames, Some(&nested)),
                            &mut out.value_refs,
                        );
                        stamp_class_attribute_declaration(
                            &member,
                            source,
                            &mut out.value_refs[before..],
                        );
                        // Its calls are the class's too. They are read here
                        // rather than left to the file-wide pass because a
                        // class body sees the scopes of the function around
                        // it, so `session_interface = FailingSessionInterface()`
                        // in a test's local class names the test's local class.
                        let no_types = PythonReceiverTypes::new();
                        extract_calls_from_context(
                            &member,
                            source,
                            &PyCallScope {
                                context_name: &name,
                                self_class: scope.self_class.as_deref(),
                                receiver_types: scope
                                    .frames
                                    .last()
                                    .map_or(&no_types, |frame| &frame.receiver_types),
                                frames: &scope.frames,
                                skip: &nested,
                                module_names: &out.module_names,
                            },
                            &mut out.relations,
                            &mut out.call_audit,
                        );
                    }
                }
            }
            true
        }
        "import_statement" | "import_from_statement" => false,
        "decorated_definition" => {
            // Collect decorator names, then extract the inner definition
            // with decorators prepended to the signature.
            let decorators = extract_decorators(node, source);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_definition" || child.kind() == "class_definition" {
                    let declaration_index = out.entities.len();
                    if !extract_py_node(&child, source, file_id, scope, out) {
                        return false;
                    }
                    // The declaration owns its decorators, including their call
                    // sites. A class extraction can append methods after the
                    // class, so retain the declaration's exact emission index.
                    //
                    // Python evaluates a decorator in the enclosing scope while
                    // it runs the definition, not when the decorated function
                    // runs. The declaration entity spans that whole definition,
                    // decorators included, so it is the innermost owner of the
                    // call site, which is the same rule every other call site
                    // follows. What the call can reach is a separate question,
                    // settled by its receiver below.
                    if !decorators.is_empty() {
                        let declaration_name = out.entities[declaration_index].name.clone();
                        for relation in &mut out.relations {
                            if relation.kind == kin_model::RelationKind::Contains
                                && relation.dst_name == declaration_name
                                && relation.site.as_ref().is_some_and(|site| {
                                    site.start_byte == child.start_byte()
                                        && site.end_byte == child.end_byte()
                                })
                            {
                                relation.site = Some(python_contains_site(
                                    node,
                                    scope.class_ctx.is_none() && scope.frames.is_empty(),
                                ));
                            }
                        }
                        if let Some(declaration) = out.entities.get_mut(declaration_index) {
                            let prefix = decorators
                                .iter()
                                .map(|decorator| format!("@{}", decorator.name))
                                .collect::<Vec<_>>()
                                .join(" ");
                            declaration.signature = format!("{} {}", prefix, declaration.signature);
                            declaration.declaration_line = Some(declaration.span.start_line);
                            declaration.span = span_from_node(node, file_id);
                            declaration.fingerprint = compute_fingerprint(node, source);
                        }
                        if let Some(declaration) = out.entities.get(declaration_index) {
                            let src_name = declaration.name.clone();
                            for decorator in &decorators {
                                if is_valid_callee_name(&decorator.name) {
                                    // Evaluated in the scope around the
                                    // definition, so a bare `@register` that
                                    // an enclosing function defines names
                                    // that local definition, one it imports
                                    // carries that module, and one it binds
                                    // as a value is a gap, exactly as a bare
                                    // call written there would be.
                                    let scoped = match decorator.receiver {
                                        None => resolve_python_scoped_name(
                                            &scope.frames,
                                            &decorator.name,
                                        ),
                                        Some(_) => None,
                                    };
                                    let (dst_name, import_source) = match scoped {
                                        Some(PythonScopedName::Definition(entity)) => {
                                            (entity.to_string(), None)
                                        }
                                        Some(PythonScopedName::Imported(module)) => {
                                            (decorator.name.clone(), Some(module.to_string()))
                                        }
                                        Some(PythonScopedName::Value) => {
                                            out.relations.push(
                                                scoped_call_extraction_incomplete_marker(
                                                    src_name.clone(),
                                                    decorator.site.clone(),
                                                    Some(decorator.name.clone()),
                                                ),
                                            );
                                            continue;
                                        }
                                        None => (decorator.name.clone(), None),
                                    };
                                    out.relations.push(ExtractedRelation {
                                        site: Some(decorator.site.clone()),
                                        receiver: decorator.receiver.clone(),
                                        call_shape: None,
                                        kind: kin_model::RelationKind::Calls,
                                        src_name: src_name.clone(),
                                        dst_name,
                                        import_source,
                                    });
                                }
                            }
                        }
                    }
                    return true;
                }
            }
            false
        }
        "expression_statement" if scope.class_ctx.is_none() && scope.frames.is_empty() => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "assignment" || child.kind() == "assignment_statement" {
                    extract_py_node(&child, source, file_id, scope, out);
                }
            }
            false
        }
        "assignment" | "assignment_statement"
            if scope.class_ctx.is_none() && scope.frames.is_empty() =>
        {
            if let Some(name) = extract_py_constant_name(node, source) {
                out.entities.push(ExtractedEntity {
                    kind: EntityKind::Constant,
                    name: name.clone(),
                    signature: node_signature(node, source),
                    visibility: if name.starts_with('_') {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    },
                    doc_summary: None,
                    fingerprint: compute_fingerprint(node, source),
                    span: span_from_node(node, file_id),
                    declaration_line: None,
                });
                if let Some(value) = node.child_by_field_name("right") {
                    fields::admit_literal_fields(value, source, file_id, &name, out);
                }
                if let Some(module) = &scope.enclosing {
                    out.relations.push(ExtractedRelation {
                        kind: kin_model::RelationKind::Contains,
                        src_name: module.clone(),
                        dst_name: name,
                        site: Some(python_contains_site(
                            node,
                            scope.class_ctx.is_none() && scope.frames.is_empty(),
                        )),
                        receiver: None,
                        call_shape: None,
                        import_source: None,
                    });
                }
            }
            false
        }
        _ => false,
    }
}

/// The child declaration, with an explicit lexical-containment role so the
/// linker can distinguish a module from its same-named declaration.
fn python_contains_site(node: &tree_sitter::Node, module: bool) -> RelationSite {
    RelationSite {
        syntactic_role: Some(if module {
            crate::extract::RelationSyntacticRole::ModuleContains
        } else {
            crate::extract::RelationSyntacticRole::LexicalContains
        }),
        ..site_from_node(node)
    }
}

/// Resolve a Python file's module name and whether it marks a package.
///
/// In Python each `.py` file IS a module (PEP 328); `__init__.py` additionally
/// marks its containing directory as a package.
fn python_module_identity(file_id: &FilePathId) -> (String, bool) {
    if file_id.0.ends_with("__init__.py") {
        let pkg = file_id
            .0
            .trim_end_matches("__init__.py")
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("__init__")
            .to_string();
        (pkg, true)
    } else {
        let stem = file_id
            .0
            .rsplit('/')
            .next()
            .and_then(|leaf| leaf.strip_suffix(".py"))
            .unwrap_or("")
            .to_string();
        (stem, false)
    }
}

/// A symbol named without being invoked, recorded during the walk.
///
/// Two positions produce one of these: a VALUE read (`set_defaults(func=cmd_ingest)`)
/// and a TYPE annotation (`def upsert(note: ParsedNote)`). Both name a symbol
/// the enclosing entity depends on, both must be found by `find_references`,
/// and both bind by the same rule, so they share one record and one emit path.
///
/// `member` is set when the reference was written as an attribute access
/// (`TAG_RE.pattern`, `typing.Optional`), because whether that names the root or
/// the leaf depends on how the root was bound, and the file's imports are not
/// known until the walk finishes. Candidates are filtered rather than emitted
/// directly because a reference may precede its definition in the file.
struct PythonValueRef {
    src_name: String,
    root: String,
    member: Option<String>,
    /// Where the name was read. Carried per occurrence rather than per
    /// (source, target) pair, because a function that reads a constant on three
    /// lines has three reference sites and a row reporting one of them would
    /// under-report while its completeness flag read true.
    site: RelationSite,
    /// The class attribute this reference DECLARES the type of, set only for a
    /// class body's own `attribute: Type` annotation.
    ///
    /// `class Response: connection: HTTPAdapter` already emitted a reference
    /// from `Response` to `HTTPAdapter`, and that edge alone cannot say which
    /// attribute holds that type. Carrying the attribute is what lets the
    /// linker join `r.connection.send(...)` under `r: Response` to
    /// `HTTPAdapter.send`: the first hop is declared in the calling scope, the
    /// second on a class the calling file may never open.
    attribute: Option<String>,
}

/// Collect the references made by one `function_definition`, sourced from the
/// entity `context_name`.
///
/// Three positions in a signature name a symbol. Parameter defaults read one
/// (`def wire(handler=cmd_ingest)`), parameter annotations name a type
/// (`def upsert(note: ParsedNote)`), and the return annotation names another
/// (`-> ParsedNote`). Parameter NAMES are still skipped, because a name being
/// bound is not a name being read.
///
/// Names the function binds locally shadow the module scope in Python, so a
/// local called `parse` is not a read of a module-level `parse`. Annotations
/// carry the same shadow set: it costs no recall on the CamelCase-type,
/// snake_case-local convention Python code follows, and it keeps a local from
/// binding to an unrelated module-level entity of the same name.
fn extract_value_refs_from_definition(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    shadowed: &PyRefScope,
    out: &mut Vec<PythonValueRef>,
) {
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            // `typed_parameter` also carries `*args: T` and `**kw: T`, whose
            // name sits in a splat pattern while the annotation stays on the
            // same `type` field.
            if let Some(annotation) = param.child_by_field_name("type") {
                collect_python_type_refs(&annotation, source, context_name, shadowed, out);
            }
            if matches!(
                param.kind(),
                "default_parameter" | "typed_default_parameter"
            ) {
                if let Some(value) = param.child_by_field_name("value") {
                    collect_python_value_refs(&value, source, context_name, shadowed, out);
                }
            }
        }
    }
    if let Some(return_type) = node.child_by_field_name("return_type") {
        collect_python_type_refs(&return_type, source, context_name, shadowed, out);
    }
    if let Some(body) = node.child_by_field_name("body") {
        collect_python_value_refs(&body, source, context_name, shadowed, out);
    }
}

/// Names a Python function binds in its own scope, which therefore shadow any
/// module-level entity of the same name.
///
/// A `global`/`nonlocal` declaration re-points the name at an outer scope, so
/// those names are removed again: `global TAG_RE` followed by an assignment is
/// a write to the module-level constant, not a local.
fn collect_python_local_bindings(
    node: &tree_sitter::Node,
    source: &[u8],
) -> std::collections::HashSet<String> {
    let mut bound = std::collections::HashSet::new();
    let mut rebound_outward = std::collections::HashSet::new();
    if let Some(params) = node.child_by_field_name("parameters") {
        collect_python_binding_targets(&params, source, &mut bound);
    }
    if let Some(body) = node.child_by_field_name("body") {
        collect_python_scope_bindings(&body, source, &mut bound, &mut rebound_outward);
    }
    for name in rebound_outward {
        bound.remove(&name);
    }
    bound
}

/// The names one function scope binds itself, and the names it declares
/// `global`, for resolving a read to the scope that owns it.
///
/// Unlike [`collect_python_local_bindings`], which feeds the reference walk's
/// deliberately broad shadow set, this stops at every nested `def`, `class`
/// and `lambda`: their parameters and locals belong to their own scope, and
/// counting them here would let a helper's local variable hide the enclosing
/// function's nested class of the same name. The nested definition's own name
/// is bound here, because the `def` statement binds it in this scope.
///
/// A `nonlocal` name is left out of both sets, so a lookup continues to the
/// enclosing function scope that really binds it.
///
/// The third result holds the names the scope binds only through a plain
/// `from module import name`, each with the module as written, so a call
/// through such a name can carry the module it came from. An aliased import,
/// an `import module` and any second binding of the same name are values.
fn collect_python_frame_bindings(
    function: &tree_sitter::Node,
    source: &[u8],
) -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
    std::collections::HashMap<String, String>,
) {
    struct Bindings {
        /// Every name bound other than through a plain `from m import name`.
        values: std::collections::HashSet<String>,
        /// Names a plain `from m import name` binds, with the module, or
        /// `None` once two such imports name different modules.
        imported: std::collections::HashMap<String, Option<String>>,
        globals: std::collections::HashSet<String>,
        nonlocals: std::collections::HashSet<String>,
    }

    fn walk(node: &tree_sitter::Node, source: &[u8], out: &mut Bindings) {
        match node.kind() {
            "function_definition" | "class_definition" => {
                if let Some(name) = node.child_by_field_name("name") {
                    collect_python_binding_targets(&name, source, &mut out.values);
                }
                return;
            }
            "lambda" => return,
            "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
                if let Some(left) = node.child_by_field_name("left") {
                    collect_python_binding_targets(&left, source, &mut out.values);
                }
            }
            "named_expression" => {
                if let Some(name) = node.child_by_field_name("name") {
                    collect_python_binding_targets(&name, source, &mut out.values);
                }
            }
            // `with m as name`, and `except E as name`, whose grammar holds the
            // name in the clause's own `alias` field rather than an `as_pattern`.
            "as_pattern" | "except_clause" => {
                if let Some(alias) = node.child_by_field_name("alias") {
                    collect_python_binding_targets(&alias, source, &mut out.values);
                }
            }
            // `import a.b` binds `a`; `import a as b` and `from m import x as
            // b` bind `b`; `from m import x` binds `x`.
            "import_statement" | "import_from_statement" => {
                let module = node.child_by_field_name("module_name");
                let from_module = (node.kind() == "import_from_statement")
                    .then(|| module.and_then(|m| m.utf8_text(source).ok()))
                    .flatten()
                    .filter(|module| !module.is_empty());
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if Some(child.id()) == module.map(|m| m.id()) {
                        continue;
                    }
                    match child.kind() {
                        "dotted_name" => {
                            let Some(bound_name) = child
                                .utf8_text(source)
                                .ok()
                                .and_then(|text| text.split('.').next())
                                .filter(|name| !name.is_empty())
                            else {
                                continue;
                            };
                            match from_module {
                                Some(from_module) => {
                                    let entry = out
                                        .imported
                                        .entry(bound_name.to_string())
                                        .or_insert_with(|| Some(from_module.to_string()));
                                    if entry.as_deref() != Some(from_module) {
                                        *entry = None;
                                    }
                                }
                                None => {
                                    out.values.insert(bound_name.to_string());
                                }
                            }
                        }
                        "aliased_import" => {
                            if let Some(alias) = child
                                .child_by_field_name("alias")
                                .and_then(|alias| alias.utf8_text(source).ok())
                                .filter(|alias| !alias.is_empty())
                            {
                                out.values.insert(alias.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            "global_statement" => {
                collect_python_binding_targets(node, source, &mut out.globals);
                return;
            }
            "nonlocal_statement" => {
                collect_python_binding_targets(node, source, &mut out.nonlocals);
                return;
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(&child, source, out);
        }
    }

    let mut out = Bindings {
        values: std::collections::HashSet::new(),
        imported: std::collections::HashMap::new(),
        globals: std::collections::HashSet::new(),
        nonlocals: std::collections::HashSet::new(),
    };
    if let Some(params) = function.child_by_field_name("parameters") {
        collect_python_binding_targets(&params, source, &mut out.values);
    }
    if let Some(body) = function.child_by_field_name("body") {
        walk(&body, source, &mut out);
    }
    let Bindings {
        values,
        imported,
        globals,
        nonlocals,
    } = out;
    let outward = |name: &String| globals.contains(name) || nonlocals.contains(name);
    let bound = values
        .iter()
        .chain(imported.keys())
        .filter(|name| !outward(name))
        .cloned()
        .collect();
    let imports = imported
        .into_iter()
        .filter(|(name, _)| !values.contains(name) && !outward(name))
        .filter_map(|(name, module)| Some((name, module?)))
        .collect();
    (bound, globals, imports)
}

/// Whether this source read reaches a module import rather than any local or
/// enclosing function binding. Reuse the adapter's lexical binding census;
/// defaults/decorators are evaluated outside the definition's body scope.
pub(crate) fn module_import_read_is_unshadowed(
    read: tree_sitter::Node<'_>,
    source: &[u8],
    name: &str,
) -> bool {
    let mut ancestor = read;
    let mut function_body = false;
    while let Some(parent) = ancestor.parent() {
        match parent.kind() {
            "lambda"
            | "list_comprehension"
            | "set_comprehension"
            | "dictionary_comprehension"
            | "generator_expression" => return false,
            "function_definition" => {
                if parent.child_by_field_name("type_parameters").is_some() {
                    return false;
                }
                if parent.child_by_field_name("body").is_some_and(|body| {
                    body.start_byte() <= read.start_byte() && read.end_byte() <= body.end_byte()
                }) {
                    function_body = true;
                    let (bound, globals, _) = collect_python_frame_bindings(&parent, source);
                    if bound.contains(name) || globals.contains(name) {
                        return false;
                    }
                }
            }
            // A method sees enclosing functions and the module, not its class's
            // namespace. Class-body expressions need separate class evidence.
            "class_definition" if !function_body => return false,
            _ => {}
        }
        ancestor = parent;
    }
    true
}

fn collect_python_scope_bindings(
    node: &tree_sitter::Node,
    source: &[u8],
    bound: &mut std::collections::HashSet<String>,
    rebound_outward: &mut std::collections::HashSet<String>,
) {
    match node.kind() {
        "assignment" | "augmented_assignment" | "for_statement" | "for_in_clause" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_python_binding_targets(&left, source, bound);
            }
        }
        "named_expression" => {
            if let Some(name) = node.child_by_field_name("name") {
                collect_python_binding_targets(&name, source, bound);
            }
        }
        "as_pattern" => {
            if let Some(alias) = node.child_by_field_name("alias") {
                collect_python_binding_targets(&alias, source, bound);
            }
        }
        "function_definition" | "class_definition" => {
            if let Some(name) = node.child_by_field_name("name") {
                collect_python_binding_targets(&name, source, bound);
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                collect_python_binding_targets(&params, source, bound);
            }
        }
        "global_statement" | "nonlocal_statement" => {
            collect_python_binding_targets(node, source, rebound_outward);
            return;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_python_scope_bindings(&child, source, bound, rebound_outward);
    }
}

/// Collect the identifiers a binding construct writes to, descending through
/// tuple and list patterns. An attribute or subscript target (`self.x = 1`,
/// `d["k"] = 1`) binds no new name, so its identifiers are not collected.
fn collect_python_binding_targets(
    node: &tree_sitter::Node,
    source: &[u8],
    out: &mut std::collections::HashSet<String>,
) {
    match node.kind() {
        "identifier" => {
            if let Ok(name) = node.utf8_text(source) {
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
            }
        }
        "attribute" | "subscript" | "type" => {}
        "default_parameter" | "typed_default_parameter" | "typed_parameter" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    collect_python_binding_targets(&child, source, out);
                    break;
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_python_binding_targets(&child, source, out);
            }
        }
    }
}

/// Collect the value reads within an expression subtree, pruning the positions
/// that are not reads.
///
/// Pruned: a call's callee identifier (already a `Calls` edge), an attribute's
/// `.leaf` selector, an assignment or loop target, a keyword argument's name,
/// and an import statement. Kept, because each is a genuine read of the named
/// symbol: a call argument (`set_defaults(func=cmd_ingest)`), an assignment
/// right-hand side, a collection literal element, a returned expression, and
/// the receiver of an attribute access (`TAG_RE.pattern`).
///
/// A `type` node is not walked as a value, because the names inside an
/// annotation are not read at this position. It is handed to
/// [`collect_python_type_refs`] instead, which is why an annotated assignment
/// (`links: Tuple[WikiLink, ...] = ()`) contributes both its right-hand side
/// and the types its annotation names.
fn collect_python_value_refs(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    shadowed: &PyRefScope,
    out: &mut Vec<PythonValueRef>,
) {
    // A nested definition the walk admitted reads its own names, sourced from
    // its own entity.
    if shadowed.skips(node) {
        return;
    }
    match node.kind() {
        "identifier" => {
            if let Some(root) = node
                .utf8_text(source)
                .ok()
                .filter(|name| !name.is_empty())
                .and_then(|name| shadowed.root_for(name))
            {
                out.push(PythonValueRef {
                    src_name: context_name.to_string(),
                    root,
                    member: None,
                    site: site_from_node(node),
                    attribute: None,
                });
            }
        }
        // `X.leaf` reads `X`. Whether it also names `leaf` depends on how `X`
        // was bound, which only the import table can say, so both halves are
        // carried and decided at emit time.
        "attribute" => {
            let object = node.child_by_field_name("object");
            let leaf = node
                .child_by_field_name("attribute")
                .and_then(|n| n.utf8_text(source).ok());
            match (object, leaf) {
                (Some(object), Some(leaf)) if object.kind() == "identifier" && !leaf.is_empty() => {
                    if let Some(root) = object
                        .utf8_text(source)
                        .ok()
                        .filter(|root| !root.is_empty())
                        .and_then(|root| shadowed.root_for(root))
                    {
                        out.push(PythonValueRef {
                            src_name: context_name.to_string(),
                            root,
                            member: Some(leaf.to_string()),
                            site: site_from_node(node),
                            attribute: None,
                        });
                    }
                }
                (Some(object), _) => {
                    collect_python_value_refs(&object, source, context_name, shadowed, out)
                }
                _ => {}
            }
        }
        // A call contributes its arguments and, for `receiver.method()`, the
        // receiver. The callee name is already carried by the `Calls` edge.
        "call" => {
            if let Some(function) = node.child_by_field_name("function") {
                match function.kind() {
                    "identifier" => {}
                    "attribute" => {
                        if let Some(object) = function.child_by_field_name("object") {
                            collect_python_value_refs(&object, source, context_name, shadowed, out);
                        }
                    }
                    _ => collect_python_value_refs(&function, source, context_name, shadowed, out),
                }
            }
            if let Some(args) = node.child_by_field_name("arguments") {
                collect_python_value_refs(&args, source, context_name, shadowed, out);
            }
        }
        // `f(name=value)`: the keyword names a parameter of the callee, not a
        // symbol in this scope. Only the value is a read.
        "keyword_argument" => {
            if let Some(value) = node.child_by_field_name("value") {
                collect_python_value_refs(&value, source, context_name, shadowed, out);
            }
        }
        "assignment" | "augmented_assignment" => {
            // `links: Tuple[WikiLink, ...] = ()` in a class body, a module-scope
            // `TOP: ParsedNote = None`, and a body-local `note: ParsedNote = x`
            // all wear this shape, so one arm covers every annotated binding
            // outside a signature.
            if let Some(annotation) = node.child_by_field_name("type") {
                collect_python_type_refs(&annotation, source, context_name, shadowed, out);
            }
            if let Some(right) = node.child_by_field_name("right") {
                collect_python_value_refs(&right, source, context_name, shadowed, out);
            }
        }
        "for_statement" | "for_in_clause" => {
            if let Some(right) = node.child_by_field_name("right") {
                collect_python_value_refs(&right, source, context_name, shadowed, out);
            }
            if let Some(body) = node.child_by_field_name("body") {
                collect_python_value_refs(&body, source, context_name, shadowed, out);
            }
        }
        "parameters"
        | "type"
        | "import_statement"
        | "import_from_statement"
        | "future_import_statement"
        | "global_statement"
        | "nonlocal_statement" => {}
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_python_value_refs(&child, source, context_name, shadowed, out);
            }
        }
    }
}

/// Collect the symbols named inside one type annotation, sourced from the
/// entity `context_name`.
///
/// A type position is a use. `def upsert(note: ParsedNote) -> int` depends on
/// `ParsedNote` exactly as a call depends on its callee, and a rename that
/// cannot see it leaves the old name in the signature. Before this, the Python
/// adapter emitted no edge of any class for an annotation, so a class used only
/// as a parameter type, a return type or a dataclass field type reported zero
/// inbound references.
///
/// The walk descends through the wrappers tree-sitter builds a type expression
/// from rather than matching each one, so `Optional[Note]`,
/// `Tuple[WikiLink, ...]`, `dict[str, Note]`, `Note | None`,
/// `Callable[[Note], int]` and `typing.List[Note]` each reach every name they
/// carry. `str`, `int` and other builtins are collected here and dropped by
/// [`emit_python_value_references`], which binds only what the file defines or
/// imports.
///
/// A quoted forward reference (`x: "Note"`) is read only when the quotes hold a
/// bare identifier. A quoted expression is left alone rather than re-parsed as
/// a type by hand.
fn collect_python_type_refs(
    node: &tree_sitter::Node,
    source: &[u8],
    context_name: &str,
    shadowed: &PyRefScope,
    out: &mut Vec<PythonValueRef>,
) {
    match node.kind() {
        "identifier" => {
            if let Some(root) = node
                .utf8_text(source)
                .ok()
                .filter(|name| !name.is_empty())
                .and_then(|name| shadowed.root_for(name))
            {
                out.push(PythonValueRef {
                    src_name: context_name.to_string(),
                    root,
                    member: None,
                    site: site_from_node(node),
                    attribute: None,
                });
            }
        }
        // `typing.Optional` in a type position binds exactly as `TAG_RE.pattern`
        // does in a value position: which half names the symbol depends on how
        // the root was bound, so both are carried to the emit step.
        "attribute" => {
            let object = node.child_by_field_name("object");
            let leaf = node
                .child_by_field_name("attribute")
                .and_then(|n| n.utf8_text(source).ok());
            match (object, leaf) {
                (Some(object), Some(leaf)) if object.kind() == "identifier" && !leaf.is_empty() => {
                    if let Some(root) = object
                        .utf8_text(source)
                        .ok()
                        .filter(|root| !root.is_empty())
                        .and_then(|root| shadowed.root_for(root))
                    {
                        out.push(PythonValueRef {
                            src_name: context_name.to_string(),
                            root,
                            member: Some(leaf.to_string()),
                            site: site_from_node(node),
                            attribute: None,
                        });
                    }
                }
                (Some(object), _) => {
                    collect_python_type_refs(&object, source, context_name, shadowed, out)
                }
                _ => {}
            }
        }
        "string" => {
            let mut cursor = node.walk();
            let mut contents = node
                .children(&mut cursor)
                .filter(|child| child.kind() == "string_content");
            if let (Some(content), None) = (contents.next(), contents.next()) {
                if let Some(root) = content
                    .utf8_text(source)
                    .ok()
                    .filter(|text| is_python_identifier(text))
                    .and_then(|text| shadowed.root_for(text))
                {
                    out.push(PythonValueRef {
                        src_name: context_name.to_string(),
                        root,
                        member: None,
                        site: site_from_node(&content),
                        attribute: None,
                    });
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_python_type_refs(&child, source, context_name, shadowed, out);
            }
        }
    }
}

/// Whether `text` is a single Python identifier, used to decide whether a
/// quoted forward reference names one symbol or holds an expression.
fn is_python_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_alphanumeric())
}

/// Turn collected value reads and type annotations into `References` edges.
///
/// Only a name this file DEFINES or IMPORTS may bind, which in Python is the
/// complete set: a module-level symbol is reachable from a module only when it
/// is defined there or imported into it, and anything else in a value or type
/// position is a local, a parameter, or a builtin. Filtering here rather than
/// in the linker is what keeps a local named `config` from binding by name
/// alone to some unrelated module-level `config` in another file, and it is
/// what stops an annotation naming `str` or `Optional` from reaching a
/// same-named entity in an unimported module.
///
/// `RelationKind::References` is the existing kind for a non-call reference,
/// already emitted by the Go, C++, C# and Ruby adapters and already read by the
/// linker's import and qualified-suffix tiers, the dead-code reference triple,
/// and `find_references`. No new kind is introduced.
fn emit_python_value_references(
    value_refs: Vec<PythonValueRef>,
    entities: &[ExtractedEntity],
    imports: &[FileImport],
    guarded_imports: &[FileImport],
    relations: &mut Vec<ExtractedRelation>,
) {
    // The file's Module entity is named for the file, and a Python module
    // binds no name for itself in its own namespace: `repr` read in `repr.py`
    // is the builtin unless the file declares or imports `repr`.
    let defined: std::collections::HashSet<&str> = entities
        .iter()
        .filter(|e| e.kind != EntityKind::Module)
        .map(|e| e.name.as_str())
        .collect();
    let mut module_bound: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut symbol_bound: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for import in imports.iter().chain(guarded_imports) {
        for spec in &import.specifiers {
            // `import parsing` binds the module itself under its own path;
            // `from parsing import TAG_RE` binds a symbol out of it.
            if spec.local_name == import.module_path {
                module_bound.insert(spec.local_name.as_str());
            } else {
                symbol_bound.insert(spec.local_name.as_str());
            }
        }
    }

    // Keyed on the site as well as the pair: the same name read twice inside one
    // function is two reference sites, and collapsing them here would leave the
    // row reporting one line while claiming its sites were complete.
    let mut seen: std::collections::HashSet<(String, String, usize)> =
        std::collections::HashSet::new();
    for value_ref in value_refs {
        let root = value_ref.root.as_str();
        let dst = match &value_ref.member {
            // `parsing.TAG_RE` through a module binding names the member. The
            // dotted form is what the linker's namespace-member tier resolves,
            // and it cannot be reduced to the bare leaf without guessing which
            // module a same-named symbol came from.
            Some(member) if module_bound.contains(root) => format!("{root}.{member}"),
            _ if defined.contains(root) || symbol_bound.contains(root) => root.to_string(),
            _ => continue,
        };
        // A self-reference establishes no reachability from another entity, and
        // the reference collectors drop it anyway.
        if dst == value_ref.src_name {
            continue;
        }
        if !seen.insert((
            value_ref.src_name.clone(),
            dst.clone(),
            value_ref.site.start_byte,
        )) {
            continue;
        }
        relations.push(ExtractedRelation {
            site: Some(value_ref.site),
            call_shape: None,
            // Set only for a class body's own `attribute: Type` annotation,
            // where it names the attribute whose type this edge declares. The
            // field is otherwise the receiver a `Calls` edge was written
            // through; both spellings answer the same question, which holder
            // does this edge hang off, and reusing it keeps the parser/linker
            // seam byte-identical rather than versioning it for one join.
            receiver: value_ref.attribute,
            kind: kin_model::RelationKind::References,
            src_name: value_ref.src_name,
            dst_name: dst,
            import_source: None,
        });
    }
}

/// Name a module-level assignment binds, when the graph should own the value.
///
/// Two independent rules admit a binding, because they answer to different
/// evidence and either one alone leaves a real constant invisible.
///
/// The first is the name's spelling, and it is decided by
/// [`looks_like_py_constant_name`].
///
/// The second is the value's shape: a name bound to a CALL is a module
/// singleton other files import and read, whatever case it is written in.
/// `codes = LookupDict(name="status_codes")` in requests is the case that named
/// the rule; three files import that name and the redirect path is built
/// entirely out of it, and with no entity behind it `kin impact codes` retargeted
/// to the enclosing module and answered "No local downstream impact found". The
/// same rule is already what JavaScript applies to a module-level binding: a
/// value containing a call expression is not data-only there and is admitted as
/// a `Constant` regardless of the name (`is_data_only_js_value` in
/// javascript.rs), which is how `React.forwardRef`, `styled` and `memo` bindings
/// become entities. Python's import is a statement rather than a call, so the
/// dependency-line flood that forced JavaScript to exclude `require(...)`
/// bindings has no equivalent here.
///
/// Only a bare identifier target qualifies for the call rule. `a, b = make()`
/// and `obj.attr = make()` reach here as `pattern_list` and `attribute`, and
/// their text is a destructuring or an attribute write rather than a name the
/// graph can carry.
///
/// Both rules run only where the caller already stands at module scope: the
/// `assignment` arm of `extract_py_node` is guarded by `class_ctx.is_none()`, so
/// class attributes never reach it, and the module-root walk never descends into
/// a function body, so locals never reach it either.
fn extract_py_constant_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let target = node
        .child_by_field_name("left")
        .or_else(|| node.named_child(0))?;
    let name = target.utf8_text(source).ok()?.trim().to_string();
    if looks_like_py_constant_name(&name) {
        return Some(name);
    }
    if target.kind() == "identifier"
        && node
            .child_by_field_name("right")
            .is_some_and(|value| py_value_is_call(&value))
    {
        return Some(name);
    }
    None
}

/// Whether an assignment's right-hand side is a call.
///
/// `await make()` and `(make())` bind the same singleton the bare call does, so
/// both unwrap to the call underneath rather than reading as some other shape.
fn py_value_is_call(node: &tree_sitter::Node) -> bool {
    match node.kind() {
        "call" => true,
        "await" | "parenthesized_expression" => node
            .named_child(0)
            .is_some_and(|inner| py_value_is_call(&inner)),
        _ => false,
    }
}

/// Whether a module-level assignment target is SPELLED as a constant.
///
/// PEP 8 writes constants in upper case "with underscores separating words", so
/// the underscore is a word separator rather than part of the convention.
/// Requiring one dropped every single-word constant while admitting its
/// multi-word siblings in the same file: on the store behind FIR-2509, five of
/// six module-level constants in one package were entities and `SCHEMA` at
/// `nk/storage.py:22` was not, so `semantic_search("SCHEMA")` certified
/// `safe_to_conclude_absent: true` over a constant that is plainly there. The
/// graph was self-consistently missing it, which is what made the certification
/// dangerous rather than merely wrong.
///
/// Two spellings are accepted. An all-uppercase name of two characters or more
/// is the PEP 8 constant with or without a separator (`SCHEMA`, `TAG_RE`,
/// `HTTP2`). A mixed-case name carrying an underscore is the looser shape this
/// predicate already admitted, kept so that no name which is an entity today
/// stops being one.
///
/// A single character stays out. At module level `T = TypeVar("T")` and a bare
/// `E` read as type parameters and scratch names far more often than as
/// constants, and the ones genuinely built by a call are admitted by the call
/// rule in [`extract_py_constant_name`] instead.
fn looks_like_py_constant_name(name: &str) -> bool {
    let mut has_upper = false;
    let mut has_lower = false;
    let mut has_underscore = false;
    for ch in name.chars() {
        if ch == '_' {
            has_underscore = true;
        } else if ch.is_ascii_uppercase() {
            has_upper = true;
        } else if ch.is_ascii_lowercase() {
            has_lower = true;
        } else if !ch.is_ascii_digit() {
            return false;
        }
    }
    if name.is_empty() || !has_upper {
        return false;
    }
    (!has_lower && name.len() >= 2) || has_underscore
}

/// One `@decorator` line on a definition, as the call it makes.
struct PythonDecorator {
    /// The callee's trailing identifier (`route` for `@app.route("/")`).
    name: String,
    /// The receiver as written when the decorator is an attribute (`app` for
    /// `@app.route("/")`), `None` for a bare `@name`. See
    /// [`extract_decorator_payload`] for why it has to travel.
    receiver: Option<String>,
    /// The `@decorator` syntax itself.
    site: RelationSite,
}

/// Extract the decorators of a `decorated_definition` node, each with the site
/// of the `@decorator` syntax that named it.
///
/// A decorator IS a call, and its `Calls` edge merges with any body call to the
/// same target, so a decorator contributing no site would leave that merged edge
/// reporting fewer lines than it has sites while its completeness flag still
/// read true.
fn extract_decorators(node: &tree_sitter::Node, source: &[u8]) -> Vec<PythonDecorator> {
    let mut decorators = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            if let Some((name, receiver)) = extract_decorator_payload(&child, source) {
                if !name.is_empty() {
                    decorators.push(PythonDecorator {
                        name,
                        receiver,
                        site: site_from_node(&child),
                    });
                }
            }
        }
    }
    decorators
}

/// Resolve a decorator's bare name from its tree-sitter payload.
/// Handles `@name`, `@mod.name`, and `@mod.name(args)` — always returning the
/// trailing identifier (e.g., "route" for `@app.route("/")`).
fn extract_decorator_payload_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    extract_decorator_payload(node, source).map(|(name, _)| name)
}

/// Resolve a decorator's bare name and, for an attribute, its receiver as
/// written: `("route", Some("app"))` for `@app.route("/")`, `("wraps",
/// Some("functools"))` for `@functools.wraps(f)`, `("register", None)` for
/// `@register`.
///
/// The receiver is what [`extract_named_callee`] records for the same call
/// written in a body, and it has to travel for the same reason. `@app.post(...)`
/// calls the member `post` of the value `app`; without its receiver it reaches
/// the linker as a bare `post(...)`, which binds any free function named `post`
/// in the repository. On fastapi that turned every route decorator into a call
/// to an unrelated test handler that happened to be named `post` or `get`.
fn extract_decorator_payload(
    node: &tree_sitter::Node,
    source: &[u8],
) -> Option<(String, Option<String>)> {
    let text = |node: tree_sitter::Node| node.utf8_text(source).unwrap_or("").to_string();
    let attribute = |node: &tree_sitter::Node| {
        let name = text(node.child_by_field_name("attribute")?);
        let receiver = node.child_by_field_name("object").map(text);
        Some((name, receiver))
    };
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" => return Some((text(child), None)),
            "attribute" => return attribute(&child),
            "call" => {
                if let Some(function) = child.child_by_field_name("function") {
                    return match function.kind() {
                        "identifier" => Some((text(function), None)),
                        "attribute" => attribute(&function),
                        _ => None,
                    };
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

fn node_signature(node: &tree_sitter::Node, source: &[u8]) -> String {
    crate::adapter::declaration_signature(node, source)
}

fn extract_docstring(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    // Python docstrings are the first expression_statement in a function/class body
    let body = node.child_by_field_name("body")?;
    let first = body.child(0)?;
    if first.kind() == "expression_statement" {
        let expr = first.child(0)?;
        if expr.kind() == "string" {
            let text = expr.utf8_text(source).ok()?;
            let cleaned = text.trim_matches('"').trim_matches('\'').trim().to_string();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        } else {
            None
        }
    } else {
        None
    }
}

/// Extract module-level docstring (first expression_statement containing a string at root level).
fn extract_module_docstring(root: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let first_child = root.child(0)?;
    if first_child.kind() == "expression_statement" {
        let expr = first_child.child(0)?;
        if expr.kind() == "string" {
            let text = expr.utf8_text(source).ok()?;
            let cleaned = text.trim_matches('"').trim_matches('\'').trim().to_string();
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        } else {
            None
        }
    } else {
        None
    }
}

#[derive(Default)]
struct PythonCallExtractionAudit {
    seen_calls: std::collections::HashSet<(usize, usize)>,
}

/// The call sites a Python file holds, split into the two kinds that reach the
/// graph by different routes.
///
/// A denominator taken off the emitted relations understates the file, because
/// a call whose callee the adapter cannot name (`handlers["render"](body)`) and
/// a call written where no callable entity owns the edge produce no
/// [`kin_model::RelationKind::Calls`] relation at all. A consumer subtracting
/// the graph's resolved edges from a short count finds no shortfall on a file
/// whose calls the graph never saw.
///
/// It has to be split rather than summed in one field, because the two halves
/// answer different questions and one of them is asymmetric in the other
/// direction. See each field.
#[derive(Default)]
struct PythonCallSiteCensus {
    /// Nodes whose kind is `call`.
    ///
    /// This is exactly the set extraction walks, so it is the one the
    /// completeness check compares against: it exceeds the number of call sites
    /// the walker visited precisely when one was missed.
    call_nodes: u64,
    sites: Vec<RelationSite>,
    /// Decorators that produce a `Calls` relation and hold no `call` node of
    /// their own, meaning every form but `@mod.name(args)`.
    ///
    /// Counted apart from `call_nodes` in both directions for a reason. Folding
    /// them in would report a file carrying a bare decorator as extraction-
    /// incomplete, because the walker never visits a `call` node for one, and
    /// that would withhold the count from a file that was measured before.
    /// Leaving them out of the total is worse: the extractor emits an edge for
    /// each, so the stamped count lands BELOW the resolved edge count, the
    /// arrival gate floors that difference at zero, and it certifies an absence
    /// over a file holding a call site the graph never saw.
    decorator_sites: u64,
}

impl PythonCallSiteCensus {
    /// Every call site the file holds, which is the number stamped on its
    /// entities.
    fn total(&self) -> u64 {
        self.call_nodes + self.decorator_sites
    }
}

/// Walk `node` once and count both halves of the census.
fn census_python_call_sites(
    node: &tree_sitter::Node,
    source: &[u8],
    census: &mut PythonCallSiteCensus,
) {
    if node.kind() == "call" {
        census.call_nodes += 1;
        census.sites.push(site_from_node(node));
    }
    if node.kind() == "decorated_definition" {
        census.decorator_sites += bare_decorator_call_sites(node, source);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        census_python_call_sites(&child, source, census);
    }
}

/// Decorators on `definition` that produce a `Calls` relation and carry no
/// `call` node of their own.
///
/// The predicate is the emitter's own, deliberately: the same
/// [`extract_decorator_payload_name`] and the same [`is_valid_callee_name`]
/// filter the `decorated_definition` arm applies before it pushes a relation,
/// and the same requirement that the definition actually decorates a function
/// or a class. Re-deriving the condition here is what would let the two sides
/// drift into disagreeing about what a decorator site is, which is the whole
/// defect this counts.
///
/// `@app.route("/")` is excluded because its own `call` node is already counted
/// and the emitter produces exactly one relation for it, so the two sides
/// already balance there.
fn bare_decorator_call_sites(definition: &tree_sitter::Node, source: &[u8]) -> u64 {
    let mut cursor = definition.walk();
    let mut decorates_a_definition = false;
    let mut sites = 0;
    for child in definition.children(&mut cursor) {
        match child.kind() {
            "function_definition" | "class_definition" => decorates_a_definition = true,
            "decorator" => {
                let names_a_callee = extract_decorator_payload_name(&child, source)
                    .is_some_and(|name| is_valid_callee_name(&name));
                if names_a_callee && !contains_call_node(&child) {
                    sites += 1;
                }
            }
            _ => {}
        }
    }
    if decorates_a_definition {
        sites
    } else {
        0
    }
}

/// Whether `node` or anything under it is a `call`.
fn contains_call_node(node: &tree_sitter::Node) -> bool {
    if node.kind() == "call" {
        return true;
    }
    // Bound rather than returned as the tail expression: the iterator borrows
    // `cursor`, and a temporary at the end of a block outlives the block's own
    // locals. `has_unobserved_call` carried the same binding for the same
    // reason before this change replaced it.
    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| contains_call_node(&child));
    found
}

struct PythonNamedCallee {
    name: String,
    resolution_proven: bool,
    /// Receiver expression as written, for an attribute call whose owning type
    /// the parser could not pin. `None` for a bare call and for a `self`/`cls`
    /// call, whose owner is already folded into `name`.
    receiver: Option<String>,
    /// The module an enclosing function body's plain `from module import
    /// name` bound this bare callee from. The call carries it as its import
    /// source, because that import, not a module-level declaration sharing
    /// the name, is what the call reaches.
    import_source: Option<String>,
    /// The bare callee names a value an enclosing function scope binds, such
    /// as a parameter, a local assignment or a loop variable. Its name says
    /// nothing about which declaration runs, so the call is recorded as a
    /// resolution gap rather than as an edge a same-named declaration could
    /// capture.
    local_value: bool,
}

/// Resolve the statically named portion of a Python callee. Parentheses are a
/// transparent expression wrapper, so `(target)(...)` has the same named
/// target as `target(...)`. A direct `self`/`cls` receiver is pinned to the
/// enclosing class. Other attribute receivers retain their historical bare
/// leaf for recall, but cannot prove which class owns that method: the linker
/// may otherwise bind a same-file free-function decoy or drop an over-cap
/// method fanout. Callers must therefore preserve the edge while downgrading
/// file-level call coverage. Those callees also carry the receiver expression
/// as written, which is what lets the linker separate a call through an object
/// from a call through an imported module. Other expression forms (conditional, subscript,
/// returned callable, lambda, and so on) do not prove one destination either.
fn extract_named_callee(
    function: &tree_sitter::Node,
    source: &[u8],
    class_ctx: Option<&str>,
    receiver_types: &PythonReceiverTypes,
    frames: &[std::rc::Rc<PyFrame>],
) -> Option<PythonNamedCallee> {
    let callee = match function.kind() {
        "parenthesized_expression" => {
            let mut cursor = function.walk();
            let mut named = function.named_children(&mut cursor);
            let inner = named.next()?;
            if named.next().is_some() {
                return None;
            }
            return extract_named_callee(&inner, source, class_ctx, receiver_types, frames);
        }
        "attribute" => {
            let attr = function
                .child_by_field_name("attribute")?
                .utf8_text(source)
                .ok()?;
            let receiver_text = function
                .child_by_field_name("object")
                .and_then(|obj| obj.utf8_text(source).ok())
                .map(str::to_string);
            let self_or_cls_receiver = function
                .child_by_field_name("object")
                .filter(|obj| obj.kind() == "identifier")
                .and_then(|obj| obj.utf8_text(source).ok())
                .is_some_and(|text| text == "self" || text == "cls");
            let declared_owner = receiver_text
                .as_deref()
                .and_then(|receiver| receiver_types.get(receiver));
            // The two-hop receiver. Nothing here declares `r.connection`, but
            // the scope declares `r: Response`, so the owner half becomes
            // `Response.connection`: the class whose body settles the second
            // hop, and the attribute to ask it about. The linker owns that
            // lookup because the declaration lives on another class in another
            // file, which is exactly why the one-hop tier could not see it.
            let declared_attribute_path = declared_owner
                .is_none()
                .then(|| python_declared_attribute_path(receiver_text.as_deref()?, receiver_types))
                .flatten();
            match (class_ctx, declared_owner, declared_attribute_path) {
                (Some(cls), _, _) if self_or_cls_receiver => PythonNamedCallee {
                    name: format!("{cls}.{attr}"),
                    resolution_proven: true,
                    receiver: None,
                    import_source: None,
                    local_value: false,
                },
                // The receiver's type is declared here, so the call names one
                // owner and arrives owner-qualified exactly as a `self.m()`
                // call does. The receiver is still carried: it is what tells
                // the linker this owner came from a declaration rather than
                // from a written path, so a type the repository does not
                // define falls back to the bare leaf instead of resolving to
                // nothing.
                (_, Some(owner), _) => PythonNamedCallee {
                    name: format!("{owner}.{attr}"),
                    resolution_proven: true,
                    receiver: receiver_text,
                    import_source: None,
                    local_value: false,
                },
                // Only half proven: the parse settles which class and which
                // attribute to ask, and whether the repository declares that
                // attribute's type is a question this file cannot answer. So
                // the call still counts against file call coverage exactly as
                // the bare leaf it would otherwise have been, and a linker that
                // finds no declaration hands it straight back to the bare-name
                // rule.
                (_, None, Some(path)) => PythonNamedCallee {
                    name: format!("{path}.{attr}"),
                    resolution_proven: false,
                    receiver: receiver_text,
                    import_source: None,
                    local_value: false,
                },
                _ => PythonNamedCallee {
                    name: attr.to_string(),
                    resolution_proven: false,
                    receiver: receiver_text,
                    import_source: None,
                    local_value: false,
                },
            }
        }
        // A bare call resolves the way Python resolves the name: in the
        // innermost function scope that binds it, then each enclosing one,
        // then the module, then the builtins. A class or function an
        // enclosing function body defines names that nested definition,
        // whose entity carries the qualified name. A plain `from m import
        // name` in an enclosing body keeps the name and carries `m`. Any
        // other local binding holds a runtime value, so the call is left to
        // its caller to record as a gap: a parameter called `helper` is not
        // the module's `def helper`. A name no function scope binds keeps
        // the name it has always had, and the linker settles it against the
        // module's own declarations, its imports and the builtin table.
        "identifier" => {
            let written = function.utf8_text(source).ok()?;
            let scoped = resolve_python_scoped_name(frames, written);
            PythonNamedCallee {
                name: match scoped {
                    Some(PythonScopedName::Definition(entity)) => entity,
                    _ => written,
                }
                .to_string(),
                resolution_proven: true,
                receiver: None,
                import_source: match scoped {
                    Some(PythonScopedName::Imported(module)) => Some(module.to_string()),
                    _ => None,
                },
                local_value: scoped == Some(PythonScopedName::Value),
            }
        }
        _ => return None,
    };
    is_valid_callee_name(&callee.name).then_some(callee)
}

/// The declared type of every receiver expression a call in this scope can be
/// written through, keyed by the receiver exactly as source spells it.
///
/// Two spellings reach it. A plain name the enclosing definition annotates
/// (`def send(self, adapter: HTTPAdapter)`, `adapter: HTTPAdapter = ...`) keys
/// on that name, and an attribute of the enclosing class the class body
/// annotates (`connection: HTTPAdapter`) keys on `self.connection` and
/// `cls.connection`, which is how such an attribute is read.
///
/// Only a declaration populates this. Nothing is inferred from an assignment's
/// right-hand side, so a name the file never annotates has no entry and its
/// calls keep the bare-leaf behaviour they had before.
type PythonReceiverTypes = std::collections::HashMap<String, String>;

/// Collect the imports written inside a module-scope guard, which bind names
/// the flat top-level walk never sees.
///
/// `if TYPE_CHECKING:` is where PEP 484 puts an import that exists only so
/// annotations can name a class without a runtime cycle, and requests puts
/// `from .adapters import HTTPAdapter` in `models.py` exactly there. A walk
/// over the module's own children saw none of them, so the file appeared not to
/// bind the name at all: [`emit_python_value_references`] admits only what a
/// file defines or imports, which meant every annotation naming such a class
/// emitted no edge of any kind. `try: import fast except ImportError: import
/// slow` is the same shape for an optional dependency.
///
/// These are kept OUT of the file's `FileImport` list and used only to admit
/// the names an annotation may bind, because a guarded import is not
/// interchangeable with a plain one. Feeding it into the linker's import map
/// widened the tier that settles a bare method name from the one owner a file
/// imports: `sessions.py` imports `HTTPAdapter` at module scope and
/// `BaseAdapter` under `TYPE_CHECKING`, both of which declare `send`, so
/// `Session.send`'s real call to `adapter.send(request)` stopped resolving at
/// all. That is a measured recall loss on the real repository, so the binding
/// is admitted exactly where it is needed and nowhere else.
///
/// A function-local import is deliberately not reached. This walks the module's
/// own statements, where a binding is visible to every annotation in the file;
/// an import inside a function body binds only in that scope.
fn collect_python_guarded_imports(
    node: &tree_sitter::Node,
    source: &[u8],
    out: &mut Vec<FileImport>,
) {
    match node.kind() {
        "import_statement" => {
            out.extend(extract_py_imports(node, source));
        }
        "import_from_statement" => {
            if let Some(import) = extract_py_from_import(node, source) {
                out.push(import);
            }
        }
        "if_statement" | "elif_clause" | "else_clause" | "try_statement" | "except_clause"
        | "finally_clause" | "block" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_python_guarded_imports(&child, source, out);
            }
        }
        _ => {}
    }
}

/// `Response.connection` for a call written through `r.connection` where the
/// scope declares `r: Response`, and `None` for every other receiver.
///
/// Exactly one attribute hop is admitted. `r.connection.send(prep)` is the
/// requests shape both hops of which are written down, `r: Response` in the
/// signature and `connection: HTTPAdapter` on the class. `a.b.c.send()` needs a
/// second declaration this returns nothing about, so it declines and the call
/// keeps the bare leaf it already had.
///
/// `self` and `cls` are excluded. The enclosing class's own attributes are
/// already keyed whole (`self.connection`) by [`python_receiver_types`], and
/// that entry wins before this is consulted; what the exclusion actually costs
/// is the rarer `def handle(self: BaseAuth, ...)`, where routing `self` through
/// the repository-wide table would resolve a call through a class the signature
/// merely annotates. That is new behaviour rather than a restored edge, this
/// lane measured no demand for it, and the fixture
/// `an_annotated_self_receiver_is_left_to_the_enclosing_class` says so, so
/// widening it later is a decision somebody makes on purpose.
fn python_declared_attribute_path(
    receiver: &str,
    receiver_types: &PythonReceiverTypes,
) -> Option<String> {
    let (root, attribute) = receiver.split_once('.')?;
    if root.is_empty() || attribute.is_empty() || attribute.contains('.') {
        return None;
    }
    if root == "self" || root == "cls" {
        return None;
    }
    let root_type = receiver_types.get(root)?;
    Some(format!("{root_type}.{attribute}"))
}

/// Collect the receiver types one `function_definition` can dispatch through.
///
/// Without this, `adapter.send(request)` reached the linker as the bare leaf
/// `send`, so `find_references(HTTPAdapter.send)` on a requests-shaped package
/// counted zero callers and held the real one as an unproven same-name
/// candidate, while the annotation naming the receiver's type sat in the graph
/// one lookup away.
fn python_receiver_types(node: &tree_sitter::Node, source: &[u8]) -> PythonReceiverTypes {
    let mut types = PythonReceiverTypes::new();
    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for param in params.named_children(&mut cursor) {
            let (Some(name), Some(annotation)) = (
                python_parameter_name(&param, source),
                param.child_by_field_name("type"),
            ) else {
                continue;
            };
            if let Some(declared) = python_annotation_type_name(&annotation, source) {
                types.insert(name, declared);
            }
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        collect_python_annotated_bindings(&body, source, "", &mut types);
    }
    if let Some(class) = python_enclosing_class(node) {
        if let Some(body) = class.child_by_field_name("body") {
            for prefix in ["self.", "cls."] {
                collect_python_annotated_bindings(&body, source, prefix, &mut types);
            }
        }
    }
    types
}

/// The name one parameter binds, for the two annotated shapes a receiver can
/// arrive as.
///
/// `typed_default_parameter` carries a `name` field; `typed_parameter` does
/// not, and holds its identifier as its first named child beside the `type`
/// field. A splat parameter (`*args: T`) binds a tuple rather than a value of
/// `T`, so its name is deliberately not returned.
fn python_parameter_name(param: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    if let Some(name) = param.child_by_field_name("name") {
        return name.utf8_text(source).ok().map(str::to_string);
    }
    let first = param.named_child(0)?;
    if first.kind() != "identifier" {
        return None;
    }
    first
        .utf8_text(source)
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The `class_definition` a method is written inside, if any.
///
/// A method's parent is its class body; a decorated method sits one
/// `decorated_definition` further out, which is why the walk is a short climb
/// rather than a single `parent()` call. A nested function inside a method
/// still reaches the class, which is correct: `self` means the same thing there.
fn python_enclosing_class<'tree>(
    node: &tree_sitter::Node<'tree>,
) -> Option<tree_sitter::Node<'tree>> {
    let mut current = node.parent();
    for _ in 0..PYTHON_CLASS_ANCESTOR_DEPTH {
        let ancestor = current?;
        if ancestor.kind() == "class_definition" {
            return Some(ancestor);
        }
        current = ancestor.parent();
    }
    None
}

/// How far above a definition its class may sit. A method's body, its own
/// definition node and one decorator wrapper are the three steps a class
/// member can be nested behind; the cap keeps the climb off unrelated
/// enclosing classes when a definition is nested deeper than that.
const PYTHON_CLASS_ANCESTOR_DEPTH: usize = 4;

/// Record every `name: Type` binding written directly in `body`, under
/// `prefix`.
///
/// Only statements at this level are read. A binding written inside a nested
/// `if` or `for` is skipped rather than hoisted, because the annotation there
/// is conditional and a receiver type has to be the one the reader can see.
fn collect_python_annotated_bindings(
    body: &tree_sitter::Node,
    source: &[u8],
    prefix: &str,
    types: &mut PythonReceiverTypes,
) {
    let mut cursor = body.walk();
    for statement in body.named_children(&mut cursor) {
        let assignment = match statement.kind() {
            "assignment" => Some(statement),
            "expression_statement" => statement
                .named_child(0)
                .filter(|child| child.kind() == "assignment"),
            _ => None,
        };
        let Some(assignment) = assignment else {
            continue;
        };
        let (Some(left), Some(annotation)) = (
            assignment.child_by_field_name("left"),
            assignment.child_by_field_name("type"),
        ) else {
            continue;
        };
        let bound = match left.kind() {
            "identifier" => left.utf8_text(source).ok().map(str::to_string),
            // `self.connection: HTTPAdapter` inside a method declares the same
            // attribute a class-body `connection: HTTPAdapter` does.
            "attribute" => python_self_attribute_path(&left, source),
            _ => None,
        };
        let (Some(bound), Some(declared)) =
            (bound, python_annotation_type_name(&annotation, source))
        else {
            continue;
        };
        if bound.contains('.') {
            types.insert(bound, declared);
        } else {
            types.insert(format!("{prefix}{bound}"), declared);
        }
    }
}

/// `self.connection` for a `self.connection` attribute node, `None` for any
/// other receiver. Only `self` and `cls` name the enclosing instance, so an
/// annotation on anything else declares a field of some other object and says
/// nothing about a call written here.
fn python_self_attribute_path(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let object = node.child_by_field_name("object")?;
    if object.kind() != "identifier" {
        return None;
    }
    let root = object.utf8_text(source).ok()?;
    if root != "self" && root != "cls" {
        return None;
    }
    let leaf = node
        .child_by_field_name("attribute")?
        .utf8_text(source)
        .ok()?;
    (!leaf.is_empty()).then(|| format!("{root}.{leaf}"))
}

/// The class name one type annotation declares, or `None` when the annotation
/// names no single class.
///
/// A bare `HTTPAdapter` and a module-qualified `adapters.HTTPAdapter` both name
/// one type, and `Optional[HTTPAdapter]` names the same type with `None` added,
/// which does not change what a call through it dispatches to. Everything else
/// stays out: a union, a container and a string forward reference each leave
/// the receiver's type undecided, and a receiver whose type is undecided must
/// keep the bare-name behaviour rather than pick one arm of it.
fn python_annotation_type_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let named = python_annotation_type_node(node, source)?;
    let text = named.utf8_text(source).ok()?;
    (!text.is_empty() && text.split('.').all(|seg| !seg.is_empty())).then(|| text.to_string())
}

/// The node inside an annotation that names its one type, or `None` when the
/// annotation names no single type.
///
/// Split out from [`python_annotation_type_name`] because a class-body
/// attribute annotation has to be matched against the reference edge the type
/// walk already emitted for it, and only the node's own byte offset identifies
/// that edge unambiguously when a class declares two attributes of one type.
/// The admitted shapes are the name's: a bare identifier, a dotted path, and
/// `Optional[T]`, which adds `None` to `T` without changing what a call
/// through it dispatches to.
fn python_annotation_type_node<'tree>(
    node: &tree_sitter::Node<'tree>,
    source: &[u8],
) -> Option<tree_sitter::Node<'tree>> {
    let inner = if node.kind() == "type" {
        node.named_child(0)?
    } else {
        *node
    };
    match inner.kind() {
        "identifier" | "attribute" => Some(inner),
        // `Optional[T]` in a TYPE position is a `generic_type`, never a
        // `subscript`: tree-sitter-python routes an annotation through its own
        // `type` rule, whose wrapper form is `identifier` plus
        // `type_parameter`. The subscript arm below therefore never matched an
        // annotation, and this function's own doc comment has claimed Optional
        // was handled since it was written. `Union[T, U]` and every other
        // multi-argument wrapper still declines here, because a receiver whose
        // type is undecided must keep the bare-name behaviour.
        "generic_type" => {
            let wrapper = inner.named_child(0)?.utf8_text(source).ok()?;
            if !matches!(wrapper, "Optional" | "typing.Optional") {
                return None;
            }
            let parameters = inner
                .named_child(1)
                .filter(|node| node.kind() == "type_parameter")?;
            let mut cursor = parameters.walk();
            let mut arguments = parameters.named_children(&mut cursor);
            let argument = arguments.next()?;
            if arguments.next().is_some() {
                return None;
            }
            python_annotation_type_node(&argument, source)
        }
        "subscript" => {
            let value = inner.child_by_field_name("value")?;
            let wrapper = value.utf8_text(source).ok()?;
            if !matches!(wrapper, "Optional" | "typing.Optional") {
                return None;
            }
            let mut cursor = inner.walk();
            let mut arguments = inner
                .children_by_field_name("subscript", &mut cursor)
                .collect::<Vec<_>>();
            let argument = arguments.pop().filter(|_| arguments.is_empty())?;
            python_annotation_type_node(&argument, source)
        }
        _ => None,
    }
}

/// Mark the reference a class body's own `attribute: Type` annotation produced
/// with the attribute it declares.
///
/// The type walk has already pushed a reference for every name the annotation
/// carries, so this stamps rather than emits: no second edge appears, and the
/// one that exists gains the half it was missing. Matching is by the type
/// node's own start byte, because a class declaring `a: T` and `b: T` produces
/// two references to one type and a name match would stamp both.
fn stamp_class_attribute_declaration(
    member: &tree_sitter::Node,
    source: &[u8],
    refs: &mut [PythonValueRef],
) {
    let statement = if member.kind() == "expression_statement" {
        match member.named_child(0) {
            Some(inner) => inner,
            None => return,
        }
    } else {
        *member
    };
    if statement.kind() != "assignment" {
        return;
    }
    let (Some(left), Some(annotation)) = (
        statement.child_by_field_name("left"),
        statement.child_by_field_name("type"),
    ) else {
        return;
    };
    // Only a plain name declares an attribute of THIS class. `self.x: T` in a
    // class body is not valid there, and a subscript or tuple target declares
    // no single attribute.
    if left.kind() != "identifier" {
        return;
    }
    let Ok(attribute) = left.utf8_text(source) else {
        return;
    };
    let Some(named) = python_annotation_type_node(&annotation, source) else {
        return;
    };
    if attribute.is_empty() {
        return;
    }
    for value_ref in refs.iter_mut() {
        if value_ref.site.start_byte == named.start_byte() {
            value_ref.attribute = Some(attribute.to_string());
        }
    }
}

/// The node ids of the `def` statements an admitted function or method owns,
/// matched by span, in which every call is already accounted for, so a later
/// walk over the file can leave them out.
///
/// Being admitted is not enough on its own. A class declared in the body has
/// code outside its methods that no walk reads (the `name = CharField()` of a
/// model a test declares), and neither does a call nested in the decorator
/// expression of a definition declared there (`@Library().simple_tag()`), so a
/// function holding either is read through.
fn walked_python_functions(
    root: &tree_sitter::Node,
    entities: &[ExtractedEntity],
    accounted: &std::collections::HashSet<(usize, usize)>,
) -> std::collections::HashSet<usize> {
    let spans: std::collections::HashSet<(usize, usize)> = entities
        .iter()
        .filter(|entity| matches!(entity.kind, EntityKind::Function | EntityKind::Method))
        .map(|entity| (entity.span.start_byte, entity.span.end_byte))
        .collect();
    let mut walked = std::collections::HashSet::new();
    let mut stack = vec![*root];
    while let Some(node) = stack.pop() {
        if node.kind() == "function_definition"
            && spans.contains(&(node.start_byte(), node.end_byte()))
            && every_python_call_accounted(&node, accounted)
        {
            walked.insert(node.id());
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    walked
}

/// Whether every `call` node below `node` is in `accounted`.
fn every_python_call_accounted(
    node: &tree_sitter::Node,
    accounted: &std::collections::HashSet<(usize, usize)>,
) -> bool {
    let mut stack = vec![*node];
    while let Some(current) = stack.pop() {
        if current.kind() == "call"
            && !accounted.contains(&(current.start_byte(), current.end_byte()))
        {
            return false;
        }
        let mut cursor = current.walk();
        stack.extend(current.children(&mut cursor));
    }
    true
}

/// Whether this `call` node is the value a `raise` throws.
///
/// Only the DIRECT operand counts. `raise Wrapper(build())` throws the wrapper;
/// `build()` is an ordinary call that happens to sit inside the same statement,
/// and marking it would demote a real hop because of its neighbour's syntax.
///
/// A `raise` target is a call edge and stays one: it is real evidence that this
/// function can throw that class. What it is not is a hop a value travels
/// along, and a trace walking data flow spends its budget as if it were. On a
/// converted `psf/requests`, nine of twelve depth-1 slots went to exception
/// constructors and the hop governing connection reuse was left out of the
/// answer entirely.
fn python_call_is_raise_target(call: &tree_sitter::Node) -> bool {
    call.parent()
        .is_some_and(|parent| parent.kind() == "raise_statement")
}

/// Extract all function/method calls within a function/method body.
///
/// A nested definition in `scope.skip` is left out entirely: its entity walks
/// its own body, so a call written there is that entity's call and not the
/// enclosing function's.
fn extract_calls_from_context(
    node: &tree_sitter::Node,
    source: &[u8],
    scope: &PyCallScope,
    relations: &mut Vec<ExtractedRelation>,
    call_audit: &mut PythonCallExtractionAudit,
) {
    let context_name = scope.context_name;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if scope.skip.contains(&child.id()) {
            continue;
        }
        if child.kind() == "call" {
            call_audit
                .seen_calls
                .insert((child.start_byte(), child.end_byte()));
            let callee = child.child_by_field_name("function").and_then(|function| {
                extract_named_callee(
                    &function,
                    source,
                    scope.self_class,
                    scope.receiver_types,
                    scope.frames,
                )
            });
            if let Some(callee) = callee.as_ref().filter(|callee| callee.local_value) {
                // The call reaches whatever the local binding holds. Its name
                // is known, so the gap withdraws only that name's certainty.
                relations.push(scoped_call_extraction_incomplete_marker(
                    context_name.into(),
                    site_from_node(&child),
                    Some(callee.name.clone()),
                ));
            } else if let Some(callee) = callee {
                match python_call_receiver_guard(&child, &callee, source, scope.module_names) {
                    guards::ReceiverGuard::Classes { guard, classes } => {
                        push_python_guarded_calls(
                            &child,
                            &guard,
                            &classes,
                            callee,
                            context_name,
                            source,
                            relations,
                        );
                    }
                    // The guard says the receiver is something this file
                    // cannot name, so no same-named member anywhere is
                    // evidence of the destination. The site stays a gap.
                    guards::ReceiverGuard::Ungrounded => {
                        relations.push(scoped_call_extraction_incomplete_marker(
                            context_name.into(),
                            site_from_node(&child),
                            Some(callee.name),
                        ));
                    }
                    guards::ReceiverGuard::Unguarded => {
                        push_python_named_call(&child, callee, context_name, source, relations);
                    }
                }
            } else {
                relations.push(scoped_call_extraction_incomplete_marker(
                    context_name.into(),
                    site_from_node(&child),
                    None,
                ));
            }
        }
        if matches!(
            child.kind(),
            "function_definition"
                | "class_definition"
                | "lambda"
                | "decorator"
                | "default_parameter"
                | "typed_default_parameter"
        ) && contains_call_node(&child)
        {
            relations.push(scoped_call_extraction_incomplete_marker(
                context_name.into(),
                site_from_node(&child),
                None,
            ));
        }
        // Recurse into child nodes. Positive extraction is unchanged; a nested
        // execution scope above cannot provide caller-specific negative evidence.
        extract_calls_from_context(&child, source, scope, relations, call_audit);
    }
}

/// The `isinstance` guard on the receiver of a member call, for a call whose
/// receiver nothing else typed. A call the parser already pinned to an owner,
/// a `raise` operand and a call through anything but a plain name are left
/// as they were.
fn python_call_receiver_guard<'tree>(
    call: &tree_sitter::Node<'tree>,
    callee: &PythonNamedCallee,
    source: &[u8],
    module_names: &guards::ModuleNames,
) -> guards::ReceiverGuard<'tree> {
    if callee.name.contains('.') || callee.receiver.is_none() || python_call_is_raise_target(call) {
        return guards::ReceiverGuard::Unguarded;
    }
    let Some(receiver) = call
        .child_by_field_name("function")
        .filter(|function| function.kind() == "attribute")
        .and_then(|function| function.child_by_field_name("object"))
    else {
        return guards::ReceiverGuard::Unguarded;
    };
    guards::receiver_guard(*call, receiver, source, module_names)
}

/// One possible-target relation per guard class, all on the call's one site,
/// plus the record naming the guard and the gap the guard cannot close: a
/// subclass passes the same guard, so no one body is certified to run.
fn push_python_guarded_calls(
    call: &tree_sitter::Node,
    guard: &tree_sitter::Node,
    classes: &[String],
    callee: PythonNamedCallee,
    context_name: &str,
    source: &[u8],
    relations: &mut Vec<ExtractedRelation>,
) {
    let site = RelationSite {
        syntactic_role: Some(crate::extract::RelationSyntacticRole::PythonGuardedReceiver),
        ..site_from_node(call)
    };
    let shape = extract_call_arg_shape(call, source);
    for class in classes {
        relations.push(ExtractedRelation {
            site: Some(site.clone()),
            receiver: callee.receiver.clone(),
            call_shape: Some(shape.clone()),
            kind: kin_model::RelationKind::Calls,
            src_name: context_name.to_string(),
            dst_name: format!("{class}.{}", callee.name),
            import_source: None,
        });
    }
    relations.push(crate::file_evidence::receiver_guard_record(
        &site_from_node(call),
        site_from_node(guard),
        guard.utf8_text(source).unwrap_or_default(),
    ));
    relations.push(scoped_call_extraction_incomplete_marker(
        context_name.into(),
        site_from_node(call),
        Some(callee.name),
    ));
}

/// The relation for a call whose callee the parser named, with the gap marker
/// when that name does not prove the destination.
fn push_python_named_call(
    call: &tree_sitter::Node,
    callee: PythonNamedCallee,
    context_name: &str,
    source: &[u8],
    relations: &mut Vec<ExtractedRelation>,
) {
    let site = RelationSite {
        syntactic_role: python_call_is_raise_target(call)
            .then_some(crate::extract::RelationSyntacticRole::RaiseTarget),
        ..site_from_node(call)
    };
    relations.push(ExtractedRelation {
        // The call expression itself, so a reference row can report the line
        // the call is written on rather than the line the caller's definition
        // starts on.
        site: Some(site),
        receiver: callee.receiver,
        call_shape: Some(extract_call_arg_shape(call, source)),
        kind: kin_model::RelationKind::Calls,
        src_name: context_name.to_string(),
        dst_name: callee.name.clone(),
        import_source: callee.import_source,
    });
    if !callee.resolution_proven {
        relations.push(scoped_call_extraction_incomplete_marker(
            context_name.into(),
            site_from_node(call),
            Some(callee.name),
        ));
    }
}

/// Extract the [`CallArgShape`] of a tree-sitter `call` node: count positional
/// arguments, collect explicit keyword-argument names, and note `*args` /
/// `**kwargs` splats. Only the call's own argument list is inspected — arguments
/// of nested calls belong to those calls, not this one. A call with no argument
/// list yields the default (empty) shape. Keyword names are sorted and
/// deduplicated so the shape is order- and duplicate-independent.
pub fn extract_call_arg_shape(call: &tree_sitter::Node, source: &[u8]) -> CallArgShape {
    let Some(arguments) = call.child_by_field_name("arguments") else {
        return CallArgShape::default();
    };

    let mut positional = 0u32;
    let mut keywords = Vec::new();
    let mut has_var_positional = false;
    let mut has_var_keyword = false;

    let mut cursor = arguments.walk();
    for arg in arguments.named_children(&mut cursor) {
        match arg.kind() {
            "keyword_argument" => {
                if let Some(name) = arg
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                {
                    keywords.push(name.to_string());
                }
            }
            "list_splat" => has_var_positional = true,
            "dictionary_splat" => has_var_keyword = true,
            "comment" => {}
            _ => positional += 1,
        }
    }

    keywords.sort();
    keywords.dedup();
    CallArgShape {
        positional,
        keywords,
        has_var_positional,
        has_var_keyword,
    }
}

/// Check if a callee name is valid (not a literal, not empty).
fn is_valid_callee_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('"')
        && !name.starts_with('\'')
        && !name.chars().all(|c| c.is_numeric())
}

/// Extract imports from `import foo`, `import foo.bar`, `import foo as f` and
/// `import alpha, beta`.
///
/// One statement can bind several modules and each names a different module
/// path, so each becomes its own [`FileImport`]. Two defects lived in the
/// single-import version this replaces, and both were silent.
///
/// An early return after the first `dotted_name` meant `import alpha, beta`
/// recorded only `alpha`, so every module after a comma was invisible to
/// resolution.
///
/// Worse, `import foo as f` produced no import at all. The aliased form nests
/// its `dotted_name` one level down inside an `aliased_import` node, so a scan
/// for a direct `dotted_name` child matched nothing and the function returned
/// `None`. An import that produces no declaration produces no edge at any
/// level, so aliased imports were absent from the graph entirely rather than
/// merely unresolved.
fn extract_py_imports(node: &tree_sitter::Node, source: &[u8]) -> Vec<FileImport> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        // `import a` puts a bare `dotted_name` under the statement; `import a as b`
        // wraps it in an `aliased_import` whose `name` field holds it.
        let (module_node, alias) = match child.kind() {
            "dotted_name" => (child, None),
            "aliased_import" => {
                let Some(name) = child.child_by_field_name("name") else {
                    continue;
                };
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(str::to_string)
                    .filter(|alias| !alias.is_empty());
                (name, alias)
            }
            _ => continue,
        };
        let module_path = module_node.utf8_text(source).unwrap_or("").to_string();
        if module_path.is_empty() {
            continue;
        }
        // An aliased module carries the alias as the local binding and the
        // module path as the original, which is the same shape
        // `extract_py_from_import` records for `from x import y as z`. A plain
        // import leaves `original_name` unset, exactly as before, because the
        // per-file import map that backs call resolution reads this field and
        // an unnecessary change there would move behaviour this fix does not
        // mean to touch.
        let (local_name, original_name) = match alias {
            Some(alias) => (alias, Some(module_path.clone())),
            None => (module_path.clone(), None),
        };
        out.push(FileImport {
            site: crate::adapter::site_from_node(node),
            module_path,
            specifiers: vec![ImportedName {
                local_name,
                original_name,
                is_default: false,
                site: Some(crate::adapter::site_from_node(&child)),
            }],
        });
    }
    out
}

/// Extract import from `from foo import bar, baz as qux`.
fn extract_py_from_import(node: &tree_sitter::Node, source: &[u8]) -> Option<FileImport> {
    let module_name = node.child_by_field_name("module_name")?;
    let module_path = module_name.utf8_text(source).unwrap_or("").to_string();
    if module_path.is_empty() {
        return None;
    }

    let mut specifiers = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                // Skip the module name itself (already captured)
                if child.id() != module_name.id() {
                    let name = child.utf8_text(source).unwrap_or("").to_string();
                    if !name.is_empty() {
                        specifiers.push(ImportedName {
                            local_name: name,
                            original_name: None,
                            is_default: false,
                            site: Some(crate::adapter::site_from_node(&child)),
                        });
                    }
                }
            }
            "aliased_import" => {
                let orig = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();
                let alias = child
                    .child_by_field_name("alias")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("")
                    .to_string();
                if !alias.is_empty() {
                    specifiers.push(ImportedName {
                        local_name: alias,
                        original_name: Some(orig),
                        is_default: false,
                        site: Some(crate::adapter::site_from_node(&child)),
                    });
                }
            }
            _ => {}
        }
    }

    Some(FileImport {
        site: crate::adapter::site_from_node(node),
        module_path,
        specifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`is_python_builtin_name`] binary-searches the table, which is only
    /// correct while the table is sorted and holds no duplicate. A hand edit
    /// that breaks either would make the lookup miss names silently, which
    /// reads exactly like a name the interpreter does not bind.
    #[test]
    fn the_builtin_table_is_sorted_deduped_and_searchable() {
        let mut sorted = PYTHON_BUILTIN_NAMES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.as_slice(),
            PYTHON_BUILTIN_NAMES,
            "the builtins table must stay sorted and deduped for binary search"
        );
        for name in PYTHON_BUILTIN_NAMES {
            assert!(
                is_python_builtin_name(name),
                "every table entry must be findable, missed {name}"
            );
        }
    }

    /// The table is the full `dir(builtins)` surface, not the handful of names
    /// a reader remembers. The ones checked here are the ones a call-resolution
    /// gate needs and a hand-typed list omits.
    #[test]
    fn the_builtin_table_covers_the_names_a_hand_typed_list_would_miss() {
        for name in [
            "open",
            "print",
            "len",
            "range",
            "str",
            "list",
            "format",
            "id",
            "filter",
            "next",
            "exit",
            "vars",
            "aiter",
            "ValueError",
            "StopIteration",
            "NotImplemented",
        ] {
            assert!(is_python_builtin_name(name), "{name} must be a builtin");
        }
        for name in [
            "NoteStore",
            "parse_file",
            "open_store",
            "ingest_directory",
            "os",
            "",
        ] {
            assert!(
                !is_python_builtin_name(name),
                "{name} must not be read as a builtin"
            );
        }
    }

    /// Depth-first search for the first `call` node in a parsed tree.
    fn first_call_node<'a>(node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
        if node.kind() == "call" {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = first_call_node(child) {
                return Some(found);
            }
        }
        None
    }

    fn shape_of(src: &[u8]) -> CallArgShape {
        let adapter = PythonAdapter;
        let tree = adapter.parse(src).unwrap();
        let call = first_call_node(tree.root_node()).expect("a call node");
        extract_call_arg_shape(&call, src)
    }

    #[test]
    fn call_arg_shape_positional_only() {
        let shape = shape_of(b"f(a, b, c)");
        assert_eq!(shape.positional, 3);
        assert!(shape.keywords.is_empty());
        assert!(!shape.has_var_positional);
        assert!(!shape.has_var_keyword);
    }

    #[test]
    fn call_arg_shape_keyword_and_mixed() {
        let shape = shape_of(b"f(a, kw=c, other=d)");
        assert_eq!(shape.positional, 1);
        assert_eq!(shape.keywords, vec!["kw".to_string(), "other".to_string()]);
        assert!(!shape.has_var_keyword);
    }

    #[test]
    fn call_arg_shape_star_and_double_star() {
        let shape = shape_of(b"f(a, *args, **kwargs)");
        assert_eq!(shape.positional, 1);
        assert!(shape.has_var_positional);
        assert!(shape.has_var_keyword);
        assert!(shape.keywords.is_empty());
    }

    #[test]
    fn call_arg_shape_empty_call_is_default() {
        assert_eq!(shape_of(b"f()"), CallArgShape::default());
    }

    #[test]
    fn call_arg_shape_keyword_names_sorted_and_deduped() {
        let shape = shape_of(b"f(zebra=1, alpha=2)");
        assert_eq!(
            shape.keywords,
            vec!["alpha".to_string(), "zebra".to_string()]
        );
    }

    #[test]
    fn call_arg_shape_nested_call_counts_outer_args_only() {
        // The inner `y=1` keyword belongs to `g`, not the outer `f` call.
        let shape = shape_of(b"f(g(x, y=1), z)");
        assert_eq!(shape.positional, 2);
        assert!(
            shape.keywords.is_empty(),
            "inner keyword must not leak to the outer call"
        );
    }

    #[test]
    fn parse_python_function() {
        let adapter = PythonAdapter;
        let source = b"def greet(name):\n    return f'Hello {name}'";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert!(matches!(output.parse_state, ParseState::Valid));
        let functions: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Function)
            .collect();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "greet");
    }

    #[test]
    fn declaration_signature_preserves_default_literal_bytes() {
        let adapter = PythonAdapter;
        let source = br#"def target(value="a  b ) , c", other=(1, 2)):
    return value, other
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let target = output
            .entities
            .iter()
            .find(|entity| entity.name == "target")
            .expect("target entity");
        assert_eq!(
            target.signature,
            r#"def target(value="a  b ) , c", other=(1, 2))"#,
            "declaration canonicalization may normalize outer formatting but must copy a literal byte-for-byte"
        );
    }

    #[test]
    fn parse_python_uppercase_constant() {
        let adapter = PythonAdapter;
        let source = b"PROBE_SECRET_abcd1234 = 'uuid'\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let constants: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .collect();
        assert_eq!(
            constants.len(),
            1,
            "expected 1 constant, got {:?}",
            constants
        );
        assert_eq!(constants[0].name, "PROBE_SECRET_abcd1234");
    }

    /// FIR-2509: a single-word module-level constant was never an entity while
    /// its multi-word siblings in the same package were, so `semantic_search`
    /// certified `safe_to_conclude_absent: true` over a constant plainly in the
    /// file. The multi-word names are asserted beside the single-word ones so a
    /// fix in one direction cannot quietly cost the other.
    #[test]
    fn parse_python_single_word_uppercase_constants_are_entities() {
        let adapter = PythonAdapter;
        let source = br##"SCHEMA = """
CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);
"""
LIMIT = 50
TIMEOUT = 30
DEBUG = False
PORT = 8080
VERSION = "1.0"
HTTP2 = True
TAG_RE = "#(\w+)"
DEFAULT_REDIRECT_LIMIT = 30
"##;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("storage.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let mut names: Vec<&str> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .map(|e| e.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "DEBUG",
                "DEFAULT_REDIRECT_LIMIT",
                "HTTP2",
                "LIMIT",
                "PORT",
                "SCHEMA",
                "TAG_RE",
                "TIMEOUT",
                "VERSION",
            ],
            "every all-uppercase module-level assignment is a constant, with or \
             without a word separator; got {names:?}"
        );
        let schema = output
            .entities
            .iter()
            .find(|e| e.name == "SCHEMA")
            .expect("SCHEMA is the single-word triple-quoted constant FIR-2509 named");
        assert_eq!(schema.kind, EntityKind::Constant);
        assert_eq!(
            schema.span.start_line, 0,
            "SCHEMA's span must name its own assignment (tree-sitter rows are 0-based, \
             so source line 1) rather than the module's"
        );
    }

    /// FIR-2481: `codes = LookupDict(name="status_codes")` in requests produced
    /// no entity, so `kin impact codes` retargeted to the enclosing module and
    /// answered "No local downstream impact found" for a name three files
    /// import. The uppercase sibling in the same file is asserted beside it
    /// because the case is the only thing that distinguished them.
    #[test]
    fn parse_python_module_level_call_binding_is_an_entity() {
        let adapter = PythonAdapter;
        let source = br#"class LookupDict(dict):
    pass


codes = LookupDict(name="status_codes")
session = build_session()
awaited = await make_client()
wrapped = (make_wrapped())
annotated: LookupDict = LookupDict(name="annotated")
DEFAULT_REDIRECT_LIMIT = 30
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("status_codes.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let mut names: Vec<&str> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .map(|e| e.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "DEFAULT_REDIRECT_LIMIT",
                "annotated",
                "awaited",
                "codes",
                "session",
                "wrapped",
            ],
            "a module-level name bound to a call is an entity whatever its case, \
             including through `await` and parentheses; got {names:?}"
        );
        let codes = output
            .entities
            .iter()
            .find(|e| e.name == "codes")
            .expect("codes is the lowercase call-bound constant FIR-2481 named");
        assert_eq!(codes.kind, EntityKind::Constant);
        assert_eq!(
            codes.span.start_line, 4,
            "codes' span must name its own assignment (tree-sitter rows are 0-based, \
             so source line 5) rather than the module's"
        );
    }

    /// The other direction of both rules. A lowercase module-level name with no
    /// call behind it, a single uppercase character, a destructuring target and
    /// an attribute write all stay out, so neither rule is a licence to promote
    /// every assignment in the file.
    #[test]
    fn parse_python_module_level_non_constants_stay_out() {
        let adapter = PythonAdapter;
        let source = br#"schema = "notes"
total = 0
_cache = {}
T = 1
E = "x"
first, second = make_pair()
holder.attr = make_attr()
holder["key"] = make_item()
__all__ = ["schema"]
KEPT = make_kept()
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("scope.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let mut names: Vec<&str> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .map(|e| e.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["KEPT"],
            "only the uppercase call-bound name qualifies; a lowercase literal \
             binding, a single character, a destructuring and an attribute write \
             must not become constants; got {names:?}"
        );
    }

    /// Scope, in both places the rules must not reach. A class attribute and a
    /// function-local carry exactly the spellings the two new rules admit at
    /// module level, and neither may produce a constant.
    #[test]
    fn parse_python_constant_rules_do_not_reach_class_or_function_scope() {
        let adapter = PythonAdapter;
        let source = br#"class Store:
    SCHEMA = "create table"
    codes = LookupDict(name="attr")

    def build(self):
        LIMIT = 50
        rows = fetch_rows()
        return LIMIT, rows


def helper():
    TIMEOUT = 30
    client = make_client()
    return TIMEOUT, client


MODULE_LIMIT = 50
module_client = make_client()
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("scope.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let mut names: Vec<&str> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Constant)
            .map(|e| e.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["MODULE_LIMIT", "module_client"],
            "only the two module-level bindings are constants; the class \
             attributes SCHEMA and codes and the locals LIMIT, rows, TIMEOUT and \
             client must stay out; got {names:?}"
        );
    }

    /// The reference edge the impact answer is built from. Once `codes` is an
    /// entity, a consumer that imports it and reads a member off it references
    /// the name rather than nothing, which is what `kin impact codes` walks.
    #[test]
    fn parse_python_call_bound_constant_receives_reference_edges() {
        let adapter = PythonAdapter;
        let source = br#"from .status_codes import codes


class Response:
    def is_redirect(self):
        return codes.moved
"#;
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("models.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let refs: Vec<(&str, &str)> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::References)
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect();
        assert!(
            refs.contains(&("Response.is_redirect", "codes")),
            "the consumer must reference `codes` by name so the impact walk can \
             reach it; got {refs:?}"
        );
    }

    #[test]
    fn parse_python_function_calls() {
        let adapter = PythonAdapter;
        let source = b"def foo():\n    bar()\n    baz(1, 2)";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].src_name, "foo");
        assert_eq!(calls[0].dst_name, "bar");
        assert_eq!(calls[1].src_name, "foo");
        assert_eq!(calls[1].dst_name, "baz");
    }

    #[test]
    fn parse_python_import() {
        let adapter = PythonAdapter;
        let source = b"import os";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.imports.len(), 1);
        assert_eq!(output.imports[0].module_path, "os");
        assert_eq!(output.imports[0].specifiers.len(), 1);
        assert_eq!(output.imports[0].specifiers[0].local_name, "os");
    }

    #[test]
    fn parse_python_from_import() {
        let adapter = PythonAdapter;
        let source = b"from os import path";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        assert_eq!(output.imports.len(), 1);
        assert_eq!(output.imports[0].module_path, "os");
        assert_eq!(output.imports[0].specifiers.len(), 1);
        assert_eq!(output.imports[0].specifiers[0].local_name, "path");
    }

    #[test]
    fn parse_python_class() {
        let adapter = PythonAdapter;
        let source = b"class Dog(Animal):\n    def bark(self):\n        pass";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Class)
            .collect();
        assert_eq!(classes.len(), 1);
        assert_eq!(classes[0].name, "Dog");

        let methods: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Method)
            .collect();
        assert_eq!(methods.len(), 1);
        assert_eq!(methods[0].name, "Dog.bark");
    }

    #[test]
    fn extract_function_docstring() {
        let adapter = PythonAdapter;
        let source =
            b"def greet(name):\n    \"\"\"Say hello to someone.\"\"\"\n    return f'Hello {name}'";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "greet")
            .expect("should find greet");
        assert_eq!(func.doc_summary.as_deref(), Some("Say hello to someone."));
    }

    #[test]
    fn extract_class_docstring() {
        let adapter = PythonAdapter;
        let source =
            b"class Dog:\n    \"\"\"A loyal companion.\"\"\"\n    def bark(self):\n        pass";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let cls = output
            .entities
            .iter()
            .find(|e| e.name == "Dog")
            .expect("should find Dog");
        assert_eq!(cls.doc_summary.as_deref(), Some("A loyal companion."));
    }

    #[test]
    fn no_docstring_yields_none() {
        let adapter = PythonAdapter;
        let source = b"def bare():\n    pass";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let func = output
            .entities
            .iter()
            .find(|e| e.name == "bare")
            .expect("should find bare");
        assert!(func.doc_summary.is_none());
    }

    #[test]
    fn self_calls_qualified_with_enclosing_class() {
        let adapter = PythonAdapter;
        let source =
            b"class Foo:\n    def run(self):\n        self.process()\n        self.helper(1)";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].src_name, "Foo.run");
        assert_eq!(calls[0].dst_name, "Foo.process");
        assert_eq!(calls[1].dst_name, "Foo.helper");
    }

    #[test]
    fn cls_calls_qualified_with_enclosing_class() {
        let adapter = PythonAdapter;
        let source = b"class Foo:\n    @classmethod\n    def make(cls):\n        cls.create()";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let body_calls: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls && r.dst_name != "classmethod")
            .collect();
        assert_eq!(body_calls.len(), 1);
        assert_eq!(body_calls[0].dst_name, "Foo.create");
    }

    #[test]
    fn non_self_receivers_stay_bare() {
        let adapter = PythonAdapter;
        let source = b"class Foo:\n    def run(self):\n        obj.render()\n        self.store.save()\n\ndef free():\n    conn.close()";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let dsts: Vec<&str> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Calls)
            .map(|r| r.dst_name.as_str())
            .collect();
        // `obj.render()` has an unknown receiver type; `self.store.save()`
        // dispatches through an attribute, not the class itself — both stay
        // bare. Only direct self./cls. receivers gain the class qualifier.
        assert_eq!(dsts, vec!["render", "save", "close"]);
    }

    #[test]
    fn init_py_emits_module_entity() {
        let adapter = PythonAdapter;
        let source = b"\"\"\"Flask web framework.\"\"\"\nfrom .app import Flask";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("flask/__init__.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let modules: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.kind == EntityKind::Module)
            .collect();
        assert_eq!(
            modules.len(),
            1,
            "expected 1 Module entity, got {:?}",
            modules
        );
        assert_eq!(modules[0].name, "flask");
        assert_eq!(modules[0].visibility, Visibility::Public);
        assert_eq!(
            modules[0].doc_summary.as_deref(),
            Some("Flask web framework.")
        );
    }

    #[test]
    fn enum_class_extends_relation() {
        let adapter = PythonAdapter;
        let source = b"class Color(Enum):\n    RED = 1\n    GREEN = 2";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("test.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let extends: Vec<_> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Extends)
            .collect();
        assert_eq!(extends.len(), 1);
        assert_eq!(extends[0].src_name, "Color");
        assert_eq!(extends[0].dst_name, "Enum");
        let classes: Vec<_> = output
            .entities
            .iter()
            .filter(|e| e.name == "Color")
            .collect();
        assert_eq!(classes[0].kind, EntityKind::EnumDef);
    }

    /// A subscripted base names the class it extends: `Confirm(PromptBase[bool])`
    /// extends `PromptBase`, so the linker can find the base and record that
    /// `Confirm.process_response` overrides `PromptBase.process_response`.
    /// A keyword such as `metaclass=` configures the class and is no base.
    #[test]
    fn subscripted_bases_extend_the_class_they_subscript() {
        let adapter = PythonAdapter;
        let source = b"import typing as t\n\
class PromptBase(t.Generic[T]):\n    def process_response(self, value): ...\n\
class Confirm(PromptBase[bool], metaclass=Meta):\n    def process_response(self, value): ...\n\
class Table(Mapping[str, list[int]]):\n    pass\n";
        let tree = adapter.parse(source).unwrap();
        let file_id = FilePathId::new("prompt.py");
        let output = adapter.extract(&tree, source, &file_id).unwrap();
        let extends: Vec<(&str, &str)> = output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::Extends)
            .map(|r| (r.src_name.as_str(), r.dst_name.as_str()))
            .collect();
        assert_eq!(
            extends,
            vec![
                ("PromptBase", "t.Generic"),
                ("Confirm", "PromptBase"),
                ("Table", "Mapping"),
            ]
        );
    }

    /// Value references made by `path`, as `(src, dst)` pairs.
    fn value_refs(path: &str, source: &str) -> Vec<(String, String)> {
        let adapter = PythonAdapter;
        let bytes = source.as_bytes();
        let tree = adapter.parse(bytes).unwrap();
        let output = adapter
            .extract(&tree, bytes, &FilePathId::new(path))
            .unwrap();
        output
            .relations
            .iter()
            .filter(|r| r.kind == kin_model::RelationKind::References)
            .map(|r| (r.src_name.clone(), r.dst_name.clone()))
            .collect()
    }

    #[test]
    fn a_function_passed_as_a_keyword_argument_is_referenced() {
        // The reported shape: an argparse subcommand wired by name. `cmd_ingest`
        // is never called anywhere, so without this edge it owns none at all.
        let refs = value_refs(
            "cli.py",
            "def cmd_ingest(args):\n    return 1\n\ndef main():\n    ingest.set_defaults(func=cmd_ingest)\n",
        );
        assert!(
            refs.contains(&("main".to_string(), "cmd_ingest".to_string())),
            "expected main -> cmd_ingest, got {refs:?}"
        );
    }

    #[test]
    fn every_value_position_that_names_a_local_entity_is_referenced() {
        let refs = value_refs(
            "wire.py",
            concat!(
                "def handler(args):\n    return 1\n\n",
                "def by_argument():\n    register(handler)\n\n",
                "def by_assignment():\n    chosen = handler\n    return chosen\n\n",
                "def by_collection():\n    return [handler]\n\n",
                "def by_mapping():\n    return {\"ingest\": handler}\n\n",
                "def by_default(fn=handler):\n    return fn\n\n",
                "def by_return():\n    return handler\n",
            ),
        );
        for src in [
            "by_argument",
            "by_assignment",
            "by_collection",
            "by_mapping",
            "by_default",
            "by_return",
        ] {
            assert!(
                refs.contains(&(src.to_string(), "handler".to_string())),
                "expected {src} -> handler, got {refs:?}"
            );
        }
    }

    #[test]
    fn attribute_access_on_a_module_constant_references_the_constant() {
        // `TAG_RE.findall(...)` reads TAG_RE. The `.findall` leaf names a method
        // of the compiled regex, not a symbol of this file.
        let refs = value_refs(
            "parsing.py",
            "TAG_RE = compile(\"#\")\n\ndef extract_tags(text):\n    return TAG_RE.findall(text)\n",
        );
        assert!(
            refs.contains(&("extract_tags".to_string(), "TAG_RE".to_string())),
            "expected extract_tags -> TAG_RE, got {refs:?}"
        );
        assert!(
            !refs.iter().any(|(_, dst)| dst == "findall"),
            "the attribute leaf is not a symbol of this file, got {refs:?}"
        );
    }

    #[test]
    fn an_imported_constant_read_through_an_attribute_is_referenced_by_its_bare_name() {
        // Cross-file: the linker binds this through the file's import table, so
        // the destination must be the imported name, not a dotted form.
        let refs = value_refs(
            "cli.py",
            "from parsing import TAG_RE\n\ndef build_parser():\n    return TAG_RE.pattern\n",
        );
        assert!(
            refs.contains(&("build_parser".to_string(), "TAG_RE".to_string())),
            "expected build_parser -> TAG_RE, got {refs:?}"
        );
    }

    #[test]
    fn a_constant_reached_through_a_module_import_keeps_its_module_qualifier() {
        // `import parsing` binds the module; `parsing.TAG_RE` names a member of
        // it. The dotted form is what the linker's namespace-member tier
        // resolves, and reducing it to the bare leaf would let a same-named
        // symbol in any other file answer for it.
        let refs = value_refs(
            "cli.py",
            "import parsing\n\ndef build_parser():\n    return parsing.TAG_RE\n",
        );
        assert!(
            refs.contains(&("build_parser".to_string(), "parsing.TAG_RE".to_string())),
            "expected build_parser -> parsing.TAG_RE, got {refs:?}"
        );
    }

    #[test]
    fn import_source_is_annotated_on_a_value_reference() {
        let adapter = PythonAdapter;
        let source =
            b"from parsing import TAG_RE\n\ndef build_parser():\n    return TAG_RE.pattern\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("cli.py"))
            .unwrap();
        let annotated = output
            .relations
            .iter()
            .find(|r| r.kind == kin_model::RelationKind::References && r.dst_name == "TAG_RE")
            .expect("the value reference exists");
        assert_eq!(annotated.import_source.as_deref(), Some("parsing"));
    }

    #[test]
    fn a_module_scope_reference_is_sourced_from_the_module() {
        let refs = value_refs(
            "cli.py",
            "def cmd_ingest(args):\n    return 1\n\nHANDLERS = {\"ingest\": cmd_ingest}\n",
        );
        assert!(
            refs.contains(&("cli".to_string(), "cmd_ingest".to_string())),
            "expected cli -> cmd_ingest, got {refs:?}"
        );
    }

    #[test]
    fn a_class_body_assignment_references_from_its_field() {
        let refs = value_refs(
            "cli.py",
            "def handler(args):\n    return 1\n\nclass Router:\n    default = handler\n",
        );
        assert!(
            refs.contains(&("Router.default".to_string(), "handler".to_string())),
            "expected Router.default -> handler, got {refs:?}"
        );
    }

    #[test]
    fn a_local_binding_shadows_a_module_level_name_of_the_same_name() {
        // `handler` inside `run` is a local, so reading it says nothing about
        // the module-level function. Without the shadow rule this edge would
        // make genuinely dead code look alive.
        let refs = value_refs(
            "cli.py",
            "def handler(args):\n    return 1\n\ndef run(handler):\n    return handler\n\ndef other():\n    handler = 1\n    return handler\n",
        );
        assert!(
            !refs.contains(&("run".to_string(), "handler".to_string())),
            "a parameter shadows the module scope, got {refs:?}"
        );
        assert!(
            !refs.contains(&("other".to_string(), "handler".to_string())),
            "a local assignment shadows the module scope, got {refs:?}"
        );
    }

    #[test]
    fn a_global_declaration_lifts_the_local_shadow() {
        let refs = value_refs(
            "counters.py",
            "TOTAL_SEEN = 0\n\ndef bump():\n    global TOTAL_SEEN\n    TOTAL_SEEN = TOTAL_SEEN + 1\n",
        );
        assert!(
            refs.contains(&("bump".to_string(), "TOTAL_SEEN".to_string())),
            "expected bump -> TOTAL_SEEN, got {refs:?}"
        );
    }

    #[test]
    fn a_name_this_file_neither_defines_nor_imports_emits_no_reference() {
        // Locals, parameters and builtins are the bulk of every value position.
        // Emitting them would let the linker bind a local called `config` to an
        // unrelated module-level `config` in some other file, by name alone.
        let refs = value_refs(
            "cli.py",
            "def run(payload):\n    total = len(payload)\n    return total\n",
        );
        assert!(refs.is_empty(), "expected no references, got {refs:?}");
    }

    #[test]
    fn an_unreferenced_function_still_owns_no_edge() {
        // The opposite direction: the rows that should stay on a dead-code list
        // must keep reporting nothing.
        let refs = value_refs(
            "parsing.py",
            "def unused_helper(text):\n    return text\n\ndef used_helper(text):\n    return text\n\ndef caller():\n    return used_helper\n",
        );
        assert!(
            refs.contains(&("caller".to_string(), "used_helper".to_string())),
            "expected caller -> used_helper, got {refs:?}"
        );
        assert!(
            !refs.iter().any(|(_, dst)| dst == "unused_helper"),
            "unused_helper is named nowhere, got {refs:?}"
        );
    }

    #[test]
    fn a_self_reference_emits_no_edge() {
        let refs = value_refs(
            "rec.py",
            "def wrapper(n):\n    if n:\n        return wrapper\n    return None\n",
        );
        assert!(
            !refs.contains(&("wrapper".to_string(), "wrapper".to_string())),
            "a self-reference establishes no reachability, got {refs:?}"
        );
    }

    /// One name read twice is one edge with two sites.
    ///
    /// The parser emits a record per site, because a reference row reports every
    /// line the name was read on and the site cannot be recovered downstream.
    /// They still resolve to ONE graph edge, whose id is derived from
    /// (src, dst, kind), so this is a per-site record rather than a duplicate
    /// edge.
    #[test]
    fn a_repeated_reference_emits_one_edge_with_a_site_each() {
        let source = "def handler(args):\n    return 1\n\ndef wire():\n    a.set_defaults(func=handler)\n    b.set_defaults(func=handler)\n";
        let refs = value_refs("cli.py", source);
        let hits = refs
            .iter()
            .filter(|(src, dst)| src == "wire" && dst == "handler")
            .count();
        assert_eq!(hits, 2, "one record per reference site, got {refs:?}");

        let adapter = PythonAdapter;
        let bytes = source.as_bytes();
        let tree = adapter.parse(bytes).unwrap();
        let output = adapter
            .extract(&tree, bytes, &FilePathId::new("cli.py"))
            .unwrap();
        let sites: std::collections::BTreeSet<usize> = output
            .relations
            .iter()
            .filter(|r| {
                r.kind == kin_model::RelationKind::References
                    && r.src_name == "wire"
                    && r.dst_name == "handler"
            })
            .filter_map(|r| r.site.as_ref().map(|site| site.start_byte))
            .collect();
        assert_eq!(
            sites.len(),
            2,
            "the two reads must be two distinct positions, got {sites:?}"
        );
    }

    #[test]
    fn a_bare_decorator_stays_a_call_edge_and_gains_no_duplicate_reference() {
        // A decorator applies its target at definition time, so it is already a
        // Calls edge. Adding a second References edge for the same site would
        // double-count one use.
        let adapter = PythonAdapter;
        let source =
            b"def register(fn):\n    return fn\n\n@register\ndef handler():\n    return 1\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("cli.py"))
            .unwrap();
        assert!(
            output.relations.iter().any(|r| {
                r.kind == kin_model::RelationKind::Calls
                    && r.src_name == "handler"
                    && r.dst_name == "register"
            }),
            "the decorator keeps its Calls edge"
        );
        assert!(
            !output
                .relations
                .iter()
                .any(|r| r.kind == kin_model::RelationKind::References && r.dst_name == "register"),
            "no duplicate reference for the same decorator site"
        );
    }

    #[test]
    fn a_value_reference_does_not_change_call_coverage() {
        // The call-extraction audit records what the CALL walk could not
        // represent. Routing value references through it would report a file as
        // call-incomplete for reading a constant.
        let adapter = PythonAdapter;
        let source = b"HANDLER = 1\n\ndef run():\n    return HANDLER\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("cov.py"))
            .unwrap();
        assert!(
            !output
                .relations
                .iter()
                .any(crate::extract::is_call_extraction_incomplete_marker),
            "reading a constant is not an unrepresented call"
        );
    }

    /// Every `(src, dst)` the adapter emits as a `References` edge for `src`,
    /// sorted so an assertion reads as a set rather than a walk order.
    fn references_in(src: &str) -> Vec<(String, String)> {
        let adapter = PythonAdapter;
        let bytes = src.as_bytes();
        let tree = adapter.parse(bytes).expect("fixture parses");
        let output = adapter
            .extract(&tree, bytes, &FilePathId::new("mod.py"))
            .expect("fixture extracts");
        let mut edges: Vec<(String, String)> = output
            .relations
            .iter()
            .filter(|rel| rel.kind == kin_model::RelationKind::References)
            .map(|rel| (rel.src_name.clone(), rel.dst_name.clone()))
            .collect();
        edges.sort();
        edges
    }

    #[test]
    fn a_raise_marks_the_class_it_throws_and_nothing_else() {
        // FIR-2642. `raise SSLError(...)` is a real call edge and real evidence
        // that this function can throw, and it is NOT a hop a value travels
        // along. Only the DIRECT operand is marked: `raise Wrapper(build())`
        // throws the wrapper, and demoting `build()` for its neighbour's syntax
        // would cost the recall the marker exists to protect.
        use crate::extract::RelationSyntacticRole;
        let adapter = PythonAdapter;
        let source =
            b"def go(x):\n    if x:\n        raise Wrapper(build())\n    return SSLError()\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("m.py"))
            .unwrap();
        let mut marked: Vec<(&str, bool)> = output
            .relations
            .iter()
            .filter(|rel| rel.kind == kin_model::RelationKind::Calls)
            .map(|rel| {
                (
                    rel.dst_name.as_str(),
                    rel.site
                        .as_ref()
                        .and_then(|site| site.syntactic_role)
                        .is_some_and(|role| role == RelationSyntacticRole::RaiseTarget),
                )
            })
            .collect();
        marked.sort();
        assert_eq!(
            marked,
            vec![("SSLError", false), ("Wrapper", true), ("build", false)],
            "the raised class is marked, the call inside its arguments is not, \
             and a constructor called outside a raise is not"
        );
    }

    #[test]
    fn a_type_checking_guarded_import_binds_the_name_its_annotation_uses() {
        // PEP 484 says an import that exists only for annotations belongs
        // behind `if TYPE_CHECKING:`, and requests puts
        // `from .adapters import HTTPAdapter` in `models.py` exactly there. The
        // import walk read the module's own children only, so the file bound
        // nothing, the emit filter dropped the annotation, and a class used
        // only in type positions had zero inbound references anywhere in the
        // repository.
        let edges = references_in(
            "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from adapters import HTTPAdapter\n\n\nclass Response:\n    connection: HTTPAdapter\n",
        );
        assert!(
            edges.contains(&("Response.connection".to_string(), "HTTPAdapter".to_string())),
            "a guarded import must bind the name its annotation reads, got {edges:?}"
        );
    }

    #[test]
    fn an_import_behind_an_import_error_guard_binds_too() {
        // The optional-dependency spelling of the same shape.
        let edges = references_in(
            "try:\n    from fast import Codec\nexcept ImportError:\n    from slow import Codec\n\n\nclass Frame:\n    codec: Codec\n",
        );
        assert!(
            edges.contains(&("Frame.codec".to_string(), "Codec".to_string())),
            "an import behind an ImportError guard must bind, got {edges:?}"
        );
    }

    #[test]
    fn a_guarded_import_stays_out_of_the_files_import_list() {
        // The bound, and it is measured rather than stylistic. Feeding a
        // guarded import into `FileImport` widened the linker tier that settles
        // a bare method name from the one owner a file imports: real
        // `sessions.py` imports `HTTPAdapter` at module scope and `BaseAdapter`
        // under `TYPE_CHECKING`, both declare `send`, and
        // `Session.send`'s call to `adapter.send(request)` stopped resolving to
        // anything. A guarded import admits a name for annotations and nothing
        // else.
        let adapter = PythonAdapter;
        let source = b"from typing import TYPE_CHECKING\nfrom adapters import HTTPAdapter\n\nif TYPE_CHECKING:\n    from adapters import BaseAdapter\n\n\nclass Session:\n    fallback: BaseAdapter\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("sessions.py"))
            .unwrap();
        let bound: Vec<&str> = output
            .imports
            .iter()
            .flat_map(|imp| imp.specifiers.iter())
            .map(|spec| spec.local_name.as_str())
            .collect();
        assert!(
            bound.contains(&"HTTPAdapter"),
            "a module-scope import is still an import, got {bound:?}"
        );
        assert!(
            !bound.contains(&"BaseAdapter"),
            "a guarded import must not enter the file's import list, got {bound:?}"
        );
        let edges: Vec<(String, String)> = output
            .relations
            .iter()
            .filter(|rel| rel.kind == kin_model::RelationKind::References)
            .map(|rel| (rel.src_name.clone(), rel.dst_name.clone()))
            .collect();
        assert!(
            edges.contains(&("Session.fallback".to_string(), "BaseAdapter".to_string())),
            "and it must still bind the annotation that reads it, got {edges:?}"
        );
    }

    #[test]
    fn a_function_local_import_is_not_a_module_binding() {
        // The other side of the bound. An import inside a function body binds
        // in that scope only, so it must not admit a module-scope annotation.
        let edges = references_in(
            "def load():\n    from adapters import HTTPAdapter\n\n    return HTTPAdapter\n\n\nclass Response:\n    connection: HTTPAdapter\n",
        );
        assert!(
            !edges.contains(&("Response.connection".to_string(), "HTTPAdapter".to_string())),
            "a function-local import must not bind a class-body annotation, got {edges:?}"
        );
    }

    #[test]
    fn a_class_body_annotation_carries_the_attribute_it_declares() {
        // The declaration half of a two-hop receiver. The reference edge alone
        // cannot say WHICH attribute holds the type, and without that the
        // linker cannot join `r.connection.send(...)` under `r: Response`.
        let adapter = PythonAdapter;
        let source =
            b"from adapters import HTTPAdapter\n\n\nclass Response:\n    connection: HTTPAdapter\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("models.py"))
            .unwrap();
        let declared: Vec<(&str, Option<&str>, &str)> = output
            .relations
            .iter()
            .filter(|rel| rel.kind == kin_model::RelationKind::References)
            .map(|rel| {
                (
                    rel.src_name.as_str(),
                    rel.receiver.as_deref(),
                    rel.dst_name.as_str(),
                )
            })
            .collect();
        assert!(
            declared.contains(&("Response.connection", Some("connection"), "HTTPAdapter")),
            "the class body's own annotation must name its attribute, got {declared:?}"
        );
        assert!(
            output.relations.iter().any(|rel| {
                rel.kind == kin_model::RelationKind::References
                    && rel.dst_name == "HTTPAdapter"
                    && rel.import_source.as_deref() == Some("adapters")
            }),
            "and it must keep the module pin: carrying an attribute on the edge \
             must not cost it the import_source every other reference gets"
        );
    }

    #[test]
    fn two_attributes_of_one_type_each_name_themselves() {
        // Matching the declaration to its edge by name alone would stamp both
        // rows with whichever attribute was seen last, and the linker would
        // then answer one attribute's join with the other's type.
        let adapter = PythonAdapter;
        let source = b"from adapters import HTTPAdapter\n\n\nclass Pair:\n    primary: HTTPAdapter\n    backup: HTTPAdapter\n";
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(&tree, source, &FilePathId::new("pair.py"))
            .unwrap();
        let mut attributes: Vec<&str> = output
            .relations
            .iter()
            .filter(|rel| {
                rel.kind == kin_model::RelationKind::References && rel.dst_name == "HTTPAdapter"
            })
            .filter_map(|rel| rel.receiver.as_deref())
            .collect();
        attributes.sort();
        assert_eq!(
            attributes,
            vec!["backup", "primary"],
            "each annotation must carry its own attribute"
        );
    }

    #[test]
    fn a_parameter_annotation_references_the_imported_class() {
        // The reported shape. `ParsedNote` is named only as a parameter type,
        // so before annotations carried an edge this file reported nothing and
        // a rename left `upsert_note` holding the old name.
        let edges = references_in(
            "from parsing import ParsedNote\n\n\nclass NoteStore:\n    def upsert_note(self, note: ParsedNote):\n        return 1\n",
        );
        assert!(
            edges.contains(&(
                "NoteStore.upsert_note".to_string(),
                "ParsedNote".to_string()
            )),
            "a parameter annotation must reference its class, got {edges:?}"
        );
    }

    #[test]
    fn a_return_annotation_references_the_imported_class() {
        let edges = references_in(
            "from parsing import ParsedNote\n\n\ndef load(path) -> ParsedNote:\n    return parse(path)\n",
        );
        assert!(
            edges.contains(&("load".to_string(), "ParsedNote".to_string())),
            "a return annotation must reference its class, got {edges:?}"
        );
    }

    #[test]
    fn a_dataclass_field_annotation_references_its_element_type() {
        // `links: Tuple[WikiLink, ...]` is the type `ParsedNote` is built from.
        // The subscripted generic must be descended into: the edge belongs to
        // `WikiLink`, not to `Tuple`.
        let edges = references_in(
            "from typing import Tuple\nfrom parsing import WikiLink\n\n\nclass ParsedNote:\n    links: Tuple[WikiLink, ...] = ()\n",
        );
        assert!(
            edges.contains(&("ParsedNote.links".to_string(), "WikiLink".to_string())),
            "a dataclass field type must reference its element type, got {edges:?}"
        );
    }

    #[test]
    fn every_annotation_wrapper_reaches_the_name_it_carries() {
        // One fixture per wrapper tree-sitter builds a type expression from, so
        // a grammar shape that stops the descent is a named failure rather than
        // a quiet miss somewhere in a larger fixture.
        let cases: [(&str, &str); 6] = [
            ("Optional[Note]", "Optional[Note]"),
            ("bare", "Note"),
            ("builtin generic", "list[Note]"),
            ("union", "Note | None"),
            ("nested generic", "dict[str, list[Note]]"),
            ("callable", "Callable[[Note], int]"),
        ];
        for (label, annotation) in cases {
            let source = format!(
                "from typing import Callable, Optional\nfrom parsing import Note\n\n\ndef take(value: {annotation}):\n    return value\n"
            );
            let edges = references_in(&source);
            assert!(
                edges.contains(&("take".to_string(), "Note".to_string())),
                "`{label}` annotation `{annotation}` must reach Note, got {edges:?}"
            );
        }
    }

    #[test]
    fn a_dotted_annotation_keeps_the_module_qualified_form() {
        // `import parsing` binds the module, so `parsing.Note` names a member of
        // it. Reducing that to the bare leaf would guess which module a
        // same-named class came from, which is the linker's namespace tier's job
        // and only possible with the dotted form.
        let edges =
            references_in("import parsing\n\n\ndef take(value: parsing.Note):\n    return value\n");
        assert!(
            edges.contains(&("take".to_string(), "parsing.Note".to_string())),
            "a dotted annotation must keep its module-qualified form, got {edges:?}"
        );
    }

    #[test]
    fn a_quoted_forward_reference_references_the_named_class() {
        let edges = references_in(
            "from parsing import Note\n\n\nclass Holder:\n    note: \"Note\" = None\n",
        );
        assert!(
            edges.contains(&("Holder.note".to_string(), "Note".to_string())),
            "a quoted forward reference must reference its class, got {edges:?}"
        );
    }

    #[test]
    fn a_variadic_parameter_annotation_references_its_class() {
        let edges = references_in(
            "from parsing import Note\n\n\ndef take(*args: Note, **kw: Note):\n    return args\n",
        );
        assert!(
            edges.contains(&("take".to_string(), "Note".to_string())),
            "`*args: Note` must reference its class, got {edges:?}"
        );
    }

    #[test]
    fn an_annotation_naming_a_builtin_or_an_unimported_name_emits_nothing() {
        // The precision half, and the reason the emit filter runs on annotations
        // rather than letting the linker sort it out: `str` and `int` are
        // builtins, and `Ghost` is defined in no file this one can reach, so a
        // blind exact-name tier would bind them to whatever entity elsewhere
        // happens to carry that name.
        let edges = references_in(
            "def take(name: str, count: int, other: Ghost) -> bool:\n    return True\n",
        );
        assert!(
            edges.is_empty(),
            "a builtin or unimported annotation must emit no reference, got {edges:?}"
        );
    }

    #[test]
    fn an_annotation_naming_a_local_binding_emits_nothing() {
        // A name the function binds itself is not a module-level symbol, so it
        // must not reach one that happens to share its name.
        let edges = references_in(
            "class Note:\n    pass\n\n\ndef take(Note):\n    value: Note = 1\n    return value\n",
        );
        assert!(
            !edges.contains(&("take".to_string(), "Note".to_string())),
            "a locally bound annotation name must emit no reference, got {edges:?}"
        );
    }

    #[test]
    fn a_parameter_name_is_still_not_a_reference() {
        // Annotations became references; parameter NAMES did not. A parameter
        // called `note` is a binding, and binding a name is not reading one.
        // Two independent rules hold this, and it takes breaking both to make
        // this assertion fail: only the `type` field of a parameter is walked,
        // and a parameter name is in the shadow set anyway.
        let edges =
            references_in("def note():\n    return 1\n\n\ndef take(note: int):\n    return note\n");
        assert!(
            !edges.contains(&("take".to_string(), "note".to_string())),
            "a parameter name must not reference a same-named function, got {edges:?}"
        );
    }
}
