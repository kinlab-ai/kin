# Language Support

Kin preserves every admitted file in exact repository authority regardless of
language. A separate graph-native enrichment stage classifies supported content
and extracts as much semantic structure as the language's adapter provides.
This page states exactly what each language gets from that enrichment. No tier
is implied beyond what the extraction actually emits.

## How classification works

Graph-native semantic enrichment routes each admitted file into one of four
tiers:

| Tier | What the graph stores |
| --- | --- |
| **Full semantic** | Entities (functions, classes, methods, types…), relations (calls, containment, inheritance…), test detection, doc/comment summaries |
| **Shallow syntax** | Coarse top-level declarations + imports + a syntax fingerprint, with no call graph, no nested entities, no tests, and no docs |
| **Structured artifact** | Dedicated extractors for manifests and configs (Cargo.toml, package.json, go.mod, pom.xml, Dockerfile, CI configs, SQL migrations…) |
| **Opaque artifact** | Content hash + MIME hint. Never dropped, never parsed |

Whole-repo ingest and incremental edits (reconcile/watch) route the same way.
Both resolve every extension the adapter registry claims, so a file lands in
the same tier whichever path admitted it, and a test asserts that the two
cannot drift apart.

## Full semantic support

| Language | Entities | Relations | Tests | Docs | LSP enrichment |
| --- | --- | --- | --- | --- | --- |
| TypeScript | ✓ | calls, contains, extends, implements, references | ✓ (jest) | ✓ | ✓ typescript-language-server |
| JavaScript | ✓ | calls, contains, extends, references | ✓ | ✓ | ✓ typescript-language-server |
| Python | ✓ | calls, contains, extends, references | ✓ (pytest) | ✓ | ✓ pyright / pylsp |
| Go | ✓ | calls, contains, extends, implements, imports, references, sends-message, spawns | ✓ | ✓ | ✓ gopls |
| Java | ✓ | calls, contains, extends, implements, imports, references | ✓ (junit) | ✓ | not wired (jdtls adapter exists) |
| Rust | ✓ | calls, contains, implements, imports, references | ✓ (cargo) | ✓ | ✓ rust-analyzer |
| C++ | ✓ | calls, contains, extends, imports, references, uses-macro | ✓ | ✓ | not wired (clangd adapter exists) |
| C | ✓ | calls, imports, references, uses-macro | ✗ | ✓ | not wired (clangd adapter exists) |
| Kotlin | ✓ | calls, contains, extends, implements, imports, references | ✓ | ✓ | ✗ |
| C# | ✓ | calls, contains, extends, references | ✗ | ✓ | ✗ |
| Ruby | ✓ | calls, contains, extends, references | ✗ | ✓ | ✗ |
| Swift | ✓ | calls, contains, implements, imports, references | ✓ (xctest) | ✓ | ✗ |
| PHP | ✓ | calls, contains, extends, implements, imports, references | ✓ (phpunit) | ✓ | ✗ |
| HCL / Terraform | ✓ | imports, references | ✗ | ✓ | ✗ |

An `imports` relation in this table is an entity-rooted one, the class that
answers "who imports this". Go, Java, Kotlin, PHP, Rust and Swift reach it at the
`import_scoped` tier, because the destination is a module a coordinate settled
rather than an export a specifier proved: a Go or Swift import names a package
directory whose representative file this build chooses, and a Java, Kotlin or PHP
one names a type through a source root the repository never writes down.
TypeScript, JavaScript and Python bind a named specifier to the export it names
and reach `type_resolved`. C's `#include` directives are `Includes` edges and its
adapter emits no module entity, so C mints no entity-rooted `Imports` edge and
the coverage envelope names it. An import of a module the repository does not
hold mints no edge in any language and is disclosed by the per-file
import-resolution certificate instead.

The module entity an import edge is sourced at is minted for a file that
produced a declaration or an import, and not for one that produced neither. A
comment-only file, an empty file and a file its adapter could not read therefore
hold no entity at all, and the parse-coverage census counts and names them
rather than reading a full row over them. This does not narrow the import class:
a file with an import has one by definition.

C# and Ruby currently use dedicated semantic adapters for entities, relations,
imports, and documentation, but those adapters do not yet implement
test-framework detection. Their extracted parse output therefore leaves the
test list empty. This differs from adapters such as Python and Swift, which
include framework-aware detection for pytest and XCTest respectively.

## Conformance scorecard

The language conformance suite measures every language Kin parses with the
same small project (`tests/language-conformance/<language>/`) and the same
checks, and states where each language stands today. The section below is
rendered from the declarations those tests hold true, and CI refuses a change
that lets it differ from them.

Run one language's cells with
`cargo nextest run -p kin-parser --lib -E 'test(/language_conformance_test::go::/)'`.
After a change that moves a cell, update that language's `expect.toml` and run
`python3 scripts/language-scorecard.py --write`.

<!-- language-scorecard:begin -->
<!-- Generated by scripts/language-scorecard.py from tests/language-conformance. Edit the expect.toml declarations, never this section. -->

Each cell is what the language conformance suite measures for one language and capability, with the same fixture project written in every language:

- **pass**: every check holds.
- **partial**: some checks fail; the list below names them.
- **fail**: Kin attempts it and every check fails.
- **refused**: Kin does not offer it for the language today, and says so.
- **n/a**: the language has no such construct.

A cell's test fails when what it measures differs from the declaration in either direction, so a language cannot claim a capability it lacks, and a change that makes one work flips its cell in the same commit.

### Parser layer

| Capability | Python | TypeScript | JavaScript | Go | Rust |
| --- | --- | --- | --- | --- | --- |
| `parse_clean` | pass | pass | pass | pass | pass |
| `entities` | pass | partial | pass | pass | pass |
| `module_name` | pass | pass | pass | pass | pass |
| `module_contains` | pass | pass | pass | pass | pass |
| `declaration_extent` | pass | partial | partial | pass | pass |
| `census_clean` | pass | pass | pass | pass | pass |
| `nested_definition` | pass | partial | partial | partial | pass |
| `call_same_file` | pass | pass | pass | pass | pass |
| `call_receiver` | pass | pass | pass | pass | pass |
| `imports` | pass | pass | pass | pass | pass |
| `tests_detected` | pass | pass | pass | pass | pass |
| `refuse_unparseable` | partial | pass | pass | pass | pass |
| `placement_three_forms` | pass | pass | pass | pass | pass |
| `create_remove_spacing` | pass | partial | partial | pass | pass |
| `body_edit_keeps_entities` | pass | pass | pass | pass | pass |
| `form_generator` | pass | pass | pass | pass | n/a |
| `form_async` | pass | pass | pass | n/a | pass |
| `form_bound_lambda` | fail | pass | pass | pass | pass |
| `form_default_export` | n/a | pass | pass | n/a | n/a |
| `form_overload` | pass | pass | n/a | n/a | n/a |
| `form_nested_type` | fail | pass | n/a | n/a | n/a |
| `form_decorated` | pass | pass | pass | n/a | pass |
| `form_field` | pass | partial | pass | pass | pass |
| `form_flow_types` | n/a | n/a | partial | n/a | n/a |
| **passing** | 19 of 22 | 18 of 23 | 18 of 22 | 17 of 18 | 19 of 19 |

Other languages Kin parses:

| Capability | Java | Kotlin | C | C++ | C# | Ruby | PHP | Swift | HCL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `parse_clean` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `entities` | pass | partial | pass | pass | partial | pass | pass | partial | pass |
| `module_name` | pass | pass | refused | refused | refused | refused | pass | pass | n/a |
| `module_contains` | pass | pass | refused | refused | refused | refused | pass | pass | n/a |
| `declaration_extent` | fail | fail | pass | fail | pass | pass | fail | fail | pass |
| `census_clean` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `nested_definition` | pass | partial | n/a | pass | fail | fail | pass | pass | n/a |
| `call_same_file` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `call_receiver` | pass | pass | n/a | pass | pass | pass | pass | pass | n/a |
| `imports` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `tests_detected` | pass | pass | pass | pass | pass | pass | pass | pass | refused |
| `refuse_unparseable` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `placement_three_forms` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `create_remove_spacing` | partial | partial | pass | partial | pass | pass | partial | partial | pass |
| `body_edit_keeps_entities` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `form_generator` | n/a | pass | n/a | n/a | pass | pass | pass | n/a | n/a |
| `form_async` | n/a | pass | n/a | n/a | pass | n/a | n/a | pass | n/a |
| `form_bound_lambda` | pass | pass | n/a | fail | pass | pass | n/a | pass | n/a |
| `form_default_export` | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| `form_overload` | pass | pass | n/a | pass | pass | n/a | n/a | pass | n/a |
| `form_nested_type` | pass | pass | n/a | pass | pass | pass | n/a | pass | n/a |
| `form_decorated` | pass | pass | pass | pass | pass | n/a | pass | pass | n/a |
| `form_field` | pass | partial | pass | pass | partial | fail | pass | partial | n/a |
| `form_flow_types` | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a |
| **passing** | 18 of 20 | 17 of 22 | 13 of 15 | 15 of 20 | 17 of 22 | 15 of 19 | 16 of 18 | 17 of 21 | 8 of 9 |

### Agent surface

| Capability | Python | TypeScript | JavaScript | Go | Rust |
| --- | --- | --- | --- | --- | --- |
| `route_source` | pass | partial | partial | pass | pass |
| `route_locate` | pass | pass | pass | pass | pass |
| `route_refs` | pass | pass | pass | pass | pass |
| `route_impact` | pass | pass | pass | pass | pass |
| `route_context` | pass | pass | pass | pass | pass |
| `route_update` | pass | pass | pass | pass | pass |
| `route_patch` | pass | pass | pass | pass | pass |
| `route_body_edit` | pass | pass | pass | pass | pass |
| `route_create_sibling` | pass | pass | pass | pass | pass |
| `route_remove` | pass | pass | pass | pass | pass |
| **passing** | 10 of 10 | 9 of 10 | 9 of 10 | 10 of 10 | 10 of 10 |

Other languages Kin parses:

| Capability | Java | Kotlin | C | C++ | C# | Ruby | PHP | Swift | HCL |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `route_source` | partial | partial | pass | partial | pass | pass | partial | partial | pass |
| `route_locate` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `route_refs` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `route_impact` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `route_context` | pass | pass | n/a | pass | pass | pass | pass | pass | n/a |
| `route_update` | pass | pass | pass | pass | pass | pass | pass | pass | pass |
| `route_patch` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `route_body_edit` | pass | pass | pass | pass | pass | pass | pass | pass | n/a |
| `route_create_sibling` | refused | refused | refused | refused | refused | refused | refused | refused | n/a |
| `route_remove` | refused | refused | refused | refused | refused | refused | refused | refused | n/a |
| **passing** | 7 of 10 | 7 of 10 | 7 of 9 | 7 of 10 | 8 of 10 | 8 of 10 | 7 of 10 | 7 of 10 | 3 of 3 |

### Cells short of pass

- **Python, `refuse_unparseable`** (partial): fails `line`. An unclosed parenthesis is reported on the line after it, where the grammar gives up.
- **Python, `form_bound_lambda`** (fail): fails `Function square`, `square`.
- **Python, `form_nested_type`** (fail): fails `Class Outer.Inner`, `Method Outer.Inner.ping`, `Outer.Inner`, `Outer.Inner.ping`. Admitted as `Inner` and `Inner.ping`, without the enclosing class.
- **TypeScript, `entities`** (partial): fails `Field Account.balance`, `unexpected Method Account.balance`. Fields are emitted as Method; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **TypeScript, `declaration_extent`** (partial): fails `total`. The JSDoc above `total` is not yet part of its span.
- **TypeScript, `nested_definition`** (partial): fails `admitted`.
- **TypeScript, `create_remove_spacing`** (partial): fails `remove_documented`.
- **TypeScript, `form_field`** (partial): fails `Field Point.x`, `Field Point.y`. Fields are emitted as Method; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **TypeScript, `route_source`** (partial): fails `starts`.
- **JavaScript, `declaration_extent`** (partial): fails `total`. The JSDoc above `total` is not yet part of its span.
- **JavaScript, `nested_definition`** (partial): fails `admitted`.
- **JavaScript, `create_remove_spacing`** (partial): fails `remove_documented`.
- **JavaScript, `form_flow_types`** (partial): fails `Badge`, `Function useCount`, `Function|Class Badge`, `useCount`. Type annotations parse through the TypeScript grammar fallback; Flow's `hook` and `component` declarations break every grammar here, so they yield no entity.
- **JavaScript, `route_source`** (partial): fails `starts`.
- **Go, `nested_definition`** (partial): fails `admitted`.
- **Java, `declaration_extent`** (fail): fails `Ledger.total`.
- **Java, `create_remove_spacing`** (partial): fails `remove_documented`.
- **Java, `route_source`** (partial): fails `starts`.
- **Java, `route_create_sibling`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **Java, `route_remove`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **Kotlin, `entities`** (partial): fails `Field Account.balance`, `unexpected StaticVar Account.balance`. Fields are emitted as StaticVar; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **Kotlin, `declaration_extent`** (fail): fails `total`.
- **Kotlin, `nested_definition`** (partial): fails `admitted`.
- **Kotlin, `create_remove_spacing`** (partial): fails `remove_documented`.
- **Kotlin, `form_field`** (partial): fails `Field Point.norm`, `Field Point.x`, `Field Point.y`. Fields are emitted as StaticVar; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **Kotlin, `route_source`** (partial): fails `starts`.
- **Kotlin, `route_create_sibling`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **Kotlin, `route_remove`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **C, `module_name`** (refused): The adapter mints no module entity.
- **C, `module_contains`** (refused): The adapter mints no module entity.
- **C, `route_create_sibling`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **C, `route_remove`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **C++, `module_name`** (refused): The adapter mints no module entity.
- **C++, `module_contains`** (refused): The adapter mints no module entity.
- **C++, `declaration_extent`** (fail): fails `total`.
- **C++, `create_remove_spacing`** (partial): fails `remove_documented`.
- **C++, `form_bound_lambda`** (fail): fails `Function|Constant|StaticVar square`, `square`.
- **C++, `route_source`** (partial): fails `starts`.
- **C++, `route_create_sibling`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **C++, `route_remove`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **C#, `entities`** (partial): fails `Field Shop.Account.balance`, `unexpected Constant Shop.Account.balance`. Fields are emitted as Constant; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **C#, `module_name`** (refused): The adapter mints no module entity.
- **C#, `module_contains`** (refused): The adapter mints no module entity.
- **C#, `nested_definition`** (fail): fails `accounted`, `admitted`.
- **C#, `form_field`** (partial): fails `Field Shop.Forms.Label`, `Field Shop.Forms.count`. Fields are emitted as Constant; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **C#, `route_create_sibling`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **C#, `route_remove`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **Ruby, `module_name`** (refused): The adapter mints no module entity.
- **Ruby, `module_contains`** (refused): The adapter mints no module entity.
- **Ruby, `nested_definition`** (fail): fails `accounted`, `admitted`.
- **Ruby, `form_field`** (fail): fails `Field|Method Shop.Forms.count`, `Field|Method Shop.Forms.label`, `Shop.Forms.count`, `Shop.Forms.label`.
- **Ruby, `route_create_sibling`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **Ruby, `route_remove`** (refused): Nothing was written: entity lifecycle requires a top-level leaf function, or in TypeScript, JavaScript, Python, Go or Rust a method or a class.
- **PHP, `declaration_extent`** (fail): fails `total`.
- **PHP, `create_remove_spacing`** (partial): fails `remove_documented`.
- **PHP, `route_source`** (partial): fails `starts`.
- **PHP, `route_create_sibling`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **PHP, `route_remove`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **Swift, `entities`** (partial): fails `Field Account.balance`, `unexpected StaticVar Account.balance`. Fields are emitted as StaticVar; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **Swift, `declaration_extent`** (fail): fails `total`.
- **Swift, `create_remove_spacing`** (partial): fails `remove_documented`.
- **Swift, `form_field`** (partial): fails `Field Point.norm`, `Field Point.x`, `Field Point.y`. Fields are emitted as StaticVar; re-kinding them to field waits until member resolution learns the field kind, with the linker dialect work, since doing it first would cost these members their references.
- **Swift, `route_source`** (partial): fails `starts`.
- **Swift, `route_create_sibling`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **Swift, `route_remove`** (refused): Nothing was written: entity lifecycle currently supports top-level leaf functions in TypeScript, JavaScript, Python, Go and Rust.
- **HCL, `tests_detected`** (refused): Kin parses .tf and .tfvars as HCL, so a Terraform test (.tftest.hcl) is held as an unparsed entity and no test is detected.

### Not applicable

- **Python, `form_default_export`**: Python has no default export.
- **Python, `form_flow_types`**: Flow annotates JavaScript only.
- **TypeScript, `form_flow_types`**: Flow annotates JavaScript, not TypeScript.
- **JavaScript, `form_overload`**: JavaScript has no overload declarations.
- **JavaScript, `form_nested_type`**: JavaScript declares no class inside a class body or a namespace.
- **Go, `form_async`**: Go has no async functions; a goroutine runs an ordinary one.
- **Go, `form_default_export`**: Go has no default export.
- **Go, `form_overload`**: Go has no overloads.
- **Go, `form_nested_type`**: Go declares no type inside a type.
- **Go, `form_decorated`**: Go has no decorators or annotations; a struct tag is field metadata.
- **Go, `form_flow_types`**: Flow annotates JavaScript only.
- **Rust, `form_generator`**: Rust has no stable generator functions.
- **Rust, `form_default_export`**: Rust has no default export.
- **Rust, `form_overload`**: Rust has no overloads.
- **Rust, `form_nested_type`**: Rust declares no type inside a type.
- **Rust, `form_flow_types`**: Flow annotates JavaScript only.
- **Java, `form_generator`**: Java has no generator functions.
- **Java, `form_async`**: Java has no async functions.
- **Java, `form_default_export`**: Java has no default export.
- **Java, `form_flow_types`**: Flow annotates JavaScript only.
- **Kotlin, `form_default_export`**: Kotlin has no default export.
- **Kotlin, `form_flow_types`**: Flow annotates JavaScript only.
- **C, `nested_definition`**: C has no nested function definitions; GNU C's are an extension.
- **C, `call_receiver`**: C has no methods and no receivers.
- **C, `form_generator`**: C has no generator functions.
- **C, `form_async`**: C has no async functions.
- **C, `form_bound_lambda`**: C has no anonymous functions to bind.
- **C, `form_default_export`**: C has no default export.
- **C, `form_overload`**: C has no overloads.
- **C, `form_nested_type`**: A struct declared inside a struct in C has file scope, so C has no nested type.
- **C, `form_flow_types`**: Flow annotates JavaScript only.
- **C, `route_context`**: C has no types with methods.
- **C++, `form_generator`**: C++ has no generator functions; std::generator is a library type over coroutines.
- **C++, `form_async`**: C++ has no async functions.
- **C++, `form_default_export`**: C++ has no default export.
- **C++, `form_flow_types`**: Flow annotates JavaScript only.
- **C#, `form_default_export`**: C# has no default export.
- **C#, `form_flow_types`**: Flow annotates JavaScript only.
- **Ruby, `form_async`**: Ruby has no async functions.
- **Ruby, `form_default_export`**: Ruby has no default export.
- **Ruby, `form_overload`**: Ruby has no overloads.
- **Ruby, `form_decorated`**: Ruby has no decorators or annotations.
- **Ruby, `form_flow_types`**: Flow annotates JavaScript only.
- **PHP, `form_async`**: PHP has no async functions.
- **PHP, `form_bound_lambda`**: PHP binds a closure to a variable, and a constant cannot hold one.
- **PHP, `form_default_export`**: PHP has no default export.
- **PHP, `form_overload`**: PHP has no overloads.
- **PHP, `form_nested_type`**: PHP declares no class inside a class.
- **PHP, `form_flow_types`**: Flow annotates JavaScript only.
- **Swift, `form_generator`**: Swift has no generator functions.
- **Swift, `form_default_export`**: Swift has no default export.
- **Swift, `form_flow_types`**: Flow annotates JavaScript only.
- **HCL, `module_name`**: A Terraform module is a directory, so HCL has no per-source module coordinate.
- **HCL, `module_contains`**: A Terraform module is a directory, so HCL has no per-source module entity to contain blocks.
- **HCL, `nested_definition`**: HCL has no functions to nest definitions in.
- **HCL, `call_same_file`**: HCL has no user-defined functions to call.
- **HCL, `call_receiver`**: HCL has no methods and no receivers.
- **HCL, `body_edit_keeps_entities`**: HCL has no function bodies.
- **HCL, `form_generator`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_async`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_bound_lambda`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_default_export`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_overload`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_nested_type`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_decorated`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_field`**: HCL declares blocks and attributes, not functions, types or fields.
- **HCL, `form_flow_types`**: Flow annotates JavaScript only.
- **HCL, `route_refs`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_impact`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_context`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_patch`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_body_edit`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_create_sibling`**: HCL declares blocks, not functions, callers or types.
- **HCL, `route_remove`**: HCL declares blocks, not functions, callers or types.

### Capabilities

- `parse_clean` (parser): Every source parses with no error node, and extraction calls it valid.
- `entities` (parser): Each function, type and method is extracted with its kind and its stored name, which carries the language's member rule.
- `module_name` (parser): The module carries a module entity, named by the language's module rule.
- `module_contains` (parser): The module entity contains each of its top-level functions, constants, classes and types, directly or through a namespace it contains.
- `declaration_extent` (parser): An entity's span starts at its whole declaration: documentation, attributes, decorators and export keywords included.
- `census_clean` (parser): The adapter takes a declaration census, and every callable, type and namespace declaration it sees is admitted or local to a callable; HCL also grades its configuration bindings.
- `nested_definition` (parser): A definition nested in a function body is admitted under the shared `<locals>` name, or at least recorded as not admitted.
- `call_same_file` (parser): A call to a function in the same module is a call edge from the caller.
- `call_receiver` (parser): A call through the receiver is a call edge from the method that carries the owner.
- `imports` (parser): The import of the helper is read with its module path and the names it binds.
- `tests_detected` (parser): The framework test is detected, with its runner.
- `refuse_unparseable` (parser): A broken copy of the module is read as broken, at the line of the break.
- `placement_three_forms` (parser): An entity's code sent back as read, at the margin, or indented as a whole is placed where it stood.
- `create_remove_spacing` (parser): A declaration added after a sibling parses and is spaced like its neighbours, removing it restores the source, and removing a documented declaration takes its documentation.
- `body_edit_keeps_entities` (parser): A statement added inside a function body, such as an assignment to a member of a local object, parses and leaves the extracted entities as they were.
- `form_generator` (parser): A generator function is an entity.
- `form_async` (parser): An async function is an entity.
- `form_bound_lambda` (parser): An anonymous function bound to a name at module or type scope is an entity under that name.
- `form_default_export` (parser): A default-exported function is an entity under its own name.
- `form_overload` (parser): An overloaded function or method is an entity.
- `form_nested_type` (parser): A type declared inside a type or namespace is an entity under its qualified name, and so are its methods.
- `form_decorated` (parser): A declaration carrying decorators, annotations or attributes is an entity.
- `form_field` (parser): Fields and properties are entities of kind field, under their owner-qualified names.
- `form_flow_types` (parser): A Flow-annotated JavaScript unit (annotations, maybe types, exact objects, opaque types, Flow's `hook` and `component` declarations) yields its functions as entities.
- `route_source` (route): Reading an entity through `source` serves its whole declaration, starting where its span does.
- `route_locate` (route): `locate` by an entity's name finds it.
- `route_refs` (route): `refs` on a function carries the function in the same module that calls it.
- `route_impact` (route): `impact` on a helper reaches the caller in another module.
- `route_context` (route): `context` on a type carries its methods.
- `route_update` (route): A whole-code `update` of the served code with one token changed lands and writes exactly that change.
- `route_patch` (route): An anchored `patch` inside a method lands and writes exactly that change.
- `route_body_edit` (route): An `update` that adds a statement inside a function body, such as a member assignment, lands as a plain change.
- `route_create_sibling` (route): `create` beside a function lands and the graph holds the new declaration.
- `route_remove` (route): `remove` of a function takes its whole declaration out.
<!-- language-scorecard:end -->

## Shallow syntax support

The shallow tier exists in the pipeline but currently routes no extensions.
Every language with a shallow grammar also has a full entity-extraction
adapter, so those files take the full semantic path instead.

Extensions with neither a full adapter nor a wired grammar are stored as
opaque artifacts: Scala, Lua, R, Zig, Elixir, Erlang, Haskell, OCaml, Perl.
Treat these as unsupported for semantic extraction today.

## LSP enrichment

When the corresponding language server binary is installed, Kin adds
type-resolved relations on top of the parser output (call hierarchy, overrides,
uses-type, references), each tagged with LSP origin and a confidence weight that
classifies it as `type_resolved` rather than as a name match.

Two facts have to hold together before any of those edges can exist: the daemon
has to wire an adapter for the language, and a server binary has to be installed
on the host. The daemon wires **Rust, Python, TypeScript, JavaScript and Go**,
which is what `kin_core::reference_coverage::ENRICHABLE_LANGUAGES` names and what
a test in `kin_daemon` holds it to. Every other language carries no reference,
override or uses-type edge by construction, whatever is installed. kin-lsp does
carry adapters for Java, C and C++, and the table above says so, but those
adapters are not reached from the runtime today. That is a decision rather than
an oversight: they are named on `NOT_YET_WIRED` in `kin_daemon`, and a test
there fails if kin-lsp registers a provider that is neither wired nor listed.

This matters because cross-file resolution without a language server falls back
to matching bare names, which produces edges that name a plausible destination
rather than a proven one.

Install the servers with:

```
npm install -g pyright                                      # Python
npm install -g typescript-language-server typescript@^5     # TypeScript and JavaScript
rustup component add rust-analyzer                          # Rust
go install golang.org/x/tools/gopls@v0.22.0                 # Go
```

Both version pins are load-bearing. TypeScript 7 ships no `lib/tsserver.js`, so
an unpinned `typescript` resolves to a version `typescript-language-server`
cannot start. An unpinned `gopls@latest` gives two machines set up a week apart
two different servers over one repository, with nothing recording which one
produced an edge.

`go install` writes into `$(go env GOPATH)/bin`, which is on nobody's PATH by
default, and the daemon starts a server with a bare `gopls`. So run the command
above only if that directory is already on your PATH. Otherwise let Kin install
it: `kin doctor --fix --install-language-servers` sets `GOBIN` to a directory
under `KIN_HOME` that both Kin binaries append to PATH at startup, which is the
one destination the daemon is guaranteed to find.

`kin setup` and `kin doctor --fix` will offer to run these for you, for the
languages the repository uses and no others. Neither installs without consent:
interactively they ask once per command with the download disclosed, and the
answer defaults to yes when the toolchain the server needs is installed.
Non-interactively they change nothing unless `--install-language-servers` is
passed. Kin runs an installer only when it is on your own PATH; one it finds
only in a usual install place is named with the command to run yourself.

A missing server used to be skipped silently. `kin doctor` now reports it as an
actionable gap naming the language, what the gap costs, and the exact command
that closes it. Import and call edges are still resolved from source with no
language server involved, so the loss is bounded to the edge classes that need a
resolved program.

The daemon discovers servers once at startup. A server installed while a daemon
is already running is picked up without a restart only if that daemon found at
least one server when it started; on a host that had none, the enrichment
channel is never opened, so run `kin daemon stop` and let the next command start
a fresh one.

## Everything else

Any file without an adapter is stored as an opaque artifact: content-addressed
and versioned, visible to file-level operations, invisible to semantic
queries. Markdown, JSON, YAML, and other structured-text formats currently
fall in this tier unless a structured-artifact extractor matches them.
