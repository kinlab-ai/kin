// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The incremental receiver-origin world answers member and declaration
//! lookups from indexes of the step's fresh entities. Each indexed lookup must
//! answer exactly what a scan of those entities answered, in the same order,
//! and a step's cost must no longer grow with (fresh entities x lookups).
//!
//! The timing test is ignored by default. Run it with an optimized profile:
//!
//! ```text
//! cargo test -p kin-index --profile suite --lib -- --ignored --nocapture \
//!     incremental_origin_step_scaling
//! ```

use super::*;
use crate::receiver_origin::{Member, OriginWorld};
use kin_model::{EntityMetadata, FingerprintAlgorithm, SemanticFingerprint};

/// The lookups as the world answered them before it indexed the step, by
/// scanning every fresh entity, kept as the oracle the indexed lookups must
/// match.
mod scan {
    use super::*;

    /// (file, name) of every entity the linker indexes, as owned strings.
    pub(super) fn placed(
        world: &IncrementalOriginWorld<'_>,
    ) -> HashMap<EntityId, (String, String)> {
        let mut placed = HashMap::new();
        for (file, names) in &world.linker.entity_by_file_name {
            for (name, id) in names {
                placed.insert(*id, (file.clone(), name.clone()));
            }
        }
        placed
    }

    pub(super) fn entity_file(
        world: &IncrementalOriginWorld<'_>,
        placed: &HashMap<EntityId, (String, String)>,
        entity: EntityId,
    ) -> Option<String> {
        if let Some(fresh) = world.fresh.get(&entity) {
            return fresh.file_origin.as_ref().map(|file| file.0.clone());
        }
        placed.get(&entity).map(|(file, _)| file.clone())
    }

    pub(super) fn entity_name(
        world: &IncrementalOriginWorld<'_>,
        placed: &HashMap<EntityId, (String, String)>,
        entity: EntityId,
    ) -> Option<String> {
        if let Some(fresh) = world.fresh.get(&entity) {
            return Some(fresh.name.clone());
        }
        placed.get(&entity).map(|(_, name)| name.clone())
    }

    pub(super) fn file_declares(
        world: &IncrementalOriginWorld<'_>,
        file: &str,
        name: &str,
    ) -> bool {
        world
            .fresh_files
            .get(file)
            .map(|entities| entities.iter().any(|entity| entity.name == name))
            .unwrap_or_else(|| {
                world
                    .linker
                    .entity_by_file_name
                    .get(file)
                    .is_some_and(|names| names.contains_key(name))
            })
    }

    pub(super) fn members_named(
        world: &IncrementalOriginWorld<'_>,
        placed: &HashMap<EntityId, (String, String)>,
        member: &str,
    ) -> Vec<Member> {
        let mut seen = HashSet::new();
        let mut members = Vec::new();
        for (file, entities) in &world.fresh_files {
            for entity in entities {
                if bare_entity_name(&entity.name) == member && seen.insert(entity.id) {
                    members.push(Member {
                        id: entity.id,
                        file: (*file).to_string(),
                        owner: member_owner(&entity.name),
                    });
                }
            }
        }
        for (file, id) in world
            .linker
            .entity_by_bare_name
            .get(member)
            .into_iter()
            .flatten()
            .chain(
                world
                    .linker
                    .entity_by_name
                    .get(member)
                    .into_iter()
                    .flatten(),
            )
        {
            if world.fresh_files.contains_key(file.as_str()) || !seen.insert(*id) {
                continue;
            }
            members.push(Member {
                id: *id,
                file: file.clone(),
                owner: placed.get(id).and_then(|(_, name)| member_owner(name)),
            });
        }
        members
    }

    pub(super) fn class_bases(
        world: &IncrementalOriginWorld<'_>,
        file: &str,
        class: &str,
    ) -> Option<Vec<String>> {
        if let Some((_, bases)) = world
            .class_bases
            .get(file)
            .and_then(|classes| classes.iter().find(|(name, _)| name == class))
        {
            return Some(bases.clone());
        }
        let declared = match world.fresh_files.get(file) {
            Some(entities) => entities
                .iter()
                .any(|entity| entity.name == class && is_class_like(Some(&entity.kind))),
            None => world
                .linker
                .entity_by_file_name
                .get(file)
                .and_then(|names| names.get(class))
                .is_some_and(|id| is_class_like(world.linker.entity_kind_by_id.get(id))),
        };
        declared.then(Vec::new)
    }

    pub(super) fn declares_class_named(world: &IncrementalOriginWorld<'_>, leaf: &str) -> bool {
        world.fresh.values().any(|entity| {
            bare_entity_name(&entity.name) == leaf && is_class_like(Some(&entity.kind))
        }) || world
            .linker
            .entity_by_name
            .get(leaf)
            .into_iter()
            .flatten()
            .chain(
                world
                    .linker
                    .entity_by_bare_name
                    .get(leaf)
                    .into_iter()
                    .flatten(),
            )
            .any(|(_, id)| is_class_like(world.linker.entity_kind_by_id.get(id)))
    }

    pub(super) fn declaration(
        world: &IncrementalOriginWorld<'_>,
        file: &str,
        name: &str,
    ) -> Option<EntityId> {
        match world.fresh_files.get(file) {
            Some(entities) => entities
                .iter()
                .find(|entity| entity.name == name)
                .map(|entity| entity.id),
            None => world
                .linker
                .entity_by_file_name
                .get(file)
                .and_then(|names| names.get(name))
                .copied(),
        }
    }
}

/// What the comparison exercised, summed over every world it checked.
#[derive(Default)]
struct Exercised {
    member_lookups: usize,
    members_returned: usize,
    lookups_with_fresh_and_indexed_members: usize,
    declaration_lookups: usize,
    fresh_declarations_found: usize,
}

/// Every probe of `names`, `files` and `ids` must get the same answer from
/// the world as from the scan. The probes run twice, in opposite orders, so
/// each index is first built by a different kind of lookup.
fn assert_world_matches_scan(
    world: &IncrementalOriginWorld<'_>,
    names: &[String],
    files: &[String],
    ids: &[EntityId],
    exercised: &mut Exercised,
) {
    let placed = scan::placed(world);
    let fresh_ids: HashSet<EntityId> = world.fresh.keys().copied().collect();
    let check_names = |reverse: bool, exercised: &mut Exercised| {
        let ordered: Vec<&String> = if reverse {
            names.iter().rev().collect()
        } else {
            names.iter().collect()
        };
        for name in ordered {
            let indexed = world.members_named(name);
            assert_eq!(
                indexed,
                scan::members_named(world, &placed, name),
                "members_named({name:?})"
            );
            exercised.member_lookups += 1;
            exercised.members_returned += indexed.len();
            let from_fresh = indexed.iter().filter(|m| fresh_ids.contains(&m.id)).count();
            if from_fresh > 0 && from_fresh < indexed.len() {
                exercised.lookups_with_fresh_and_indexed_members += 1;
            }
            assert_eq!(
                world.declares_class_named(name),
                scan::declares_class_named(world, name),
                "declares_class_named({name:?})"
            );
        }
    };
    let check_files = |reverse: bool, exercised: &mut Exercised| {
        let ordered: Vec<&String> = if reverse {
            files.iter().rev().collect()
        } else {
            files.iter().collect()
        };
        for file in ordered {
            for name in names {
                assert_eq!(
                    world.file_declares(file, name),
                    scan::file_declares(world, file, name),
                    "file_declares({file:?}, {name:?})"
                );
                let declared = world.declaration(file, name);
                assert_eq!(
                    declared,
                    scan::declaration(world, file, name),
                    "declaration({file:?}, {name:?})"
                );
                assert_eq!(
                    world.class_bases(file, name),
                    scan::class_bases(world, file, name),
                    "class_bases({file:?}, {name:?})"
                );
                exercised.declaration_lookups += 1;
                if world.fresh_files.contains_key(file.as_str()) && declared.is_some() {
                    exercised.fresh_declarations_found += 1;
                }
            }
        }
    };
    let check_ids = || {
        for &id in ids {
            assert_eq!(
                world.entity_file(id),
                scan::entity_file(world, &placed, id),
                "entity_file({id:?})"
            );
            assert_eq!(
                world.entity_name(id),
                scan::entity_name(world, &placed, id),
                "entity_name({id:?})"
            );
        }
    };
    check_files(false, exercised);
    check_names(false, exercised);
    check_ids();
    check_ids();
    check_names(true, exercised);
    check_files(true, exercised);
}

fn fingerprint() -> SemanticFingerprint {
    let zero = kin_model::Hash256::from_bytes([0u8; 32]);
    SemanticFingerprint {
        algorithm: FingerprintAlgorithm::V1TreeSitter,
        ast_hash: zero,
        signature_hash: zero,
        behavior_hash: zero,
        equivalence_hash: zero,
        stability_score: 1.0,
    }
}

fn entity(id: EntityId, kind: EntityKind, name: &str, file: &str, line: u32) -> Entity {
    let file_id = FilePathId::new(file);
    Entity {
        id,
        kind,
        name: name.to_string(),
        language: LanguageId::Python,
        fingerprint: fingerprint(),
        file_origin: Some(file_id.clone()),
        span: Some(SourceSpan {
            file: file_id,
            start_byte: line as usize * 10,
            end_byte: line as usize * 10 + 5,
            start_line: line,
            start_col: 0,
            end_line: line,
            end_col: 5,
        }),
        signature: name.to_string(),
        visibility: Visibility::Public,
        role: EntityRole::Source,
        doc_summary: None,
        metadata: EntityMetadata::default(),
        lineage_parent: None,
        created_in: None,
        superseded_by: None,
    }
}

fn id(n: u128) -> EntityId {
    EntityId(uuid::Uuid::from_u128(n))
}

fn unparsed(file: &str, entities: Vec<Entity>) -> FileParseData {
    FileParseData {
        file_path: file.to_string(),
        entities,
        relations: Vec::new(),
        imports: Vec::new(),
    }
}

fn no_imports() -> HashMap<&'static str, HashMap<&'static str, (&'static str, &'static str)>> {
    HashMap::new()
}

/// A step that renames, mints, moves and repeats entities: each lookup still
/// answers what the scan answered, fresh members first and then the linker's,
/// each id once.
#[test]
fn renamed_minted_moved_and_repeated_entities_answer_as_the_scan_did() {
    let (widget, pop_renamed, panel_pop, store, store_pop, moved_pop, cache_pop, helper) =
        (id(1), id(2), id(3), id(4), id(5), id(6), id(7), id(8));
    let mut linker = IncrementalLinker::new();
    linker.add_file(
        "src/a.py",
        ArtifactId::new(),
        &[
            entity(widget, EntityKind::Class, "Widget", "src/a.py", 1),
            entity(
                pop_renamed,
                EntityKind::Method,
                "Widget.push",
                "src/a.py",
                2,
            ),
        ],
    );
    linker.add_file(
        "src/b.py",
        ArtifactId::new(),
        &[
            entity(store, EntityKind::Class, "Store", "src/b.py", 1),
            entity(store_pop, EntityKind::Method, "Store.pop", "src/b.py", 2),
        ],
    );
    linker.add_file(
        "src/c.py",
        ArtifactId::new(),
        &[
            entity(moved_pop, EntityKind::Function, "pop", "src/c.py", 1),
            entity(helper, EntityKind::Function, "helper", "src/c.py", 2),
        ],
    );
    linker.add_file(
        "src/e.py",
        ArtifactId::new(),
        &[entity(
            cache_pop,
            EntityKind::Method,
            "Cache.pop",
            "src/e.py",
            1,
        )],
    );
    let files = vec![
        // `Widget.push` is renamed `Widget.pop`; `Panel.pop` is minted.
        unparsed(
            "src/a.py",
            vec![
                entity(widget, EntityKind::Class, "Widget", "src/a.py", 1),
                entity(pop_renamed, EntityKind::Method, "Widget.pop", "src/a.py", 2),
                entity(panel_pop, EntityKind::Method, "Panel.pop", "src/a.py", 3),
            ],
        ),
        // The same id twice in one file, under two names.
        unparsed(
            "src/b.py",
            vec![
                entity(store, EntityKind::Class, "Store", "src/b.py", 1),
                entity(store_pop, EntityKind::Method, "Store.pop", "src/b.py", 2),
                entity(store_pop, EntityKind::Method, "Other::pop", "src/b.py", 3),
            ],
        ),
        // An entity the linker still places in `src/c.py`, now parsed here.
        unparsed(
            "src/d.py",
            vec![entity(
                moved_pop,
                EntityKind::Method,
                "Queue.pop",
                "src/d.py",
                1,
            )],
        ),
        // A step file that declares nothing.
        unparsed("src/empty.py", Vec::new()),
    ];
    let imports = no_imports();
    let class_bases = HashMap::new();
    let world = IncrementalOriginWorld::new(&files, &linker, &imports, &class_bases, None);

    let members = world.members_named("pop");
    let placed = scan::placed(&world);
    assert_eq!(members, scan::members_named(&world, &placed, "pop"));
    let ids: Vec<EntityId> = members.iter().map(|member| member.id).collect();
    // Fresh entities come first, each id once, then the linker's own.
    assert_eq!(ids.len(), 5, "{members:#?}");
    assert_eq!(
        ids[..4].iter().copied().collect::<HashSet<_>>(),
        HashSet::from([pop_renamed, panel_pop, store_pop, moved_pop]),
        "{members:#?}"
    );
    let at = |id: EntityId| ids.iter().position(|held| *held == id).unwrap();
    assert!(at(pop_renamed) < at(panel_pop), "parse order within a file");
    assert_eq!(ids[4], cache_pop);
    let owner_of = |id: EntityId| members[at(id)].owner.clone();
    assert_eq!(owner_of(pop_renamed).as_deref(), Some("Widget"));
    assert_eq!(owner_of(panel_pop).as_deref(), Some("Panel"));
    assert_eq!(owner_of(store_pop).as_deref(), Some("Store"));
    assert_eq!(owner_of(moved_pop).as_deref(), Some("Queue"));
    assert_eq!(owner_of(cache_pop).as_deref(), Some("Cache"));
    assert_eq!(members[at(moved_pop)].file, "src/d.py");

    // A fresh file answers from its parse, even where the linker disagrees.
    assert!(!world.file_declares("src/a.py", "Widget.push"));
    assert!(world.file_declares("src/a.py", "Widget.pop"));
    assert!(!world.file_declares("src/empty.py", "anything"));
    assert!(world.file_declares("src/c.py", "pop"));
    assert_eq!(world.declaration("src/b.py", "Other::pop"), Some(store_pop));
    assert_eq!(world.declaration("src/c.py", "helper"), Some(helper));
    assert_eq!(world.declaration("src/empty.py", "helper"), None);
    assert_eq!(world.class_bases("src/a.py", "Widget"), Some(Vec::new()));
    assert_eq!(world.class_bases("src/a.py", "Widget.pop"), None);
    assert!(world.declares_class_named("Widget"));
    assert!(!world.declares_class_named("Panel"));

    let names: Vec<String> = [
        "pop",
        "push",
        "Widget",
        "Widget.pop",
        "Widget.push",
        "Panel.pop",
        "Store",
        "Store.pop",
        "Other::pop",
        "Queue.pop",
        "Cache.pop",
        "helper",
        "missing",
        "",
    ]
    .map(String::from)
    .to_vec();
    let file_probes: Vec<String> = [
        "src/a.py",
        "src/b.py",
        "src/c.py",
        "src/d.py",
        "src/e.py",
        "src/empty.py",
        "src/none.py",
    ]
    .map(String::from)
    .to_vec();
    let id_probes = [
        widget,
        pop_renamed,
        panel_pop,
        store,
        store_pop,
        moved_pop,
        cache_pop,
        helper,
        id(99),
    ];
    assert_world_matches_scan(
        &world,
        &names,
        &file_probes,
        &id_probes,
        &mut Exercised::default(),
    );
}

/// A small deterministic generator, so a failing seed reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn percent(&mut self, chance: u64) -> bool {
        self.next() % 100 < chance
    }

    fn pick<'s, T>(&mut self, items: &'s [T]) -> &'s T {
        &items[self.below(items.len())]
    }
}

const FILES: [&str; 6] = [
    "src/a.py", "src/b.py", "src/c.ts", "src/d.rs", "src/e.py", "src/f.go",
];
const LEAVES: [&str; 6] = ["pop", "get", "run", "Widget", "Store", "new"];
const OWNERS: [&str; 4] = ["Widget", "Store", "app.Panel", "ns::Queue"];
const KINDS: [EntityKind; 8] = [
    EntityKind::Function,
    EntityKind::Method,
    EntityKind::Class,
    EntityKind::Interface,
    EntityKind::TraitDef,
    EntityKind::EnumDef,
    EntityKind::Module,
    EntityKind::Constant,
];

fn random_name(rng: &mut Rng) -> String {
    let leaf = *rng.pick(&LEAVES);
    let owner = *rng.pick(&OWNERS);
    match rng.below(6) {
        0 | 1 => leaf.to_string(),
        2 => format!("{owner}.{leaf}"),
        3 => format!("{owner}::{leaf}"),
        4 => format!("pkg.{owner}.{leaf}"),
        _ => format!("{owner}.inner::{leaf}"),
    }
}

/// What one random case produced, and which awkward shapes it holds.
struct RandomCase {
    files: Vec<FileParseData>,
    linker: IncrementalLinker,
    class_bases: HashMap<String, Vec<(String, Vec<String>)>>,
    ids: Vec<EntityId>,
    names: Vec<String>,
    repeated_file: bool,
    empty_file: bool,
    repeated_id: bool,
    renamed_or_moved: bool,
}

fn random_case(seed: u64) -> RandomCase {
    let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
    let mut next_id = 1u128;
    let mut mint = || {
        next_id += 1;
        id(next_id)
    };
    let mut names: Vec<String> = Vec::new();

    // The linker's state from earlier steps.
    let mut linker = IncrementalLinker::new();
    let mut linker_entities: Vec<(EntityId, String)> = Vec::new();
    for file in FILES {
        if !rng.percent(70) {
            continue;
        }
        let entities: Vec<Entity> = (0..rng.below(8))
            .map(|line| {
                let id = if !linker_entities.is_empty() && rng.percent(10) {
                    rng.pick(&linker_entities).0
                } else {
                    mint()
                };
                let name = random_name(&mut rng);
                names.push(name.clone());
                entity(id, *rng.pick(&KINDS), &name, file, line as u32 + 1)
            })
            .collect();
        linker_entities.extend(entities.iter().map(|e| (e.id, e.name.clone())));
        linker.add_file(file, ArtifactId::new(), &entities);
    }

    // The step's own parse.
    let mut files: Vec<FileParseData> = Vec::new();
    let mut step_ids: Vec<EntityId> = Vec::new();
    let (mut repeated_file, mut empty_file, mut repeated_id, mut renamed_or_moved) =
        (false, false, false, false);
    for _ in 0..rng.below(5) + 1 {
        let file = *rng.pick(&FILES);
        repeated_file |= files.iter().any(|held| held.file_path == file);
        let count = if rng.percent(10) {
            0
        } else {
            rng.below(10) + 1
        };
        empty_file |= count == 0;
        let mut entities = Vec::new();
        for line in 0..count {
            let roll = rng.below(100);
            let id = if roll < 20 && !linker_entities.is_empty() {
                renamed_or_moved = true;
                rng.pick(&linker_entities).0
            } else if roll < 35 && !step_ids.is_empty() {
                repeated_id = true;
                *rng.pick(&step_ids)
            } else {
                mint()
            };
            step_ids.push(id);
            let name = random_name(&mut rng);
            names.push(name.clone());
            entities.push(entity(id, *rng.pick(&KINDS), &name, file, line as u32 + 1));
        }
        files.push(unparsed(file, entities));
    }

    // Some classes name a base, which answers `class_bases` before any
    // declaration lookup does.
    let mut class_bases: HashMap<String, Vec<(String, Vec<String>)>> = HashMap::new();
    for _ in 0..rng.below(3) {
        let class = random_name(&mut rng);
        class_bases
            .entry(rng.pick(&FILES).to_string())
            .or_default()
            .push((class, vec!["Base".to_string()]));
    }

    let mut probes: Vec<String> = names.clone();
    probes.extend(LEAVES.iter().map(|leaf| leaf.to_string()));
    probes.extend(OWNERS.iter().map(|owner| owner.to_string()));
    probes.extend([
        "inner::pop".to_string(),
        "missing".to_string(),
        String::new(),
    ]);
    probes.sort();
    probes.dedup();
    let mut ids: Vec<EntityId> = linker_entities.iter().map(|(id, _)| *id).collect();
    ids.extend(step_ids);
    ids.push(mint());
    ids.sort();
    ids.dedup();
    RandomCase {
        files,
        linker,
        class_bases,
        ids,
        names: probes,
        repeated_file,
        empty_file,
        repeated_id,
        renamed_or_moved,
    }
}

/// Random steps, with files repeated or empty, ids repeated across and within
/// files, and entities the linker still holds under another name or file.
#[test]
fn indexed_lookups_match_the_scan_on_random_steps() {
    let mut exercised = Exercised::default();
    let (mut repeated_file, mut empty_file, mut repeated_id, mut renamed_or_moved) = (0, 0, 0, 0);
    let mut file_probes: Vec<String> = FILES.iter().map(|file| file.to_string()).collect();
    file_probes.push("src/none.py".to_string());
    for seed in 0..600 {
        let case = random_case(seed);
        let imports = no_imports();
        let world = IncrementalOriginWorld::new(
            &case.files,
            &case.linker,
            &imports,
            &case.class_bases,
            None,
        );
        assert_world_matches_scan(&world, &case.names, &file_probes, &case.ids, &mut exercised);
        repeated_file += usize::from(case.repeated_file);
        empty_file += usize::from(case.empty_file);
        repeated_id += usize::from(case.repeated_id);
        renamed_or_moved += usize::from(case.renamed_or_moved);
    }
    // The cases reach every shape the indexes must keep.
    assert!(repeated_file >= 50, "repeated files: {repeated_file}");
    assert!(empty_file >= 30, "empty files: {empty_file}");
    assert!(repeated_id >= 100, "repeated ids: {repeated_id}");
    assert!(
        renamed_or_moved >= 100,
        "renamed or moved: {renamed_or_moved}"
    );
    assert!(
        exercised.lookups_with_fresh_and_indexed_members >= 100,
        "lookups answered from both the parse and the linker: {}",
        exercised.lookups_with_fresh_and_indexed_members
    );
    eprintln!(
        "{} member lookups returned {} members; {} declaration lookups found {} fresh \
         declarations; repeated files {repeated_file}, empty files {empty_file}, repeated ids \
         {repeated_id}, renamed or moved {renamed_or_moved}",
        exercised.member_lookups,
        exercised.members_returned,
        exercised.declaration_lookups,
        exercised.fresh_declarations_found,
    );
    assert!(exercised.members_returned >= 10_000);
    assert!(exercised.fresh_declarations_found >= 1_000);
}

/// One parsed Python module of the synthetic step. Each `values.pop(...)` or
/// `values.get(...)` call on a `dict` parameter asks the world for every
/// declaration of that name before the call is settled outside the
/// repository. One module in fifty declares a `pop` of its own, so the
/// answers stay small while the step grows, as they do in a real repository.
fn step_module(index: usize, fillers: usize) -> FileParseData {
    let mut source = String::from("import typing as t\n\n");
    source.push_str(&format!(
        "class Widget{index}:\n    def take_{index}(self, name):\n        return None\n\n    \
         def run(self, values: dict[str, t.Any]) -> None:\n        \
         values.pop('a', None)\n        values.get('b')\n        self.take_{index}('c')\n\n"
    ));
    if index % 50 == 0 {
        source.push_str("    def pop(self):\n        return None\n\n");
    }
    source.push_str(&format!(
        "def use_{index}(values: dict[str, t.Any], items: list[int]) -> None:\n    \
         values.pop('k', None)\n    values.setdefault('k', 1)\n    items.append(1)\n    \
         items.pop()\n\n"
    ));
    for filler in 0..fillers {
        source.push_str(&format!(
            "def filler_{index}_{filler}(x):\n    return x\n\n"
        ));
    }
    parse_module(&format!("pkg/step/mod_{index}.py"), &source)
}

/// A module of the repository the step leaves alone. One in ten declares a
/// `get`.
fn background_module(index: usize, fillers: usize) -> FileParseData {
    let mut source = String::new();
    source.push_str(&format!(
        "class Store{index}:\n    def keep(self, key):\n        return key\n\n"
    ));
    if index % 10 == 0 {
        source.push_str("    def get(self, key):\n        return key\n\n");
    }
    for filler in 0..fillers {
        source.push_str(&format!("def keep_{index}_{filler}(x):\n    return x\n\n"));
    }
    parse_module(&format!("pkg/rest/mod_{index}.py"), &source)
}

fn parse_module(path: &str, source: &str) -> FileParseData {
    let indexed = crate::IndexPipeline::new()
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

struct Step {
    files: Vec<FileParseData>,
    background: Vec<FileParseData>,
    linker: IncrementalLinker,
    artifact_ids: HashMap<String, ArtifactId>,
    completeness: FileParseCompletenessMap,
}

fn synthetic_step(step_files: usize, background_files: usize) -> Step {
    let files: Vec<FileParseData> = (0..step_files).map(|i| step_module(i, 20)).collect();
    let background: Vec<FileParseData> = (0..background_files)
        .map(|i| background_module(i, 20))
        .collect();
    let mut linker = IncrementalLinker::new();
    let mut artifact_ids = HashMap::new();
    for (index, file) in background.iter().chain(&files).enumerate() {
        // Fixed identities, so two builds' relations can be compared.
        let artifact = ArtifactId(uuid::Uuid::from_u128(index as u128 + 1));
        artifact_ids.insert(file.file_path.clone(), artifact);
        linker.add_file(&file.file_path, artifact, &file.entities);
    }
    let completeness = files
        .iter()
        .map(|file| (file.file_path.clone(), ParseCompleteness::Full))
        .collect();
    Step {
        files,
        background,
        linker,
        artifact_ids,
        completeness,
    }
}

/// The world of a parsed step answers every lookup the scan answered, for
/// every name and file the parse holds.
#[test]
fn indexed_lookups_match_the_scan_on_a_parsed_step() {
    let step = synthetic_step(60, 20);
    let overlays = build_incremental_link_overlays(&step.files, &step.linker);
    let world = IncrementalOriginWorld::new(
        &step.files,
        &step.linker,
        &overlays.import_map,
        &overlays.class_bases,
        None,
    );
    let mut names: Vec<String> = step
        .files
        .iter()
        .chain(&step.background)
        .flat_map(|file| file.entities.iter())
        .flat_map(|entity| {
            [
                entity.name.clone(),
                bare_entity_name(&entity.name).to_string(),
            ]
        })
        .collect();
    names.extend(["missing".to_string(), String::new()]);
    names.sort();
    names.dedup();
    let mut files: Vec<String> = step
        .files
        .iter()
        .chain(step.background.iter().take(3))
        .map(|file| file.file_path.clone())
        .collect();
    files.push("pkg/none.py".to_string());
    let ids: Vec<EntityId> = step
        .files
        .iter()
        .chain(&step.background)
        .flat_map(|file| file.entities.iter().map(|entity| entity.id))
        .collect();
    let mut exercised = Exercised::default();
    assert_world_matches_scan(&world, &names, &files, &ids, &mut exercised);
    // Every `get` and `pop` of the step and of the rest of the repository.
    let declared = |leaf: &str| {
        step.files
            .iter()
            .chain(&step.background)
            .flat_map(|file| file.entities.iter())
            .filter(|entity| entity.name.ends_with(&format!(".{leaf}")))
            .count()
    };
    assert_eq!(world.members_named("pop").len(), declared("pop"));
    assert_eq!(world.members_named("get").len(), declared("get"));
    assert!(declared("pop") > 0 && declared("get") > 0);
}

/// The member names the step's receiver calls ask the world about, one per
/// call.
fn receiver_call_members(files: &[FileParseData]) -> Vec<&str> {
    files
        .iter()
        .flat_map(|file| file.relations.iter())
        .filter(|rel| rel.kind == RelationKind::Calls && rel.receiver.is_some())
        .map(|rel| bare_entity_name(&rel.dst_name))
        .collect()
}

/// A digest of relations that two builds can compare: relation ids are
/// minted per run, so they are left out, and everything else is kept.
fn relations_digest(relations: &[Relation]) -> String {
    let mut lines: Vec<String> = relations
        .iter()
        .map(|relation| {
            let mut relation = relation.clone();
            relation.id = RelationId(uuid::Uuid::nil());
            serde_json::to_string(&relation).unwrap()
        })
        .collect();
    lines.sort();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())[..16].to_string()
}

#[test]
#[ignore = "timing benchmark; run explicitly with an optimized profile"]
fn incremental_origin_step_scaling() {
    let background = 2_000;
    println!(
        "{:>6} {:>8} {:>7} {:>11} {:>11} {:>11} {:>11}  {:<16} {:<16}",
        "files",
        "entities",
        "calls",
        "members ms",
        "link ms",
        "graph ms",
        "cover ms",
        "link digest",
        "graph digest",
    );
    // `KIN_STEP_SIZES=400,1600` measures only those step sizes.
    let sizes: Vec<usize> = std::env::var("KIN_STEP_SIZES")
        .map(|sizes| {
            sizes
                .split(',')
                .map(|size| size.trim().parse().unwrap())
                .collect()
        })
        .unwrap_or_else(|_| vec![100, 200, 400, 800, 1_600]);
    for step_files in sizes {
        let step = synthetic_step(step_files, background);
        let fresh_entities: usize = step.files.iter().map(|file| file.entities.len()).sum();
        let members = receiver_call_members(&step.files);

        // The lookups alone, as the receiver-origin pass makes them.
        let overlays = build_incremental_link_overlays(&step.files, &step.linker);
        let world = IncrementalOriginWorld::new(
            &step.files,
            &step.linker,
            &overlays.import_map,
            &overlays.class_bases,
            None,
        );
        let started = std::time::Instant::now();
        let mut found = 0usize;
        for member in &members {
            found += world.members_named(member).len();
        }
        let lookups = started.elapsed();
        assert!(found > 0);
        drop(world);
        drop(overlays);

        // One whole step without a graph.
        let started = std::time::Instant::now();
        let relations = link_cross_file_incremental_with_completeness(
            &step.files,
            &step.linker,
            &step.completeness,
        )
        .unwrap();
        let link = started.elapsed();
        assert!(!relations.is_empty());

        // The daemon's path: the step reads other files through the graph,
        // then each file's coverage certificate is built on its own.
        let graph = kin_db::InMemoryGraph::new();
        for file in step.background.iter().chain(&step.files) {
            for entity in &file.entities {
                kin_model::EntityStore::upsert_entity(&graph, entity).unwrap();
            }
        }
        let started = std::time::Instant::now();
        let graph_relations = link_cross_file_incremental_with_graph(
            &step.files,
            &step.linker,
            &step.completeness,
            &graph,
        )
        .unwrap();
        let graph_link = started.elapsed();
        let started = std::time::Instant::now();
        let mut certificates = Vec::with_capacity(step.files.len());
        for file in &step.files {
            certificates.push(build_incremental_parse_coverage_relation_with_graph(
                file,
                step.artifact_ids[&file.file_path],
                &ParseCompleteness::Full,
                &step.linker,
                &graph,
            ));
        }
        let coverage = started.elapsed();
        let mut graph_output = graph_relations;
        graph_output.extend(certificates);

        println!(
            "{:>6} {:>8} {:>7} {:>11.1} {:>11.1} {:>11.1} {:>11.1}  {:<16} {:<16}",
            step_files,
            fresh_entities,
            members.len(),
            lookups.as_secs_f64() * 1e3,
            link.as_secs_f64() * 1e3,
            graph_link.as_secs_f64() * 1e3,
            coverage.as_secs_f64() * 1e3,
            relations_digest(&relations),
            relations_digest(&graph_output),
        );
    }
}
