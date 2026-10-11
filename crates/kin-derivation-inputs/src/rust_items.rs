// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Rust sources as a run of top-level items, so the tests in a file leave its
//! identity alone.
//!
//! Taken from `kin-lsp`'s producer identity, which reads its sources the same
//! way, unchanged, so the two agree on what a test is.

/// `source` without its top-level `#[cfg(test)]` items: test modules, and the
/// functions, types, statics and declarations only tests compile.
///
/// The file is read as a run of top-level items, each with the comments and
/// attributes written above it, and the items that carry an attribute spelled
/// exactly `#[cfg(test)]` are left out. The rest are joined by one line break
/// each, so neither a test nor the blank lines around one leaves a trace. An
/// item under `#[cfg(any(test, ...))]` also compiles outside tests, and one
/// nested inside other code is a seam in that code, so both stay.
///
/// The text is read as Rust tokens (comments, string and raw string literals,
/// character literals and lifetimes), never by indentation, so a brace inside
/// a test's string ends nothing.
///
/// An item whose end cannot be found is an error, never a silent cut.
pub fn without_tests(source: &str) -> Result<String, String> {
    let mut kept = String::with_capacity(source.len());
    let mut at = skip_whitespace(source, 0);
    while at < source.len() {
        let item = top_level_item(source, at).ok_or_else(|| {
            format!(
                "could not find where the item that starts on line {} ends",
                source[..at].lines().count() + 1
            )
        })?;
        if !item.test_only {
            if !kept.is_empty() {
                kept.push('\n');
            }
            kept.push_str(&source[at..item.end]);
        }
        at = skip_whitespace(source, item.end);
    }
    Ok(kept)
}

/// One top-level item, from the first comment or attribute above it.
struct Item {
    /// Just past its last token.
    end: usize,
    /// Whether an attribute on it is exactly `#[cfg(test)]`.
    test_only: bool,
}

/// The item that starts at `at`, or `None` when it never ends.
///
/// An item ends after the `;` that ends it, or after the brace that closes
/// its body and a `;` right behind that. An inner attribute (`#![...]`) is an
/// item of its own. Comments with nothing after them are a last item.
fn top_level_item(source: &str, at: usize) -> Option<Item> {
    let bytes = source.as_bytes();
    let mut cursor = at;
    let mut test_only = false;
    // The comments and outer attributes above the item.
    loop {
        let token = skip_blank(source, cursor);
        if token >= bytes.len() {
            return Some(Item {
                end: bytes.len(),
                test_only,
            });
        }
        if bytes[token] != b'#' {
            cursor = token;
            break;
        }
        let opening = skip_blank(source, token + 1);
        let inner = bytes.get(opening) == Some(&b'!');
        let bracket = if inner {
            skip_blank(source, opening + 1)
        } else {
            opening
        };
        if bytes.get(bracket) != Some(&b'[') {
            cursor = token;
            break;
        }
        let (end, spelled) = attribute(source, token)?;
        if inner {
            // It configures the file, not the item below it.
            return Some(Item {
                end,
                test_only: false,
            });
        }
        test_only |= spelled == "#[cfg(test)]";
        cursor = end;
    }
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    loop {
        cursor = skip_blank(source, cursor);
        let byte = *bytes.get(cursor)?;
        let next = skip_token(source, cursor);
        match byte {
            b'(' => parens += 1,
            b')' => parens = parens.checked_sub(1)?,
            b'[' => brackets += 1,
            b']' => brackets = brackets.checked_sub(1)?,
            b'{' => braces += 1,
            b'}' => {
                braces = braces.checked_sub(1)?;
                if braces == 0 && parens == 0 && brackets == 0 {
                    let behind = skip_blank(source, next);
                    let end = if bytes.get(behind) == Some(&b';') {
                        behind + 1
                    } else {
                        next
                    };
                    return Some(Item { end, test_only });
                }
            }
            b';' if braces == 0 && parens == 0 && brackets == 0 => {
                return Some(Item {
                    end: next,
                    test_only,
                });
            }
            _ => {}
        }
        cursor = next;
    }
}

/// The attribute that starts at the `#` at `at`: where it ends, and its text
/// without whitespace or comments.
pub(crate) fn attribute(source: &str, at: usize) -> Option<(usize, String)> {
    let bytes = source.as_bytes();
    let mut spelled = String::new();
    let mut cursor = at;
    let mut brackets = 0usize;
    loop {
        cursor = skip_blank(source, cursor);
        let byte = *bytes.get(cursor)?;
        let next = skip_token(source, cursor);
        spelled.push_str(&source[cursor..next]);
        match byte {
            b'[' => brackets += 1,
            b']' => {
                brackets = brackets.checked_sub(1)?;
                if brackets == 0 {
                    return Some((next, spelled));
                }
            }
            _ => {}
        }
        cursor = next;
    }
}

/// Past any whitespace at `at`.
fn skip_whitespace(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

/// Past any whitespace and comments at `at`.
pub(crate) fn skip_blank(source: &str, mut at: usize) -> usize {
    let bytes = source.as_bytes();
    loop {
        match bytes.get(at) {
            Some(byte) if byte.is_ascii_whitespace() => at += 1,
            Some(b'/') if matches!(bytes.get(at + 1), Some(b'/' | b'*')) => {
                at = skip_token(source, at);
            }
            _ => return at,
        }
    }
}

/// Past the token that starts at `at`: a comment, a string or character
/// literal, an identifier, or one other character.
pub(crate) fn skip_token(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let rest = &source[at..];
    match bytes[at] {
        b'/' if rest.starts_with("//") => at + rest.find('\n').unwrap_or(rest.len()),
        b'/' if rest.starts_with("/*") => {
            let mut cursor = at + 2;
            let mut nesting = 1usize;
            while cursor < bytes.len() && nesting > 0 {
                if source[cursor..].starts_with("/*") {
                    nesting += 1;
                    cursor += 2;
                } else if source[cursor..].starts_with("*/") {
                    nesting -= 1;
                    cursor += 2;
                } else {
                    cursor += 1;
                }
            }
            cursor
        }
        b'"' => skip_string(source, at),
        b'\'' => skip_quote(source, at),
        byte if byte == b'_' || byte.is_ascii_alphabetic() => {
            let mut end = at + 1;
            while bytes
                .get(end)
                .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
            {
                end += 1;
            }
            let hashes = bytes[end..]
                .iter()
                .take_while(|byte| **byte == b'#')
                .count();
            match &source[at..end] {
                "r" | "br" | "cr" if bytes.get(end + hashes) == Some(&b'"') => {
                    skip_raw_string(source, end + hashes + 1, hashes)
                }
                "b" | "c" if bytes.get(end) == Some(&b'"') => skip_string(source, end),
                "b" if bytes.get(end) == Some(&b'\'') => skip_quote(source, end),
                _ => end,
            }
        }
        _ => at + rest.chars().next().map_or(1, char::len_utf8),
    }
}

/// Past the string literal whose opening quote is at `at`.
fn skip_string(source: &str, at: usize) -> usize {
    let bytes = source.as_bytes();
    let mut cursor = at + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'\\' => cursor += 2,
            b'"' => return cursor + 1,
            _ => cursor += 1,
        }
    }
    bytes.len()
}

/// Past the raw string whose text starts at `at`, closed by a quote and
/// `hashes` hash signs.
fn skip_raw_string(source: &str, at: usize, hashes: usize) -> usize {
    let closing = format!("\"{}", "#".repeat(hashes));
    source[at..]
        .find(&closing)
        .map_or(source.len(), |found| at + found + closing.len())
}

/// Past the character literal at `at`, or past the quote alone when it opens
/// a lifetime or a label.
fn skip_quote(source: &str, at: usize) -> usize {
    let mut rest = source[at + 1..].chars();
    match rest.next() {
        Some('\\') => {
            // An escape: the escaped character, then everything to the quote.
            let escaped = rest.next().map_or(0, char::len_utf8);
            let from = at + 2 + escaped;
            source[from.min(source.len())..]
                .find('\'')
                .map_or(source.len(), |found| from + found + 1)
        }
        Some(character) if rest.next() == Some('\'') => at + 1 + character.len_utf8() + 1,
        _ => at + 1,
    }
}
