// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

use kin_model::{Entity, FilePathId, LanguageId};

use super::*;
use crate::{LanguageAdapter, SwiftAdapter};

fn census(source: &str) -> FileEscapeCensus {
    let tree = SwiftAdapter.parse(source.as_bytes()).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::Swift).unwrap()
}

/// (1-based line, context, binding, aliases, receiver, target line).
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
    Option<u32>,
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
                        occurrence.target_line.map(|line| line + 1),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

const CONTROLLER: &str = r#"import UIKit

private func onTimeout() {}

final class FeedController: UIViewController {
    @Published var items: [Item] = []
    private let store: Store

    init(store: Store) {
        self.store = store
        super.init(nibName: nil, bundle: nil)
    }

    @objc func refresh() {}
    func handleTap() {}

    func wire(button: Button, queue: Queue) {
        button.action = handleTap
        run(helper)
        let saver = store.save
        let unbound = Store.save
        button.addTarget(self, action: #selector(refresh), for: .touchUpInside)
        let byName = items.sorted(by: \.date)
        let path = \Store.name
        perform(Selector("refresh"))
        perform(NSSelectorFromString(selectorName))
        let mirror = Mirror(reflecting: store)
        let title = store.value(forKey: "title")
        queue.schedule(onTimeout)
        callbacks.append(self.refresh)
        if handleTap == nil { return }
        print(saver, unbound, byName, path, mirror, title)
    }

    override func viewDidLoad() {
        super.viewDidLoad()
    }

    static func == (lhs: FeedController, rhs: FeedController) -> Bool { true }
    func callAsFunction() {}
    func hash(into hasher: inout Hasher) {}
    deinit {}
}
"#;

#[test]
fn function_and_method_references_selectors_and_key_paths_are_recorded() {
    let census = census(CONTROLLER);
    assert_eq!(census.record.scope, ModuleScope::Closed);
    // A member the type body declares, read unqualified inside the type.
    let taps = uses(&census, "handleTap");
    assert!(
        taps.contains(&(
            18,
            OccurrenceContext::Alias,
            OccurrenceBinding::Class,
            vec!["action".into()],
            None,
            Some(15)
        )),
        "{taps:?}"
    );
    assert_eq!(taps.len(), 1, "the comparison is inert: {taps:?}");
    // Undeclared in the type: a member of it or of a superclass, or a
    // module-level function.
    assert_eq!(
        uses(&census, "helper"),
        vec![(
            19,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("FeedController".into()),
            None
        )]
    );
    let saves = uses(&census, "save");
    assert!(
        saves.contains(&(
            20,
            OccurrenceContext::Assignment,
            OccurrenceBinding::Member,
            vec![],
            Some("store".into()),
            None
        )),
        "{saves:?}"
    );
    assert!(
        saves.contains(&(
            21,
            OccurrenceContext::Assignment,
            OccurrenceBinding::Member,
            vec![],
            Some("Store".into()),
            None
        )),
        "{saves:?}"
    );
    let refreshes = uses(&census, "refresh");
    for line in [22, 25, 30] {
        assert!(
            refreshes
                .iter()
                .any(|use_| use_.0 == line && use_.1 == OccurrenceContext::Argument),
            "line {line} in {refreshes:?}"
        );
    }
    assert!(refreshes
        .iter()
        .any(|use_| use_.0 == 30 && use_.4.as_deref() == Some("self")));
    assert!(uses(&census, "date").iter().any(|use_| use_.0 == 23));
    assert!(uses(&census, "name")
        .iter()
        .any(|use_| use_.0 == 24 && use_.4.as_deref() == Some("Store")));
    assert!(uses(&census, "title")
        .iter()
        .any(|use_| use_.0 == 28 && use_.2 == OccurrenceBinding::Member));
    // Inside the type a name it does not declare may be a member another
    // file's extension or a superclass declares, or this private top-level
    // function: a member read with an unknown receiver reaches both.
    assert_eq!(
        uses(&census, "onTimeout"),
        vec![(
            29,
            OccurrenceContext::Argument,
            OccurrenceBinding::Member,
            vec![],
            Some("FeedController".into()),
            None
        )]
    );
    for local in [
        "button", "queue", "saver", "unbound", "byName", "path", "mirror",
    ] {
        assert!(uses(&census, local).is_empty(), "{local}");
    }
    let forms: Vec<(u32, ReflectiveForm, ReflectiveReach)> = census
        .record
        .reflective
        .iter()
        .map(|site| (site.line + 1, site.form, site.reach))
        .collect();
    assert_eq!(
        forms,
        vec![
            (26, ReflectiveForm::ComputedMember, ReflectiveReach::Members),
            (27, ReflectiveForm::Enumerate, ReflectiveReach::Members),
        ]
    );
    assert_eq!(
        census.record.protocol,
        vec![ProtocolSite {
            line: 5,
            form: ProtocolForm::Decorator,
            name: Some("Published".into()),
        }]
    );
}

#[test]
fn members_the_language_runs_without_a_named_call_are_stamped() {
    let source = CONTROLLER.as_bytes();
    let tree = SwiftAdapter.parse(source).unwrap();
    let file_id = FilePathId::new("Sources/Feed/FeedController.swift");
    let output = SwiftAdapter.extract(&tree, source, &file_id).unwrap();
    let mut entities: Vec<Entity> = output
        .entities
        .into_iter()
        .map(|entity| entity.into_entity(LanguageId::Swift, &file_id))
        .collect();
    let census = crate::value_escape::census_file(&tree, source, LanguageId::Swift);
    crate::value_escape::attach(census.as_ref(), &mut entities);
    let stamped: Vec<(&str, String)> = entities
        .iter()
        .filter_map(
            |entity| match kin_model::ProtocolMember::from_entity(entity)? {
                ProtocolMember::Implicit(words) => Some((entity.name.as_str(), words)),
                ProtocolMember::Listed(_) => None,
            },
        )
        .collect();
    let names: Vec<&str> = stamped.iter().map(|(name, _)| *name).collect();
    for expected in [
        "FeedController.viewDidLoad",
        "FeedController.==",
        "FeedController.callAsFunction",
        "FeedController.hash",
        "FeedController.deinit",
    ] {
        assert!(names.contains(&expected), "{expected} in {stamped:?}");
    }
    assert!(!names.contains(&"FeedController.wire"), "{stamped:?}");
    assert!(!names.contains(&"FeedController.init"), "{stamped:?}");
}

#[test]
fn declared_names_are_not_reads() {
    let source = "protocol Persisting {\n    func save()\n    var name: String { get }\n}\nenum Mode { case fast, safe }\n";
    let census = census(source);
    for name in ["save", "name", "fast", "safe"] {
        assert!(
            uses(&census, name).is_empty(),
            "{name}: {:?}",
            census.record.values
        );
    }
}

#[test]
fn a_private_top_level_function_read_at_top_level_is_the_files_own() {
    let source = "private func onTimeout() {}\nfunc start(queue: Queue) {\n    queue.schedule(onTimeout)\n}\n";
    let census = census(source);
    assert_eq!(
        uses(&census, "onTimeout"),
        vec![(
            3,
            OccurrenceContext::Argument,
            OccurrenceBinding::Module,
            vec![],
            None,
            Some(1)
        )]
    );
}

#[test]
fn a_file_of_top_level_statements_still_carries_its_record() {
    let source = "let handler = onTimeout\nregister(handler)\n";
    let tree = SwiftAdapter.parse(source.as_bytes()).unwrap();
    let file_id = FilePathId::new("Sources/App/main.swift");
    let output = SwiftAdapter
        .extract(&tree, source.as_bytes(), &file_id)
        .unwrap();
    let census =
        crate::value_escape::census_file(&tree, source.as_bytes(), LanguageId::Swift).unwrap();
    assert_eq!(
        uses(&census, "onTimeout"),
        vec![(
            1,
            OccurrenceContext::Alias,
            OccurrenceBinding::Free,
            vec!["handler".into()],
            None,
            None
        )]
    );
    if output.entities.is_empty() {
        assert!(crate::value_escape::module_surface(
            LanguageId::Swift,
            &file_id,
            &tree,
            source.as_bytes()
        )
        .is_some());
    }
}
