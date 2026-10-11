// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What the names of a JavaScript or TypeScript file hold, as far as a key
//! read from one goes: a number, an array, a symbol or a string constant.
//! [`super::javascript_escape`] reads these facts to leave out lookups whose
//! key can never be a name and to keep the literal a constant key spells.
//!
//! Every fact is read off the writes of a name: its declarations, its
//! assignments and updates, the parameters, patterns, loop targets, catch
//! bindings, declarations and imports that bind it. A name holds a kind of
//! value only when every write of it does, settled as the greatest such
//! assignment across names that read each other. A declared type (`number`,
//! `Array<number>`) counts as what the program says the value is.

use std::collections::{HashMap, HashSet};

use tree_sitter::Node;

use crate::value_escape::{plain_string_literal, text_of};

use super::javascript_escape::TYPE_ONLY;

/// What a value written to a name is, as far as a key read from the name
/// goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Written {
    /// A number, whatever the other names written here hold (see
    /// [`NameFacts::collect`]).
    Numeric,
    /// Numeric when these needs hold.
    NumericIf(Vec<Need>),
    /// An index into an array: numeric when these names hold arrays.
    IndexIf(Vec<String>),
    /// An array.
    Array,
    /// An array when these names hold arrays.
    ArrayIf(Vec<String>),
    /// An array of numbers: a parameter or declaration its type declares one,
    /// or a typed array.
    NumberArray,
    /// A symbol: never a string key.
    Symbol,
    /// A `const` or `let` declaration of this string literal.
    Constant(String),
    /// Anything else, or a binding no initializer settles.
    Other,
}

/// What a numeric write needs of a name it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Need {
    /// The name holds a number.
    Number(String),
    /// The name holds an array of numbers, which an element read yields.
    NumberArray(String),
}

/// What each name holds across every write of it in one file: a
/// declaration, an assignment, an update, a parameter, a pattern, a loop
/// target, an import or a catch binding. A name is read by its spelling, not
/// its scope, so two bindings that share a name share one answer, which can
/// only lose precision, never soundness. Numbers are settled per scope
/// instead (see [`Walker::settle_numeric`]), where names like `i` and
/// `index` are bound over and over.
#[derive(Debug, Default)]
pub(super) struct NameFacts {
    /// Names every write of which is an array.
    pub(super) array: HashSet<String>,
    /// Names every write of which is an array of numbers.
    pub(super) number_array: HashSet<String>,
    /// Names every write of which is a symbol.
    pub(super) symbol: HashSet<String>,
    /// Names written exactly once, by a `const` or `let` declaration of a
    /// string literal.
    pub(super) constant: HashMap<String, String>,
    /// How many times each name is written.
    pub(super) writes: HashMap<String, usize>,
}

impl NameFacts {
    pub(super) fn collect(root: Node<'_>, source: &[u8]) -> Self {
        let mut written: HashMap<String, Vec<Written>> = HashMap::new();
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            collect_writes(node, source, &mut written);
            let mut cursor = node.walk();
            stack.extend(node.named_children(&mut cursor));
        }
        // The greatest assignment of arrays, then of numbers, consistent
        // with every write: start from every name whose writes could all be
        // of the kind and drop one while any write needs a name already
        // dropped.
        let array = greatest(&written, |write, array| match write {
            Written::Array | Written::NumberArray => Some(true),
            Written::ArrayIf(needs) => Some(needs.iter().all(|need| array.contains(need))),
            _ => None,
        });
        let number_array = greatest(&written, |write, _| match write {
            Written::NumberArray => Some(true),
            _ => None,
        });
        let symbol = written
            .iter()
            .filter(|(_, writes)| writes.iter().all(|write| *write == Written::Symbol))
            .map(|(name, _)| name.clone())
            .collect();
        let constant = written
            .iter()
            .filter_map(|(name, writes)| match writes.as_slice() {
                [Written::Constant(text)] => Some((name.clone(), text.clone())),
                _ => None,
            })
            .collect();
        let writes = written
            .iter()
            .map(|(name, writes)| (name.clone(), writes.len()))
            .collect();
        Self {
            array,
            number_array,
            symbol,
            constant,
            writes,
        }
    }
}

/// The greatest set of names every write of which `holds` accepts, given the
/// set itself: `holds` answers `None` for a write that is never of the kind,
/// and whether it is, given the names still in the set, otherwise.
pub(super) fn greatest(
    written: &HashMap<String, Vec<Written>>,
    holds: impl Fn(&Written, &HashSet<String>) -> Option<bool>,
) -> HashSet<String> {
    let everything: HashSet<String> = HashSet::new();
    let mut set: HashSet<String> = written
        .iter()
        .filter(|(_, writes)| {
            !writes.is_empty()
                && writes
                    .iter()
                    .all(|write| holds(write, &everything).is_some())
        })
        .map(|(name, _)| name.clone())
        .collect();
    loop {
        let dropped: Vec<String> = set
            .iter()
            .filter(|name| {
                written[*name]
                    .iter()
                    .any(|write| holds(write, &set) != Some(true))
            })
            .cloned()
            .collect();
        if dropped.is_empty() {
            return set;
        }
        for name in dropped {
            set.remove(&name);
        }
    }
}

/// Record the writes `node` makes itself, without its children.
pub(super) fn collect_writes(
    node: Node<'_>,
    source: &[u8],
    written: &mut HashMap<String, Vec<Written>>,
) {
    let write = |written: &mut HashMap<String, Vec<Written>>, name: Node<'_>, value: Written| {
        if name.kind() == "identifier" {
            written
                .entry(text_of(name, source).to_string())
                .or_default()
                .push(value);
        }
    };
    match node.kind() {
        "variable_declarator" => {
            let Some(name) = node.child_by_field_name("name") else {
                return;
            };
            if name.kind() != "identifier" {
                // A destructuring pattern: its identifiers are written below.
                return;
            }
            let lexical = node
                .parent()
                .is_some_and(|declaration| declaration.kind() == "lexical_declaration");
            let value = match node.child_by_field_name("value") {
                Some(value) => written_value(value, source, lexical),
                None => Written::Other,
            };
            write(written, name, value);
        }
        "as_expression" | "satisfies_expression" => {}
        "assignment_expression" => {
            if let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) {
                let value = match written_value(right, source, false) {
                    // A reassignment is no constant.
                    Written::Constant(_) => Written::Other,
                    value => value,
                };
                write(written, left, value);
            }
        }
        "augmented_assignment_expression" => {
            let (Some(left), Some(right)) = (
                node.child_by_field_name("left"),
                node.child_by_field_name("right"),
            ) else {
                return;
            };
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| text_of(operator, source))
                .unwrap_or("");
            let value = match operator {
                // Arithmetic assignment always stores a number.
                "-=" | "*=" | "/=" | "%=" | "**=" | "<<=" | ">>=" | ">>>=" | "&=" | "|=" | "^=" => {
                    Written::Numeric
                }
                // `+=` adds a number only to a number; `&&=`, `||=` and
                // `??=` store the right side or keep the left.
                _ => match written_value(right, source, false) {
                    Written::Numeric => {
                        Written::NumericIf(vec![Need::Number(text_of(left, source).into())])
                    }
                    Written::NumericIf(mut needs) => {
                        needs.push(Need::Number(text_of(left, source).into()));
                        Written::NumericIf(needs)
                    }
                    _ => Written::Other,
                },
            };
            write(written, left, value);
        }
        "update_expression" => {
            if let Some(argument) = node.child_by_field_name("argument") {
                write(written, argument, Written::Numeric);
            }
        }
        // Bindings no initializer settles: parameters, patterns, loop
        // targets, catch bindings, declarations and imports.
        "formal_parameters" => {
            let index = callback_index(node, source);
            let mut cursor = node.walk();
            for (position, parameter) in node.named_children(&mut cursor).enumerate() {
                let name = parameter
                    .child_by_field_name("pattern")
                    .or_else(|| parameter.child_by_field_name("name"))
                    .unwrap_or(parameter);
                if name.kind() != "identifier" {
                    bind_pattern_names(parameter, source, written);
                    continue;
                }
                let value = match (&index, declared_type(parameter, source)) {
                    (Some((at, needs)), _) if *at == position => Written::IndexIf(needs.clone()),
                    (_, Some(DeclaredType::Number)) => Written::Numeric,
                    (_, Some(DeclaredType::Array)) => Written::Array,
                    (_, Some(DeclaredType::NumberArray)) => Written::NumberArray,
                    _ => Written::Other,
                };
                write(written, name, value);
            }
        }
        "arrow_function" => {
            if let Some(parameter) = node.child_by_field_name("parameter") {
                bind_pattern_names(parameter, source, written);
            }
        }
        "object_pattern" | "array_pattern" => {
            // Only a destructuring declaration or assignment: parameters are
            // bound above.
            let binds_here = node.parent().is_some_and(|parent| {
                matches!(
                    parent.kind(),
                    "variable_declarator" | "assignment_expression" | "for_in_statement"
                )
            });
            if binds_here {
                bind_pattern_names(node, source, written);
            }
        }
        "for_in_statement" => {
            if let Some(left) = node.child_by_field_name("left") {
                if left.kind() == "identifier" {
                    write(written, left, Written::Other);
                }
            }
        }
        "catch_clause" => {
            if let Some(parameter) = node.child_by_field_name("parameter") {
                bind_pattern_names(parameter, source, written);
            }
        }
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "function"
        | "generator_function"
        | "class_declaration"
        | "class"
        | "abstract_class_declaration" => {
            if let Some(name) = node.child_by_field_name("name") {
                write(written, name, Written::Other);
            }
        }
        "import_specifier" => {
            if let Some(name) = node
                .child_by_field_name("alias")
                .or_else(|| node.child_by_field_name("name"))
            {
                write(written, name, Written::Other);
            }
        }
        "import_clause" | "namespace_import" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "identifier" {
                    write(written, child, Written::Other);
                }
            }
        }
        _ => {}
    }
}

/// Every identifier a parameter or pattern binds, as a write no initializer
/// settles.
pub(super) fn bind_pattern_names(
    pattern: Node<'_>,
    source: &[u8],
    written: &mut HashMap<String, Vec<Written>>,
) {
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => {
                written
                    .entry(text_of(node, source).to_string())
                    .or_default()
                    .push(Written::Other);
            }
            // A default value and a computed key are reads, and a pair's key
            // binds nothing.
            "assignment_pattern" | "object_assignment_pattern" => {
                if let Some(left) = node.child_by_field_name("left") {
                    stack.push(left);
                }
            }
            "pair_pattern" => {
                if let Some(value) = node.child_by_field_name("value") {
                    stack.push(value);
                }
            }
            "required_parameter" | "optional_parameter" => {
                if let Some(inner) = node
                    .child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("name"))
                {
                    stack.push(inner);
                }
            }
            kind if TYPE_ONLY.contains(&kind) => {}
            "member_expression" | "subscript_expression" | "this" => {}
            _ => {
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
            }
        }
    }
}

/// The text a decimal integer literal converts to, when `node` is one
/// written in its canonical form.
pub(super) fn integer_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    if node.kind() != "number" {
        return None;
    }
    let text = text_of(node, source);
    let canonical = !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
        && text.len() < 16;
    canonical.then(|| text.to_string())
}

/// Whether anything under `node` writes one of `names`: an assignment, an
/// update, a declaration, a parameter, a pattern or a loop target.
pub(super) fn writes_any(node: Node<'_>, source: &[u8], names: &[&str]) -> bool {
    let mut written = HashMap::new();
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        collect_writes(current, source, &mut written);
        let mut cursor = current.walk();
        stack.extend(current.named_children(&mut cursor));
    }
    names.iter().any(|name| written.contains_key(*name))
}

/// What writing `value` stores, before the names it reads are settled.
/// `lexical` says whether the write is a `const` or `let` declaration, the
/// only write a constant may come from.
pub(super) fn written_value(value: Node<'_>, source: &[u8], lexical: bool) -> Written {
    if lexical {
        if let Some(text) = plain_string_literal(value, source) {
            return Written::Constant(text);
        }
    }
    if is_symbol_expression(value, source, &HashSet::new()) {
        return Written::Symbol;
    }
    let mut needs = Vec::new();
    if numeric_expression(value, source, &mut needs) {
        return if needs.is_empty() {
            Written::Numeric
        } else {
            Written::NumericIf(needs)
        };
    }
    if value.kind() == "new_expression"
        && value
            .child_by_field_name("constructor")
            .is_some_and(|constructor| TYPED_ARRAYS.contains(&text_of(constructor, source)))
    {
        return Written::NumberArray;
    }
    let mut needs = Vec::new();
    if array_expression(value, source, &mut needs) {
        return if needs.is_empty() {
            Written::Array
        } else {
            Written::ArrayIf(needs)
        };
    }
    Written::Other
}

/// Arrays whose every element is a number.
pub(super) const TYPED_ARRAYS: &[&str] = &[
    "Int8Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "Int16Array",
    "Uint16Array",
    "Int32Array",
    "Uint32Array",
    "Float32Array",
    "Float64Array",
];

/// A type a declaration or parameter annotation names.
pub(super) enum DeclaredType {
    Number,
    Array,
    NumberArray,
}

/// The type `node`'s annotation declares, when it is `number` or an array.
pub(super) fn declared_type(node: Node<'_>, source: &[u8]) -> Option<DeclaredType> {
    let annotation = node.child_by_field_name("type").or_else(|| {
        let mut cursor = node.walk();
        let found = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "type_annotation");
        found
    })?;
    annotated_type(annotation, source)
}

/// The type an annotation or type node names, when it is `number` or an
/// array: `T[]`, `Array<T>`, `ReadonlyArray<T>` or `$ReadOnlyArray<T>`.
fn annotated_type(node: Node<'_>, source: &[u8]) -> Option<DeclaredType> {
    let text = text_of(node, source).trim_start_matches(':').trim();
    if text == "number" {
        return Some(DeclaredType::Number);
    }
    let number_array = [
        "Array<number>",
        "ReadonlyArray<number>",
        "$ReadOnlyArray<number>",
        "number[]",
    ]
    .contains(&text)
        || TYPED_ARRAYS.contains(&text);
    if number_array {
        return Some(DeclaredType::NumberArray);
    }
    let generic = ["Array<", "ReadonlyArray<", "$ReadOnlyArray<"]
        .iter()
        .any(|head| text.starts_with(head) && text.ends_with('>'));
    // `T[]` whose outermost form is an array, not `(A | B[])`.
    let suffix =
        text.ends_with("[]") && !text.contains('|') && !text.contains('&') && !text.contains("=>");
    (generic || suffix).then_some(DeclaredType::Array)
}

/// Whether `node` always evaluates to an array, given that each name pushed
/// onto `needs` holds one.
pub(super) fn array_expression(node: Node<'_>, source: &[u8], needs: &mut Vec<String>) -> bool {
    match node.kind() {
        "array" => true,
        "parenthesized_expression" | "non_null_expression" => node
            .named_child(0)
            .is_some_and(|inner| array_expression(inner, source, needs)),
        "as_expression" | "satisfies_expression" => {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            children
                .first()
                .is_some_and(|inner| array_expression(*inner, source, needs))
        }
        "identifier" => {
            needs.push(text_of(node, source).to_string());
            true
        }
        "new_expression" => node
            .child_by_field_name("constructor")
            .is_some_and(|constructor| text_of(constructor, source) == "Array"),
        "call_expression" => {
            let Some(function) = node.child_by_field_name("function") else {
                return false;
            };
            if matches!(
                text_of(function, source),
                "Object.keys"
                    | "Object.values"
                    | "Object.entries"
                    | "Array.from"
                    | "Array.of"
                    | "Array"
            ) {
                return true;
            }
            // An array method that returns a new array, called on an array.
            function.kind() == "member_expression"
                && function
                    .child_by_field_name("property")
                    .is_some_and(|property| {
                        matches!(
                            text_of(property, source),
                            "map"
                                | "filter"
                                | "slice"
                                | "concat"
                                | "flat"
                                | "flatMap"
                                | "toSorted"
                                | "toReversed"
                                | "toSpliced"
                        )
                    })
                && function
                    .child_by_field_name("object")
                    .is_some_and(|object| array_expression(object, source, needs))
        }
        _ => false,
    }
}

/// When `parameters` belong to a callback handed to an array's iteration
/// method (`list.map((item, i) => ...)`), the position of the index
/// parameter and the names that must hold arrays for the receiver to be one.
pub(super) fn callback_index(parameters: Node<'_>, source: &[u8]) -> Option<(usize, Vec<String>)> {
    let callback = parameters.parent()?;
    if !matches!(
        callback.kind(),
        "arrow_function" | "function_expression" | "function"
    ) {
        return None;
    }
    let arguments = callback
        .parent()
        .filter(|parent| parent.kind() == "arguments")?;
    if arguments.named_child(0).map(|first| first.id()) != Some(callback.id()) {
        return None;
    }
    let function = arguments
        .parent()
        .filter(|call| call.kind() == "call_expression")?
        .child_by_field_name("function")
        .filter(|function| function.kind() == "member_expression")?;
    let position = match text_of(function.child_by_field_name("property")?, source) {
        "forEach" | "map" | "filter" | "some" | "every" | "find" | "findIndex" | "findLast"
        | "findLastIndex" | "flatMap" => 1,
        "reduce" | "reduceRight" => 2,
        _ => return None,
    };
    let mut needs = Vec::new();
    array_expression(function.child_by_field_name("object")?, source, &mut needs)
        .then_some((position, needs))
}

/// Whether `node` always evaluates to a number, given that each need pushed
/// onto `needs` holds.
pub(super) fn numeric_expression(node: Node<'_>, source: &[u8], needs: &mut Vec<Need>) -> bool {
    match node.kind() {
        "number" => true,
        "parenthesized_expression" => node
            .named_child(0)
            .is_some_and(|inner| numeric_expression(inner, source, needs)),
        "unary_expression" => node
            .child_by_field_name("operator")
            .is_some_and(|operator| matches!(text_of(operator, source), "-" | "+" | "~")),
        "update_expression" => true,
        "binary_expression" => {
            let operator = node
                .child_by_field_name("operator")
                .map(|operator| text_of(operator, source))
                .unwrap_or("");
            match operator {
                "-" | "*" | "/" | "%" | "**" | "|" | "&" | "^" | "<<" | ">>" | ">>>" => true,
                "+" => {
                    node.child_by_field_name("left")
                        .is_some_and(|left| numeric_expression(left, source, needs))
                        && node
                            .child_by_field_name("right")
                            .is_some_and(|right| numeric_expression(right, source, needs))
                }
                _ => false,
            }
        }
        "member_expression" => node
            .child_by_field_name("property")
            .is_some_and(|property| text_of(property, source) == "length"),
        "identifier" => {
            needs.push(Need::Number(text_of(node, source).to_string()));
            true
        }
        // An element of an array of numbers.
        "subscript_expression" => node
            .child_by_field_name("object")
            .filter(|object| object.kind() == "identifier")
            .is_some_and(|object| {
                needs.push(Need::NumberArray(text_of(object, source).to_string()));
                true
            }),
        // `x as number`: the value the program declares it to be.
        "as_expression" | "satisfies_expression" => {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            children.last().is_some_and(|annotation| {
                matches!(
                    annotated_type(*annotation, source),
                    Some(DeclaredType::Number)
                )
            })
        }
        "ternary_expression" => {
            node.child_by_field_name("consequence")
                .is_some_and(|value| numeric_expression(value, source, needs))
                && node
                    .child_by_field_name("alternative")
                    .is_some_and(|value| numeric_expression(value, source, needs))
        }
        "call_expression" => node
            .child_by_field_name("function")
            .is_some_and(|function| numeric_call(function, source)),
        _ => false,
    }
}

/// Whether a call of `function` always returns a number: `Math.*`,
/// `Number`, `parseInt`, `parseFloat`, `Date.now`, `performance.now`, and a
/// string's or array's index search.
pub(super) fn numeric_call(function: Node<'_>, source: &[u8]) -> bool {
    let text = text_of(function, source);
    if text.starts_with("Math.")
        || matches!(
            text,
            "parseInt" | "parseFloat" | "Number" | "Date.now" | "performance.now"
        )
    {
        return true;
    }
    function.kind() == "member_expression"
        && function
            .child_by_field_name("property")
            .is_some_and(|property| {
                matches!(
                    text_of(property, source),
                    "indexOf"
                        | "lastIndexOf"
                        | "findIndex"
                        | "findLastIndex"
                        | "charCodeAt"
                        | "codePointAt"
                        | "search"
                )
            })
}

/// Whether `node` always evaluates to a symbol: `Symbol(...)`,
/// `Symbol.for(...)`, a well-known symbol `Symbol.iterator`, or a name in
/// `symbols`.
pub(super) fn is_symbol_expression(
    node: Node<'_>,
    source: &[u8],
    symbols: &HashSet<String>,
) -> bool {
    match node.kind() {
        "parenthesized_expression" => node
            .named_child(0)
            .is_some_and(|inner| is_symbol_expression(inner, source, symbols)),
        "identifier" => symbols.contains(text_of(node, source)),
        "member_expression" => {
            node.child_by_field_name("object").is_some_and(|object| {
                object.kind() == "identifier" && text_of(object, source) == "Symbol"
            }) && node
                .child_by_field_name("property")
                .is_some_and(|property| text_of(property, source) != "prototype")
        }
        "call_expression" => node
            .child_by_field_name("function")
            .is_some_and(|function| matches!(text_of(function, source), "Symbol" | "Symbol.for")),
        _ => false,
    }
}
