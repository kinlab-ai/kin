// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Whether an entity only declares a callable whose running body is another
//! entity's.
//!
//! A C prototype, a TypeScript ambient declaration and a Python `@overload`
//! stub each carry the name of a definition the program actually runs. Name
//! resolution offers both, and a call bound to the declaration lands on code
//! that never runs: Flask's `ScriptInfo.load_app` calls `locate_app`, whose
//! two `@t.overload` stubs sit above the implementation, and every reader
//! that took a stub for the callee answered about a body of `...`.
//!
//! The linker and the language-server placement prefer the definition, and
//! they ask the entity's own adapter which entities only declare, through
//! [`declares_only`]. The rule is the adapter's because the spelling is the
//! language's: a trailing `;` on a C signature, a leading `declare` on a
//! TypeScript one, an `@overload` decorator on a Python one. A language whose
//! adapter states nothing has no declaration-only entities, so nothing of it
//! can be passed over.

use std::sync::OnceLock;

use kin_model::{EntityKind, LanguageId};

use crate::languages::AdapterRegistry;

/// Whether an entity `language`'s adapter extracted, of `kind` and stored
/// `signature`, only declares a callable whose body is elsewhere. False for a
/// language no registered adapter handles.
pub fn declares_only(language: LanguageId, kind: EntityKind, signature: &str) -> bool {
    static REGISTRY: OnceLock<AdapterRegistry> = OnceLock::new();
    REGISTRY
        .get_or_init(AdapterRegistry::new)
        .get_by_language(language)
        .is_some_and(|adapter| adapter.declares_only(kind, signature))
}

/// The decorator names a stored signature opens with, as the Python adapter
/// writes them: each decorator's trailing identifier after an `@`, separated
/// by spaces, ahead of the `def` or `class` they decorate (`@overload def
/// parse(text: str) -> int`).
pub(crate) fn leading_decorators(signature: &str) -> impl Iterator<Item = &str> {
    signature
        .split_whitespace()
        .map_while(|token| token.strip_prefix('@'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LanguageAdapter as _;

    /// What the Python adapter extracts from Flask's shapes reads as the
    /// adapter says: every `@t.overload` stub, at module level and in a
    /// class, only declares, and each implementation declares its body.
    #[test]
    fn python_overload_stubs_extracted_from_source_only_declare() {
        let adapter = crate::languages::PythonAdapter;
        let source = br#"import typing as t
from typing import overload


@t.overload
def locate_app(name: str) -> str: ...


@overload
def locate_app(name: None) -> None: ...


def locate_app(name):
    return name


class App:
    @t.overload
    def template_filter(self, name: t.Callable) -> t.Callable: ...

    @typing.overload
    def template_filter(self, name: str | None = None) -> t.Callable: ...

    @setupmethod
    def template_filter(self, name=None):
        return name
"#;
        let tree = adapter.parse(source).unwrap();
        let output = adapter
            .extract(
                &tree,
                source,
                &kin_model::FilePathId::new("src/flask/cli.py"),
            )
            .unwrap();
        for name in ["locate_app", "App.template_filter"] {
            let mut group: Vec<_> = output
                .entities
                .iter()
                .filter(|entity| entity.name == name)
                .collect();
            group.sort_by_key(|entity| entity.span.start_line);
            let read: Vec<bool> = group
                .iter()
                .map(|entity| declares_only(LanguageId::Python, entity.kind, &entity.signature))
                .collect();
            assert_eq!(
                read,
                [true, true, false],
                "{name}: two stubs, then the implementation: {:?}",
                group
                    .iter()
                    .map(|entity| &entity.signature)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn each_adapter_states_its_own_declaration_spelling() {
        assert!(declares_only(
            LanguageId::C,
            EntityKind::Function,
            "int add(int a, int b);"
        ));
        assert!(!declares_only(
            LanguageId::C,
            EntityKind::Function,
            "int add(int a, int b)"
        ));
        assert!(declares_only(
            LanguageId::Cpp,
            EntityKind::Method,
            "void Widget::draw() const;"
        ));
        assert!(declares_only(
            LanguageId::TypeScript,
            EntityKind::Constant,
            "declare const VERSION: string"
        ));
        assert!(!declares_only(
            LanguageId::TypeScript,
            EntityKind::Method,
            "declare(): void"
        ));
        assert!(declares_only(
            LanguageId::Python,
            EntityKind::Function,
            "@overload def parse(text: str) -> int"
        ));
        assert!(declares_only(
            LanguageId::Python,
            EntityKind::Method,
            "@staticmethod @overload def parse(text: str) -> int"
        ));
        assert!(
            !declares_only(LanguageId::Python, EntityKind::Function, "def parse(text)"),
            "the implementation an overload set keeps declares its body"
        );
        assert!(
            !declares_only(
                LanguageId::Python,
                EntityKind::Function,
                "def overload(text) -> int"
            ),
            "a function named overload is not decorated by it"
        );
        assert!(
            !declares_only(
                LanguageId::Python,
                EntityKind::Class,
                "@overload class Parser"
            ),
            "only a callable is an overload stub"
        );
        assert!(
            !declares_only(LanguageId::Rust, EntityKind::Function, "fn add(a: i32);"),
            "a language whose adapter states nothing has no declaration-only entities"
        );
    }
}
