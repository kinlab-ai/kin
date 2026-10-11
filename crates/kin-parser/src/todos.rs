// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Inline TODO/FIXME/HACK/NOTE markers in held text.
//!
//! The scan reads text a caller already holds, a code entity's source or an
//! unparsed entity's text, and returns structured markers suitable for
//! conversion into work graph items. It never reaches for a working tree:
//! where the text comes from is the caller's boundary, and the TODO import
//! reads it from the graph's own content.

/// One inline TODO/FIXME/HACK/NOTE marker.
#[derive(Debug, Clone)]
pub struct ExtractedTodo {
    /// The marker kind: "TODO", "FIXME", "HACK", "NOTE".
    pub kind: String,
    /// The body text after the marker.
    pub body: String,
    /// The repository path of the entity the marker sits in.
    pub file_path: String,
    /// 1-based line number.
    pub line_number: usize,
}

/// Markers we scan for (case-insensitive match on the tag).
const TODO_MARKERS: &[&str] = &["TODO", "FIXME", "HACK", "NOTE"];

/// How much a line must say before it counts as a marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerForm {
    /// Code: a comment line whose marker is followed by `:`, `(`, `-`, a
    /// space, or nothing.
    Comment,
    /// Unparsed text: the same lines, but the marker must be followed by `:`.
    /// Kin does not know an unparsed entity's comment syntax, so a `#` may
    /// open a heading rather than a comment, and `# Note on setup` is a title,
    /// not a task. The colon is the one spelling that says "task" in any
    /// format.
    Colon,
}

/// The markers in one code entity's held source, attributed to `location`.
pub fn todos_in_text(content: &str, location: &str) -> Vec<ExtractedTodo> {
    scan(content, location, MarkerForm::Comment)
}

/// The markers in one unparsed entity's held text, attributed to `location`.
///
/// Stricter than [`todos_in_text`]: only `TODO:`, `FIXME:`, `HACK:` and
/// `NOTE:` count, so prose headings and bullets that merely start with one of
/// those words are not imported as work.
pub fn todos_in_unparsed_text(content: &str, location: &str) -> Vec<ExtractedTodo> {
    scan(content, location, MarkerForm::Colon)
}

fn scan(content: &str, location: &str, form: MarkerForm) -> Vec<ExtractedTodo> {
    let mut out = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();

        // Look for comment-style TODO markers.
        // Supports: // TODO, # TODO, /* TODO, * TODO, -- TODO
        let comment_body = if let Some(rest) = trimmed.strip_prefix("//") {
            Some(rest.trim())
        } else if let Some(rest) = trimmed.strip_prefix('#') {
            Some(rest.trim())
        } else if let Some(rest) = trimmed.strip_prefix("/*") {
            Some(rest.trim())
        } else if let Some(rest) = trimmed.strip_prefix('*') {
            Some(rest.trim())
        } else {
            trimmed.strip_prefix("--").map(|rest| rest.trim())
        };

        let Some(body) = comment_body else {
            continue;
        };
        let upper = body.to_uppercase();
        for marker in TODO_MARKERS {
            // Match "TODO:", "TODO(", "TODO ", "TODO -" patterns.
            let Some(after) = upper.strip_prefix(marker) else {
                continue;
            };
            let opens = match form {
                MarkerForm::Comment => {
                    after.is_empty()
                        || after.starts_with(':')
                        || after.starts_with('(')
                        || after.starts_with(' ')
                        || after.starts_with('-')
                }
                MarkerForm::Colon => after.starts_with(':'),
            };
            if !opens {
                continue;
            }
            // Extract the actual body text after the marker.
            let raw = &body[marker.len()..];
            let clean = raw
                .trim_start_matches(':')
                .trim_start_matches('(')
                .trim_end_matches(')')
                .trim_start_matches('-')
                .trim();

            if !clean.is_empty() {
                out.push(ExtractedTodo {
                    kind: marker.to_string(),
                    body: clean.to_string(),
                    file_path: location.to_string(),
                    line_number: line_idx + 1,
                });
            }
            break; // Only match first marker per line.
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_markers() {
        let todos = todos_in_text(
            "// TODO: implement error handling\nfn main() {}\n// FIXME: this is broken\n",
            "src/main.rs",
        );
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].kind, "TODO");
        assert!(todos[0].body.contains("implement error handling"));
        assert_eq!(todos[0].line_number, 1);
        assert_eq!(todos[0].file_path, "src/main.rs");
        assert_eq!(todos[1].kind, "FIXME");
        assert!(todos[1].body.contains("this is broken"));
        assert_eq!(todos[1].line_number, 3);
    }

    #[test]
    fn python_markers() {
        let todos = todos_in_text(
            "# TODO: add logging\ndef hello():\n    pass\n# HACK: temporary workaround\n",
            "app.py",
        );
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].kind, "TODO");
        assert_eq!(todos[1].kind, "HACK");
    }

    #[test]
    fn handles_various_marker_formats() {
        let todos = todos_in_text(
            concat!(
                "// TODO: colon style\n",
                "// TODO(user) paren style\n",
                "// TODO - dash style\n",
                "// NOTE: informational\n",
            ),
            "src/test.rs",
        );
        assert_eq!(todos.len(), 4);
        assert_eq!(todos[0].kind, "TODO");
        assert_eq!(todos[1].kind, "TODO");
        assert_eq!(todos[2].kind, "TODO");
        assert_eq!(todos[3].kind, "NOTE");
    }

    #[test]
    fn empty_text_has_no_markers() {
        assert!(todos_in_text("", "src/lib.rs").is_empty());
        assert!(todos_in_unparsed_text("", "README.md").is_empty());
    }

    /// Unparsed text takes only the colon form, so a heading or bullet that
    /// starts with a marker word is not a task, while a `TODO:` in a config
    /// comment or a prose bullet still is.
    #[test]
    fn unparsed_text_takes_only_the_colon_form() {
        let todos = todos_in_unparsed_text(
            concat!(
                "# Note on setup\n",
                "* Todo list for the release\n",
                "# TODO: pin the image digest\n",
                "- FIXME: the mirror is down\n",
                "* NOTE: keep this in sync with the CLI\n",
            ),
            "deploy.yml",
        );
        let found: Vec<(&str, &str)> = todos
            .iter()
            .map(|todo| (todo.kind.as_str(), todo.body.as_str()))
            .collect();
        assert_eq!(
            found,
            vec![
                ("TODO", "pin the image digest"),
                ("NOTE", "keep this in sync with the CLI"),
            ],
            "a `-` bullet is not a comment opener, and headings without a colon are titles"
        );
        // The same lines in code take the looser comment forms.
        assert_eq!(
            todos_in_text("# Note on setup\n", "setup.py").len(),
            1,
            "a `#` comment in code is a comment"
        );
    }
}
