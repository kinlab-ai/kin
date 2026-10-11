// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! What an import specifier's own syntax says about where its module lives.
//!
//! The linker resolves each import against the files the repository holds.
//! One it cannot resolve is either a module outside the repository, which is
//! an ordinary dependency and not a gap, or a module the resolver failed to
//! find, which is. Only the second should keep a file's import coverage from
//! reading complete. The specifier's root, the name that anchors it before any
//! path, separates them: when the repository holds no directory or file of that
//! name anywhere, no resolver could have found the module inside it, so the
//! import is external. When it does hold one, the import stays unresolved and
//! is listed, because a failed lookup of a module the repository may own is
//! exactly the case that must not be excused.
//!
//! How to find the root is a fact about each language's specifier syntax, so
//! adapters supply it through [`crate::LanguageAdapter::import_specifier_root`].

/// The root of one import specifier, as its language reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSpecifierRoot {
    /// A path relative to the importing file. Only a resolver can place it,
    /// so an unresolved one is a gap.
    Relative,
    /// A name the importing package itself maps, such as an ECMAScript
    /// `#internal` subpath. Never external.
    PackageInternal,
    /// A module the language's runtime supplies by syntax alone, such as a
    /// `node:` specifier. Always external.
    Builtin,
    /// Names anchoring the specifier. The import is external when the
    /// repository holds no directory or file named by any of them.
    Names(Vec<String>),
    /// This language's specifier syntax is not read here, so no import of it
    /// is proven external.
    Unknown,
}

/// Python: a leading dot is relative, and otherwise the first dotted segment is
/// the top-level package or module the import names.
pub(crate) fn python_specifier_root(module_path: &str) -> ImportSpecifierRoot {
    let specifier = module_path.trim();
    if specifier.is_empty() {
        return ImportSpecifierRoot::Unknown;
    }
    if specifier.starts_with('.') {
        return ImportSpecifierRoot::Relative;
    }
    let root = specifier.split('.').next().unwrap_or(specifier);
    ImportSpecifierRoot::Names(vec![root.to_string()])
}

/// ECMAScript: `.` and `/` are paths, `#` is the package's own import map,
/// `node:` is a runtime builtin, and anything else is a bare package specifier
/// whose first segment, or first two for a scoped `@scope/name`, name it.
pub(crate) fn ecmascript_specifier_root(module_path: &str) -> ImportSpecifierRoot {
    let specifier = module_path.trim();
    if specifier.is_empty() {
        return ImportSpecifierRoot::Unknown;
    }
    if specifier.starts_with('.') || specifier.starts_with('/') {
        return ImportSpecifierRoot::Relative;
    }
    if specifier.starts_with('#') {
        return ImportSpecifierRoot::PackageInternal;
    }
    if specifier.starts_with("node:") {
        return ImportSpecifierRoot::Builtin;
    }
    let mut segments = specifier.split('/');
    let first = segments.next().unwrap_or(specifier);
    if let Some(scope) = first.strip_prefix('@') {
        let mut names = vec![first.to_string()];
        if !scope.is_empty() {
            names.push(scope.to_string());
        }
        if let Some(name) = segments.next().filter(|name| !name.is_empty()) {
            names.push(name.to_string());
        }
        return ImportSpecifierRoot::Names(names);
    }
    ImportSpecifierRoot::Names(vec![first.to_string()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_roots() {
        assert_eq!(
            python_specifier_root("os.path"),
            ImportSpecifierRoot::Names(vec!["os".into()])
        );
        assert_eq!(python_specifier_root(".app"), ImportSpecifierRoot::Relative);
        assert_eq!(python_specifier_root(""), ImportSpecifierRoot::Unknown);
    }

    #[test]
    fn ecmascript_roots() {
        assert_eq!(
            ecmascript_specifier_root("shared/ReactFeatureFlags"),
            ImportSpecifierRoot::Names(vec!["shared".into()])
        );
        assert_eq!(
            ecmascript_specifier_root("@babel/core"),
            ImportSpecifierRoot::Names(vec!["@babel".into(), "babel".into(), "core".into()])
        );
        assert_eq!(
            ecmascript_specifier_root("./x"),
            ImportSpecifierRoot::Relative
        );
        assert_eq!(
            ecmascript_specifier_root("#dep"),
            ImportSpecifierRoot::PackageInternal
        );
        assert_eq!(
            ecmascript_specifier_root("node:fs"),
            ImportSpecifierRoot::Builtin
        );
    }
}
