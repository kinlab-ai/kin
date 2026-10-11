// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The path conventions that mark test code, in one place.
//!
//! Kin reads a unit's role from where it sits as well as from what its
//! adapter detects. Several readers keep their own rule for that, and the
//! rules differ on purpose: the role classifier is a union across every
//! ecosystem's test conventions, and the traversal's nonproduction filter also
//! sets aside examples and benchmarks. They are gathered here so they are read
//! and changed together, each kept exactly as its reader uses it.

/// Whether a single path component names a test directory: its last
/// `[_-.]`-delimited word is an exact test keyword.
///
/// Word-level matching (never a raw substring) keeps a testing tool's shipped
/// product package (`_pytest`, `pytest`, `contest`, `latest`) out of the test
/// role, while still catching compound test dirs such as `unit_tests`,
/// `acceptance_test`, and `__tests__`. The last word is the one that names
/// what a compound directory holds: `unit_tests` holds tests, while
/// `react-test-renderer` is a renderer, `test-utils` utilities and
/// `testing-library` a library, products a repository ships for others'
/// tests. Their source stays source; a test file inside one is still a test
/// by its own name.
pub fn is_test_dir_component(component: &str) -> bool {
    const TEST_DIR_WORDS: [&str; 5] = ["test", "tests", "testing", "spec", "specs"];
    component
        .split(['_', '-', '.'])
        .rfind(|word| !word.is_empty())
        .is_some_and(|word| TEST_DIR_WORDS.contains(&word))
}

/// Whether a filename names a test by one of the conventions the ecosystems in
/// this registry actually use.
///
/// This was eight literal extensions, which classified `Foo.test.tsx`,
/// `FooTest.java`, `foo_spec.rb` and every other colocated framework test as
/// production source. A React repository with colocated tests read 100% Source,
/// and two linker gates that filter on `role != Test` then removed nothing,
/// because nothing was Test.
///
/// The rule is delimited rather than a bare suffix, and the delimiter is what
/// makes it safe to widen. `latest.java` ends with the letters `test.java`, and
/// a lowercased `ends_with` would call it a test; `release.rs` under a `latest/`
/// directory is the same hazard one level up. So a marker counts only when a
/// delimiter or a case boundary separates it from the rest of the stem:
///
/// - `test_foo.py`, delimited by the underscore that follows.
/// - `foo_test.go`, `foo_spec.rb`, delimited by the underscore before.
/// - `foo.test.tsx`, `foo.spec.js`, delimited by the dot before, which covers
///   every extension the JS and TS adapters claim rather than four of them.
/// - `FooTest.java`, `FooTests.cs`, `FooSpec.kt`, `FooTests.swift`, delimited by
///   the capital. This is the one case that reads the ORIGINAL spelling: the
///   lowercased form cannot tell `FooTest` from `latest`.
///
/// `original` is the filename as written; `lower` is the same string lowercased,
/// passed in because the caller already has it.
pub fn is_test_file_name(original: &str, lower: &str) -> bool {
    let stem_lower = lower.rsplit_once('.').map_or(lower, |(stem, _)| stem);
    if stem_lower.starts_with("test_")
        || stem_lower.ends_with("_test")
        || stem_lower.ends_with("_spec")
        || stem_lower.ends_with(".test")
        || stem_lower.ends_with(".spec")
    {
        return true;
    }
    // The capital is the delimiter. Matching on the original spelling is what
    // separates `FooTest` from `latest`, and lowercasing first is what made the
    // widened rule unsafe in the first place.
    let stem = original.rsplit_once('.').map_or(original, |(stem, _)| stem);
    for marker in ["Test", "Tests", "Spec", "Specs"] {
        if stem.len() > marker.len() && stem.ends_with(marker) {
            return true;
        }
    }
    false
}

/// Whether `path` is a test unit by where it sits or by its own name: a
/// directory on the way to it names a test tree ([`is_test_dir_component`]),
/// or its file name follows a test convention ([`is_test_file_name`]). This is
/// the test branch of the role classifier.
pub fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let components: Vec<&str> = lower.split('/').filter(|c| !c.is_empty()).collect();
    let file_name = components.last().copied().unwrap_or("");
    let dir_len = components.len().saturating_sub(1);

    // Test paths. A directory is a test tree only when one of its words is an
    // exact test keyword (component match, never a substring), so `_pytest/` and
    // peers are not swept in. Test *files* are recognized by filename convention
    // regardless of the directory they live in.
    let in_test_dir = components
        .iter()
        .take(dir_len)
        .any(|c| is_test_dir_component(c));
    // The ORIGINAL spelling, taken from `path` rather than from `lower`, because
    // the capital in `FooTest` is a delimiter and lowercasing destroys it.
    let original_file_name = path.rsplit('/').next().unwrap_or("");
    let is_test_file = is_test_file_name(original_file_name, file_name);
    in_test_dir || is_test_file
}

/// Whether `path` holds code a production change does not reach: a test by
/// one of the common file conventions, or anything under a test, example or
/// benchmark directory. The traversal's dead-code and impact readers use this
/// wider rule.
pub fn is_nonproduction_path(path: &str) -> bool {
    let normalized = path.to_ascii_lowercase().replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or(&normalized);

    file_name == "conftest.py"
        || file_name.starts_with("test_")
        || file_name.ends_with("_test.py")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.go")
        || file_name.ends_with(".spec.ts")
        || file_name.ends_with(".spec.tsx")
        || file_name.ends_with(".spec.js")
        || file_name.ends_with(".spec.jsx")
        || normalized.contains("/test/")
        || normalized.contains("/tests/")
        || normalized.contains("/testing/")
        || normalized.starts_with("test/")
        || normalized.starts_with("tests/")
        || normalized.starts_with("testing/")
        || normalized.contains("/example/")
        || normalized.contains("/examples/")
        || normalized.starts_with("example/")
        || normalized.starts_with("examples/")
        || normalized.contains("/bench/")
        || normalized.contains("/benches/")
        || normalized.contains("/benchmark/")
        || normalized.contains("/benchmarks/")
        || normalized.starts_with("bench/")
        || normalized.starts_with("benches/")
        || normalized.starts_with("benchmark/")
        || normalized.starts_with("benchmarks/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_role_rule_and_the_nonproduction_rule_are_each_kept_as_written() {
        assert!(is_test_path("src/foo_test.go"));
        assert!(is_test_path("src/Foo.spec.tsx"));
        assert!(is_test_path("tests/render.js"));
        assert!(!is_test_path("_pytest/main.py"));
        assert!(!is_test_path("src/latest.java"));
        // Examples and benchmarks are not tests, but production does not reach them.
        assert!(!is_test_path("examples/demo.rs"));
        assert!(is_nonproduction_path("examples/demo.rs"));
        assert!(is_nonproduction_path("benches/parse.rs"));
        assert!(is_nonproduction_path("pkg/conftest.py"));
        assert!(!is_nonproduction_path("src/lib.rs"));
    }
}
