// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{Entity, FilePathId, LanguageId};

use super::*;
use crate::{LanguageAdapter, PhpAdapter};

fn census(source: &str) -> FileEscapeCensus {
    let tree = PhpAdapter.parse(source.as_bytes()).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::Php).unwrap()
}

/// (1-based line, context, binding, aliases, receiver).
#[allow(clippy::type_complexity)]
fn uses(
    census: &FileEscapeCensus,
    name: &str,
) -> Vec<(
    u32,
    OccurrenceContext,
    OccurrenceBinding,
    Vec<String>,
    Option<String>,
)> {
    census
        .record
        .values
        .get(name)
        .map(|occurrences| {
            occurrences
                .iter()
                .map(|occurrence| {
                    (
                        occurrence.line + 1,
                        occurrence.context,
                        occurrence.binding,
                        occurrence.aliases.clone(),
                        occurrence.receiver.clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const HOOKS: &str = r#"<?php
namespace App\Services;

use function App\Support\normalize as clean;
use App\Models\{Store, Cache as Memo};

#[Route('/checkout')]
class Checkout implements \Countable, \ArrayAccess
{
    public function boot(Store $store, array $rows, string $handler, string $method): void
    {
        $this->onSave = [$this, 'persist'];
        usort($rows, [Store::class, 'compare']);
        array_map('trim_row', $rows);
        register_shutdown_function('App\Support\flush_all');
        call_user_func('Store::make');
        $saver = $store->save(...);
        $fmt = format_row(...);
        $handler($rows);
        $store->$method();
        call_user_func($handler);
        $made = new $handler();
        $reflected = new \ReflectionMethod(Store::class, 'save');
        $store->save();
        include $path;
        eval($code);
        return;
    }

    public function persist(): void {}
    public function count(): int { return 0; }
    public function offsetGet(mixed $offset): mixed { return null; }
    public function __toString(): string { return ''; }
    public function __invoke(): void {}
    public function helper(): void {}
}
"#;

#[test]
fn string_array_and_first_class_callables_are_uses_of_what_they_name() {
    let census = census(HOOKS);
    assert_eq!(census.record.scope, ModuleScope::Closed);
    assert_eq!(
        uses(&census, "persist"),
        vec![(
            12,
            OccurrenceContext::Alias,
            OccurrenceBinding::Member,
            vec!["onSave".into()],
            Some("$this".into())
        )]
    );
    assert_eq!(
        census.record.values["persist"][0].receiver_kind,
        Some(ReceiverKind::Instance)
    );
    assert_eq!(
        uses(&census, "compare"),
        vec![(
            13,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("Store".into())
        )]
    );
    assert_eq!(
        uses(&census, "trim_row"),
        vec![(
            14,
            OccurrenceContext::Argument,
            OccurrenceBinding::Free,
            vec![],
            None
        )]
    );
    assert_eq!(
        uses(&census, "flush_all"),
        vec![(
            15,
            OccurrenceContext::Argument,
            OccurrenceBinding::Free,
            vec![],
            None
        )]
    );
    assert_eq!(
        uses(&census, "make"),
        vec![(
            16,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("Store".into())
        )]
    );
    let saves = uses(&census, "save");
    assert!(
        saves.contains(&(
            17,
            OccurrenceContext::Assignment,
            OccurrenceBinding::Member,
            vec![],
            Some("$store".into())
        )),
        "{saves:?}"
    );
    assert!(
        saves.contains(&(
            23,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("Store".into())
        )),
        "a literal reflection names the member: {saves:?}"
    );
    assert_eq!(
        saves.len(),
        2,
        "an ordinary call is no value use: {saves:?}"
    );
    assert_eq!(
        uses(&census, "format_row"),
        vec![(
            18,
            OccurrenceContext::Assignment,
            OccurrenceBinding::Free,
            vec![],
            None
        )]
    );
    // Imports under another name.
    assert!(uses(&census, "normalize")
        .iter()
        .any(|use_| use_.1 == OccurrenceContext::Import && use_.3 == vec!["clean".to_string()]));
    assert!(uses(&census, "Cache")
        .iter()
        .any(|use_| use_.1 == OccurrenceContext::Import && use_.3 == vec!["Memo".to_string()]));
    assert!(
        uses(&census, "Store").is_empty(),
        "an import under its own name aliases nothing"
    );

    let forms: Vec<(u32, ReflectiveForm, ReflectiveReach)> = census
        .record
        .reflective
        .iter()
        .map(|site| (site.line + 1, site.form, site.reach))
        .collect();
    for expected in [
        (19, ReflectiveForm::ComputedMember, ReflectiveReach::Any),
        (20, ReflectiveForm::ComputedMember, ReflectiveReach::Members),
        (21, ReflectiveForm::ComputedMember, ReflectiveReach::Any),
        (22, ReflectiveForm::ComputedMember, ReflectiveReach::Any),
        (25, ReflectiveForm::DynamicImport, ReflectiveReach::Nothing),
        (26, ReflectiveForm::Evaluate, ReflectiveReach::Any),
    ] {
        assert!(forms.contains(&expected), "{expected:?} in {forms:?}");
    }
    assert_eq!(forms.len(), 6, "{forms:?}");
    assert_eq!(
        census.record.protocol,
        vec![ProtocolSite {
            line: 6,
            form: ProtocolForm::Decorator,
            name: Some("Route".into()),
        }]
    );
}

#[test]
fn a_call_through_a_closure_bound_once_is_no_lookup() {
    let census = census(
        "<?php\nfunction run(array $rows) {\n    $format = fn($row) => trim($row);\n    $format($rows[0]);\n    $pick = fn() => 1;\n    $pick = fn() => 2;\n    $pick();\n}\n",
    );
    let lines: Vec<u32> = census
        .record
        .reflective
        .iter()
        .map(|site| site.line + 1)
        .collect();
    assert_eq!(lines, vec![7], "only the closure bound twice is a lookup");
}

#[test]
fn magic_and_interface_members_are_stamped() {
    let source = HOOKS.as_bytes();
    let tree = PhpAdapter.parse(source).unwrap();
    let file_id = FilePathId::new("app/Services/Checkout.php");
    let output = PhpAdapter.extract(&tree, source, &file_id).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(LanguageId::Php, &file_id))
        .collect();
    let census = crate::value_escape::census_file(&tree, source, LanguageId::Php);
    crate::value_escape::attach(census.as_ref(), &mut entities);
    let stamped: Vec<&str> = entities
        .iter()
        .filter(|entity| kin_model::ProtocolMember::from_entity(entity).is_some())
        .map(|entity| entity.name.as_str())
        .collect();
    for expected in [
        "Checkout.count",
        "Checkout.offsetGet",
        "Checkout.__toString",
        "Checkout.__invoke",
    ] {
        assert!(stamped.contains(&expected), "{expected} in {stamped:?}");
    }
    assert!(!stamped.contains(&"Checkout.helper"), "{stamped:?}");
    assert!(!stamped.contains(&"Checkout.persist"), "{stamped:?}");
}
