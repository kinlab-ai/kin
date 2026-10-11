#!/usr/bin/env python3
"""Tests for the filesystem boundary lint's own logic (scripts/check-io-lint.py).

These need no toolchain: they feed the checker source text and cargo messages
it would otherwise get from clippy. The probe crate in scripts/io-lint/probe
proves the clippy side, and runs with the lint itself.
"""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest

SCRIPT = Path(__file__).resolve().parent / "check-io-lint.py"
SPEC = importlib.util.spec_from_file_location("check_io_lint", SCRIPT)
lint = importlib.util.module_from_spec(SPEC)
sys.modules["check_io_lint"] = lint
SPEC.loader.exec_module(lint)

SOURCE = textwrap.dedent(
    """\
    use std::path::Path;

    const LIMIT: u64 = 7;

    pub fn one_line(path: &Path) -> bool { path.exists() }

    pub fn outer(path: &Path) -> usize {
        let closure = |p: &Path| p.is_file();
        fn inner(path: &Path) -> bool {
            path.is_dir()
        }
        if closure(path) || inner(path) {
            1
        } else {
            0
        }
    }

    pub fn long_signature<P>(
        path: P,
    ) -> std::io::Result<Vec<u8>>
    where
        P: AsRef<Path>,
    {
        std::fs::read(path)
    }

    pub trait Reader {
        fn read(&self) -> Vec<u8>;

        fn read_twice(&self) -> Vec<u8> {
            self.read()
        }
    }

    impl Holder {
        pub(crate) async fn load(&self) -> usize {
            self.size
        }
    }
    """
)


def message(file_name, line, method, *, code="clippy::disallowed_methods", kind="lib", expansion=None, level="warning"):
    span = {"file_name": file_name, "line_start": line, "is_primary": True}
    if expansion is not None:
        span["expansion"] = {"span": {"file_name": file_name, "line_start": expansion, "is_primary": False}}
    return json.dumps(
        {
            "reason": "compiler-message",
            "target": {"kind": [kind]},
            "message": {
                "code": {"code": code} if code else None,
                "level": level,
                "message": f"use of a disallowed method `{method}`",
                "spans": [span],
                "rendered": f"{level}: {method}",
            },
        }
    )


class FunctionRanges(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "src").mkdir()
        (self.root / "src/lib.rs").write_text(SOURCE)
        self.index = lint.FunctionIndex(self.root)
        self.lines = SOURCE.split("\n")

    def at(self, needle):
        return next(i for i, text in enumerate(self.lines, 1) if needle in text)

    def enclosing(self, needle):
        return self.index.enclosing("src/lib.rs", self.at(needle))

    def test_each_line_names_its_innermost_function(self):
        self.assertEqual(self.enclosing("const LIMIT"), lint.MODULE_SCOPE)
        self.assertEqual(self.enclosing("path.exists()"), "one_line")
        self.assertEqual(self.enclosing("let closure"), "outer")
        self.assertEqual(self.enclosing("path.is_dir()"), "inner")
        self.assertEqual(self.enclosing("closure(path) || inner(path)"), "outer")
        self.assertEqual(self.enclosing("std::fs::read(path)"), "long_signature")
        self.assertEqual(self.enclosing("self.read()"), "read_twice")
        self.assertEqual(self.enclosing("self.size"), "load")

    def test_a_declaration_without_a_body_spans_nothing(self):
        names = [name for _, _, name in lint.function_ranges(self.lines)]
        self.assertEqual(names.count("read"), 0)
        self.assertIn("read_twice", names)


class Messages(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        (self.root / "crates/kin-x/src").mkdir(parents=True)
        (self.root / "crates/kin-x/src/lib.rs").write_text(SOURCE)
        self.index = lint.FunctionIndex(self.root)
        self.methods = {"std::fs::read", "std::path::Path::exists"}
        self.file = str(self.root / "crates/kin-x/src/lib.rs")

    def parse(self, *records):
        return lint.parse_messages(list(records), self.root, self.methods, self.index)

    def test_a_reported_call_becomes_a_site(self):
        found, errors = self.parse(message(self.file, 25, "std::fs::read"))
        self.assertEqual(errors, [])
        self.assertEqual(
            [(r.site, r.line) for r in found],
            [(lint.Site("crates/kin-x/src/lib.rs", "long_signature", "std::fs::read"), 25)],
        )

    def test_a_macro_site_belongs_to_the_function_that_invoked_it(self):
        found, _ = self.parse(message(self.file, 3, "std::fs::read", expansion=5))
        self.assertEqual(found[0].site.function, "one_line")
        self.assertEqual(found[0].line, 5)

    def test_the_same_site_from_two_targets_counts_once(self):
        found, _ = self.parse(
            message(self.file, 25, "std::fs::read", kind="lib"),
            message(self.file, 25, "std::fs::read", kind="bin"),
        )
        self.assertEqual(len(found), 1)

    def test_build_scripts_other_lints_and_other_methods_are_ignored(self):
        found, errors = self.parse(
            message(self.file, 25, "std::fs::read", kind="custom-build"),
            message(self.file, 25, "std::fs::read", code="clippy::needless_return"),
            message(self.file, 25, "std::fs::File::sync_all"),
            "not json",
            json.dumps({"reason": "compiler-artifact"}),
        )
        self.assertEqual((found, errors), ([], []))

    def test_a_compile_error_is_reported_as_an_unfinished_run(self):
        _, errors = self.parse(message(self.file, 9, "x", code="E0308", level="error"))
        self.assertEqual(len(errors), 1)


class Comparison(unittest.TestCase):
    def reported(self, file, function, method, line=1):
        return lint.Reported(lint.Site(file, function, method), line)

    def test_new_and_stale_sites(self):
        allowed = {
            lint.Site("crates/a/src/lib.rs", "load", "std::fs::read"),
            lint.Site("crates/a/src/lib.rs", "gone", "std::fs::write"),
            lint.Site("crates/b/src/lib.rs", "other", "std::fs::read"),
        }
        found = [
            self.reported("crates/a/src/lib.rs", "load", "std::fs::read"),
            self.reported("crates/a/src/lib.rs", "load", "std::fs::read", line=9),
            self.reported("crates/a/src/lib.rs", "answer", "std::fs::read_to_string"),
        ]
        new, stale = lint.compare(found, allowed, ["crates/a"])
        self.assertEqual([r.site.function for r in new], ["answer"])
        self.assertEqual([s.function for s in stale], ["gone"])
        _, stale_everywhere = lint.compare(found, allowed, None)
        self.assertEqual(sorted(s.function for s in stale_everywhere), ["gone", "other"])

    def test_a_second_call_of_a_listed_method_in_the_same_function_is_accepted(self):
        allowed = {lint.Site("crates/a/src/lib.rs", "load", "std::fs::read")}
        found = [self.reported("crates/a/src/lib.rs", "load", "std::fs::read", line=n) for n in (3, 4)]
        self.assertEqual(lint.compare(found, allowed, None), ([], []))

    def test_the_list_round_trips_sorted(self):
        path = Path(tempfile.mkdtemp()) / "allowed.txt"
        sites = {
            lint.Site("crates/b/src/lib.rs", "x", "std::fs::read"),
            lint.Site("crates/a/src/lib.rs", lint.MODULE_SCOPE, "std::fs::write"),
        }
        lint.write_allowed(path, sites)
        self.assertEqual(lint.load_allowed(path), sites)
        body = [line for line in path.read_text().splitlines() if line and not line.startswith("#")]
        self.assertEqual(body, sorted(body))

    def test_a_malformed_line_is_refused(self):
        path = Path(tempfile.mkdtemp()) / "allowed.txt"
        path.write_text("crates/a/src/lib.rs std::fs::read\n")
        with self.assertRaises(SystemExit):
            lint.load_allowed(path)


class Suppressions(unittest.TestCase):
    def test_attributes_that_silence_the_lint_are_found(self):
        text = textwrap.dedent(
            """\
            #![allow(clippy::all)]
            #[allow(clippy::disallowed_methods)]
            fn a() {}
            #[expect(clippy::style, reason = "x")]
            fn b() {}
            #[cfg_attr(feature = "fast", allow(warnings))]
            fn c() {}
            #[allow(
                dead_code,
                clippy::disallowed_types
            )]
            fn d() {}
            """
        )
        self.assertEqual([line for line, _ in lint.suppressions(text)], [1, 2, 4, 6, 8])

    def test_unrelated_and_test_only_attributes_are_not(self):
        text = textwrap.dedent(
            """\
            #![cfg_attr(test, allow(clippy::disallowed_methods))]
            #[cfg_attr(all(test, unix), allow(clippy::style))]
            #[allow(dead_code, clippy::too_many_arguments)]
            #[allow(clippy::all_the_things_named_differently)]
            fn a() {}
            """
        )
        self.assertEqual(lint.suppressions(text), [])

    def test_test_and_build_sources_are_not_audited(self):
        for path in (
            "crates/kin-x/tests/live.rs",
            "crates/kin-x/src/api_test.rs",
            "crates/kin-x/src/binding_tests.rs",
            "crates/kin-x/build.rs",
            "crates/kin-lsp/build/producer_identity.rs",
            "crates/kin-x/examples/dump.rs",
        ):
            self.assertTrue(lint.is_test_source(path), path)
        self.assertFalse(lint.is_test_source("crates/kin-x/src/build.rs"))
        self.assertFalse(lint.is_test_source("crates/kin-x/src/latest.rs"))


class Scope(unittest.TestCase):
    def test_scope_follows_arguments_then_the_ci_selection(self):
        self.assertEqual(lint.scope_from_arguments(["kin-io"], {}), ["kin-io"])
        self.assertIsNone(lint.scope_from_arguments([], {}))
        self.assertEqual(
            lint.scope_from_arguments([], {"RUST_TEST_PACKAGES": '["kin-mcp", "kin-db"]'}),
            ["kin-mcp", "kin-db"],
        )
        self.assertIsNone(
            lint.scope_from_arguments(
                [], {"RUST_TEST_PACKAGES": '["kin-mcp"]', "RUN_ALL_RUST_TESTS": "true"}
            )
        )


class Configuration(unittest.TestCase):
    def test_the_configuration_names_the_raw_entry_points_the_text_guard_denied(self):
        methods = lint.configured_methods(lint.CONFIG_DIR / "clippy.toml")
        for required in (
            "std::fs::read",
            "std::fs::read_to_string",
            "std::fs::read_dir",
            "std::fs::metadata",
            "std::fs::symlink_metadata",
            "std::fs::read_link",
            "std::fs::canonicalize",
            "std::fs::File::open",
            "std::fs::OpenOptions::open",
            "std::path::Path::exists",
            "std::path::Path::try_exists",
            "std::path::Path::is_file",
            "std::path::Path::is_dir",
            "std::path::Path::is_symlink",
            "std::path::Path::metadata",
            "std::path::Path::symlink_metadata",
            "std::path::Path::canonicalize",
            "std::path::Path::read_dir",
            "std::path::Path::read_link",
            "std::process::Command::new",
            "tokio::fs::read",
            "tokio::process::Command::new",
            "walkdir::WalkDir::new",
            "glob::glob",
            "fs_extra::file::read_to_string",
            "fs2::available_space",
            "filetime::set_file_mtime",
            "tempfile::NamedTempFile::new",
            "notify::recommended_watcher",
            "std::os::unix::fs::symlink",
            "std::os::wasi::fs::symlink_path",
            "cap_std::ambient_authority",
            "kin_blobs::authority::Grant::ambient",
        ):
            self.assertIn(required, methods)

    def test_every_probe_mark_names_a_configured_method(self):
        methods = lint.configured_methods(lint.CONFIG_DIR / "clippy.toml")
        source = (lint.PROBE_MANIFEST.parent / "src/lib.rs").read_text()
        marks = [m.group(2) for m in map(lint.PROBE_MARK.search, source.splitlines()) if m and m.group(2)]
        self.assertGreater(len(marks), 10)
        for mark in marks:
            self.assertIn(mark, methods)


if __name__ == "__main__":
    unittest.main()
