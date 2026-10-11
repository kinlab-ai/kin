// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The expression a call or use site sits in, quoted as written.
//!
//! A site row names its caller and the site's line in that caller, and its
//! `callee` is the name the call spells. What the call was made on, and with
//! what, is what a reader weighs to judge whether the site can reach a focal,
//! and a row that leaves it out sends the reader to the caller's whole body
//! for one line. [`read_expression`] cuts it from the caller's body the answer
//! already holds, at the offset and length the site ledger or the edge
//! recorded: the receiver and the callee through the argument list, on one
//! line, elided in the middle past [`EXPR_MAX_CHARS`]. It never reads a file,
//! and a body the answer does not hold leaves the site without an expression
//! and says why.
//!
//! Which text is a comment or a string, and which names are an instance or a
//! builtin, is the language's to say. It comes from the caller's adapter as a
//! [`SiteSyntax`] record, and nothing here names a language.
//!
//! Every list that serves these carries a byte budget ([`cap_expressions`]):
//! the rows past it keep their address, drop the quoted text, and the list says
//! how many did and how to read them.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;

use kin_model::entity::{Entity, SourceSpan};
use kin_model::graph::GraphStore;
use kin_model::FocalEscape;
use kin_parser::site_syntax::SiteSyntax;
use kin_parser::value_origins::{read_value_origins, ScopeFacts, ValueOrigin};
use serde_json::{json, Value};

use crate::handlers::external_symbols::SiteText;

mod passed;
pub use passed::{passed_note, PassedValue};

/// The longest expression a row serves, in characters. Longer ones keep their
/// start, their callee and their end, with `…` where text was cut.
pub const EXPR_MAX_CHARS: usize = 100;

/// The longest receiver a row serves, in characters.
pub const RECEIVER_MAX_CHARS: usize = 60;

/// Bytes past a callee token scanned for the end of its argument list, past
/// which the expression runs to the end of the line the list opens on.
const ARGUMENTS_SCAN_MAX_BYTES: usize = 8 * 1024;

/// Bytes before a callee token scanned for its receiver.
const RECEIVER_SCAN_MAX_BYTES: usize = 1024;

/// What stands where text was cut.
const ELLIPSIS: char = '\u{2026}';

/// Characters an elided expression keeps after its last cut, at the least.
const TAIL_MIN_CHARS: usize = 16;

/// The added bytes a list of named leftovers may spend on quoted expressions.
pub const NAMED_EXPR_BYTES_MAX: usize = 2_000;

/// The added bytes a list of reference rows may spend on quoted expressions.
pub const REFERENCE_EXPR_BYTES_MAX: usize = 1_500;

/// The added bytes one listed escape residue may spend on quoted lines.
pub const RESIDUE_EXPR_BYTES_MAX: usize = 1_000;

/// The fields a site row carries for its expression, each counted against
/// its list's budget and all dropped together past it: the call as written,
/// what it is made on, what that receiver says about reaching the focal,
/// which link of the chain typing it the graph could not establish, and the
/// module its file imports it from.
pub const EXPR_FIELDS: [&str; 6] = [
    "expr",
    "receiver",
    "receiver_hint",
    "expr_unavailable",
    crate::receiver_binding::RECEIVER_BINDING_MISSING_KEY,
    RECEIVER_IMPORT_KEY,
];

/// The row field saying what a named site's receiver is imported from, when
/// its file's record binds that receiver by one import statement:
/// `{"module": "json", "outside_repository": true}`, the module as the
/// statement writes it and whether no file of the repository can supply it
/// (see [`kin_db::call_site_escape::ReceiverImports`]).
pub const RECEIVER_IMPORT_KEY: &str = "receiver_import";

/// The expression one site sits in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteExpression {
    /// The call or use as written, on one line and at most
    /// [`EXPR_MAX_CHARS`] long.
    pub expr: String,
    /// What the callee is reached through (`values` in
    /// `values.pop("lang_code", None)`), when the site names one.
    pub receiver: Option<String>,
}

/// The site syntax `entity`'s adapter states, or [`SiteSyntax::UNSTATED`].
pub fn syntax_of(entity: &Entity) -> SiteSyntax {
    kin_parser::site_syntax::site_syntax_for(entity.language).unwrap_or(SiteSyntax::UNSTATED)
}

fn is_identifier_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn char_before(text: &str, at: usize) -> Option<char> {
    text.get(..at)?.chars().next_back()
}

/// The expression around the site at `start..end` of `body`, the caller's
/// exact text. The site is either a callee token, which is widened left over
/// its receiver and right through its argument list, or an expression, which
/// is served as it stands. `None` when the range is not a site of `body`.
pub fn read_expression(
    body: &str,
    start: usize,
    end: usize,
    syntax: &SiteSyntax,
) -> Option<SiteExpression> {
    if start >= end
        || end > body.len()
        || !body.is_char_boundary(start)
        || !body.is_char_boundary(end)
    {
        return None;
    }
    let seed = &body[start..end];
    let callee = callee_token(body, start, end, syntax);
    let expr_start = if seed.chars().next().is_some_and(is_identifier_char) {
        receiver_start(body, start, syntax)
    } else {
        start
    };
    let expr_end = if seed.chars().next_back().is_some_and(is_identifier_char) {
        arguments_end(body, end, syntax).unwrap_or(end)
    } else {
        end
    };
    let raw = &body[expr_start..expr_end];
    let keep = callee.map(|(from, to)| (from - expr_start, to - expr_start));
    let expr = one_line(raw, keep, syntax, EXPR_MAX_CHARS);
    let receiver = callee.and_then(|(callee_start, _)| {
        let head = body[expr_start..callee_start].trim_end();
        let head = strip_member_operator(head)?.trim();
        (!head.is_empty()).then(|| one_line(head, None, syntax, RECEIVER_MAX_CHARS))
    });
    (!expr.is_empty()).then_some(SiteExpression { expr, receiver })
}

/// Whether the callee token at `token_start` of `body`, the caller's exact
/// text, is written on no value: no member operator comes before it, so the
/// call is of a bare name its scope binds and reads no member off an object.
pub fn written_bare(body: &str, token_start: usize) -> bool {
    body.get(..token_start)
        .is_some_and(|before| strip_member_operator(before.trim_end()).is_none())
}

/// The callee token of the site at `start..end` of `body`, the caller's exact
/// text, as a range of `body`: the token a call-site ledger keys the same
/// call by. The site is a callee token already, or a call expression, whose
/// callee is the identifier written last before its argument list. `None`
/// when the range is no site of `body` or the callee is itself an expression.
pub fn callee_token(
    body: &str,
    start: usize,
    end: usize,
    syntax: &SiteSyntax,
) -> Option<(usize, usize)> {
    if start >= end
        || end > body.len()
        || !body.is_char_boundary(start)
        || !body.is_char_boundary(end)
    {
        return None;
    }
    let seed = &body[start..end];
    if seed.chars().all(is_identifier_char) {
        return Some((start, end));
    }
    call_head(seed, syntax).map(|(from, to)| (start + from, start + to))
}

/// Where a trace step quotes the code at each of its `reference_lines`.
pub const TRACE_REFERENCE_SITES_KEY: &str = "reference_sites";

/// The added bytes one trace may spend on quoted site code. Past it, a site
/// keeps its line and drops its code.
pub const TRACE_SITE_EXPR_BYTES_MAX: usize = 4_000;

/// The last segment of a qualified name, the part a call site spells.
pub fn spelled_name(name: &str) -> &str {
    let after_dot = name.rsplit('.').next().unwrap_or(name);
    after_dot.rsplit("::").next().unwrap_or(after_dot)
}

/// Whether a trace step's sites sit in its parent's body rather than its own.
/// A callee is reached by a call its parent makes; a caller, referrer or
/// importer holds the site that reaches its parent.
pub fn trace_sites_in_parent(role: &str) -> bool {
    role == "callee"
}

/// The code at each of `lines`, 1-based lines of `holder`'s file, cut from
/// `holder`'s exact body read through `text`: one row per line, `{line,
/// expr}`, with the line trimmed and widened around where it spells `name`.
/// Every quoted line adds to `spent`, and a line that would take it past
/// [`TRACE_SITE_EXPR_BYTES_MAX`] keeps its number alone, as does a line
/// outside the body. `None` when the body cannot be read.
pub fn quote_lines<T: SiteText + ?Sized>(
    text: &T,
    holder: &Entity,
    name: &str,
    lines: &[u64],
    spent: &mut usize,
) -> Option<Vec<Value>> {
    let first_line = holder.span.as_ref()?.start_line as u64 + 1;
    let body = whole_text(text, holder).ok()?;
    let syntax = syntax_of(holder);
    Some(
        lines
            .iter()
            .map(|line| {
                let mut site = json!({ "line": line });
                let text = line
                    .checked_sub(first_line)
                    .and_then(|offset| body.lines().nth(offset as usize));
                if let Some(text) = text {
                    let expr = line_expression(text, Some(name), &syntax);
                    if !expr.is_empty() && *spent + expr.len() <= TRACE_SITE_EXPR_BYTES_MAX {
                        *spent += expr.len();
                        site["expr"] = json!(expr);
                    }
                }
                site
            })
            .collect(),
    )
}

/// `line` on its own, the way a listed residue site serves it: trimmed, its
/// comment dropped, elided around the first place it spells `name`.
pub fn line_expression(line: &str, name: Option<&str>, syntax: &SiteSyntax) -> String {
    let keep = name.and_then(|name| spelled_at(line, name));
    one_line(line, keep, syntax, EXPR_MAX_CHARS)
}

/// Where `line` first spells `name` as a whole identifier.
fn spelled_at(line: &str, name: &str) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    line.match_indices(name)
        .map(|(at, _)| at)
        .find(|at| {
            let end = at + name.len();
            !char_before(line, *at).is_some_and(is_identifier_char)
                && !line[end..].chars().next().is_some_and(is_identifier_char)
        })
        .map(|at| (at, at + name.len()))
}

/// The member operator `text` ends with removed, or `None` when it ends with
/// none: `?.`, `->`, `::` or a `.` that is not part of `..`.
fn strip_member_operator(text: &str) -> Option<&str> {
    for operator in ["?.", "->", "::"] {
        if let Some(head) = text.strip_suffix(operator) {
            return Some(head);
        }
    }
    let head = text.strip_suffix('.')?;
    (!head.ends_with('.')).then_some(head)
}

/// Where the receiver chain before the callee token at `token_start` starts,
/// or `token_start` when the token is reached through none.
fn receiver_start(body: &str, token_start: usize, syntax: &SiteSyntax) -> usize {
    let mut start = token_start;
    loop {
        // Only spaces and tabs sit between a member operator and the name
        // after it; a chain may break its line before the operator.
        let operator_end = body[..start].trim_end_matches([' ', '\t']).len();
        let Some(mut head) = strip_member_operator(&body[..operator_end]) else {
            break;
        };
        // A block comment may sit between the operator and its operand.
        while let Some((open, close)) = syntax.block_comment {
            let trimmed = head.trim_end();
            match trimmed
                .strip_suffix(close)
                .and_then(|inner| inner.rfind(open))
            {
                Some(comment) => head = &trimmed[..comment],
                None => break,
            }
        }
        let primary_end = head.trim_end().len();
        if head[primary_end..].contains('\n') {
            let line_start = head[..primary_end].rfind('\n').map_or(0, |at| at + 1);
            let previous = &head[line_start..primary_end];
            if syntax
                .line_comments
                .iter()
                .any(|token| previous.contains(token))
            {
                break;
            }
        }
        let Some(primary) = primary_start(body, primary_end, syntax) else {
            break;
        };
        if token_start - primary > RECEIVER_SCAN_MAX_BYTES {
            break;
        }
        start = primary;
    }
    // A constructor call keeps the keyword the language writes before it.
    if let Some(keyword) = syntax.constructor_keyword {
        let before = body[..start].trim_end_matches([' ', '\t']);
        if before.len() < start
            && before.ends_with(keyword)
            && !char_before(before, before.len() - keyword.len()).is_some_and(is_identifier_char)
        {
            return before.len() - keyword.len();
        }
    }
    start
}

/// Where the one operand that ends at `end` starts: a name, a call or an
/// index on one, a bracketed group or a string literal.
fn primary_start(body: &str, end: usize, syntax: &SiteSyntax) -> Option<usize> {
    let floor = end.saturating_sub(RECEIVER_SCAN_MAX_BYTES);
    let mut at = end;
    loop {
        let character = char_before(body, at)?;
        if is_identifier_char(character) {
            let start = body[..at]
                .char_indices()
                .rev()
                .take_while(|(_, character)| is_identifier_char(*character))
                .last()
                .map(|(offset, _)| offset)?;
            // A private member name, `#name`, after a member operator.
            let start = if body[..start].ends_with(".#") {
                start - 1
            } else {
                start
            };
            return Some(start);
        }
        match character {
            ')' | ']' => {
                at = group_open_before(body, at - 1, floor)?;
                // Generic arguments between a callee and its call belong to
                // the call.
                if character == ')' && char_before(body, at) == Some('>') {
                    if let Some(open) = generics_open_before(body, at - 1, floor) {
                        let open = if body[..open].ends_with("::") {
                            open - 2
                        } else {
                            open
                        };
                        if char_before(body, open).is_some_and(is_identifier_char) {
                            at = open;
                        }
                    }
                }
                // A call or an index on something before it continues the
                // operand.
                match char_before(body, at) {
                    Some(before) if is_identifier_char(before) || matches!(before, ')' | ']') => {
                        continue
                    }
                    _ => return Some(at),
                }
            }
            '}' => return group_open_before(body, at - 1, floor),
            quote if syntax.string_quotes.contains(&quote) => {
                let open = string_open_before(body, at - quote.len_utf8(), quote, floor)?;
                // A short prefix such as `f`, `b` or `r` is part of the literal.
                let prefix: usize = body[..open]
                    .chars()
                    .rev()
                    .take_while(char::is_ascii_alphabetic)
                    .count();
                return Some(if prefix <= 2 { open - prefix } else { open });
            }
            _ => return None,
        }
    }
}

/// The opening bracket matching the closing one at `close`, scanning back no
/// further than `floor`.
fn group_open_before(body: &str, close: usize, floor: usize) -> Option<usize> {
    let mut open: Vec<char> = Vec::new();
    for (at, character) in body[..=close].char_indices().rev() {
        if at < floor {
            return None;
        }
        match character {
            ')' | ']' | '}' => open.push(character),
            '(' | '[' | '{' => {
                let closing = open.pop()?;
                if !matches!((character, closing), ('(', ')') | ('[', ']') | ('{', '}')) {
                    return None;
                }
                if open.is_empty() {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The `<` matching the `>` at `close`, on the same line.
fn generics_open_before(body: &str, close: usize, floor: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, character) in body[..=close].char_indices().rev() {
        if at < floor {
            return None;
        }
        match character {
            '>' => depth += 1,
            '<' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(at);
                }
            }
            '\n' | ';' | '{' | '}' => return None,
            _ => {}
        }
    }
    None
}

/// The opening quote of the string literal whose closing `quote` is at
/// `close`.
fn string_open_before(body: &str, close: usize, quote: char, floor: usize) -> Option<usize> {
    body[..close]
        .char_indices()
        .rev()
        .take_while(|(at, _)| *at >= floor)
        .find(|(at, character)| *character == quote && !body[..*at].ends_with('\\'))
        .map(|(at, _)| at)
}

/// Where the string literal opening at `at` with `quote` ends, one past its
/// closing quote.
fn string_end(text: &str, at: usize, quote: char) -> Option<usize> {
    let from = at + quote.len_utf8();
    let mut escaped = false;
    for (offset, character) in text[from..].char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == quote {
            return Some(from + offset + character.len_utf8());
        }
    }
    None
}

/// Walk the code of `text` from `from`, skipping its strings and comments by
/// `syntax`, handing each other character to `visit` until it breaks, or
/// `limit` bytes pass, or the text ends.
fn scan_code<T>(
    text: &str,
    from: usize,
    syntax: &SiteSyntax,
    limit: usize,
    mut visit: impl FnMut(usize, char) -> ControlFlow<Option<T>>,
) -> Option<T> {
    let mut at = from;
    while at < text.len() {
        if at - from > limit {
            return None;
        }
        let rest = &text[at..];
        if syntax
            .line_comments
            .iter()
            .any(|token| rest.starts_with(token))
        {
            at += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if let Some((open, close)) = syntax.block_comment {
            if let Some(inner) = rest.strip_prefix(open) {
                at += open.len() + inner.find(close)? + close.len();
                continue;
            }
        }
        let character = rest.chars().next()?;
        if syntax.string_quotes.contains(&character) {
            at = string_end(text, at, character)?;
            continue;
        }
        if let ControlFlow::Break(found) = visit(at, character) {
            return found;
        }
        at += character.len_utf8();
    }
    None
}

/// Where the argument list after the callee token ending at `end` closes, one
/// past its `)`. An optional call's `?.`, a non-null `!` and generic
/// arguments may sit between the two. `None` when no argument list follows.
/// A list that does not close within the scan runs to the end of the line it
/// opens on.
fn arguments_end(body: &str, end: usize, syntax: &SiteSyntax) -> Option<usize> {
    let mut at = end + (body[end..].len() - body[end..].trim_start_matches([' ', '\t']).len());
    let rest = &body[at..];
    if rest.starts_with("?.(") {
        at += 2;
    } else if rest.starts_with("!(") {
        at += 1;
    } else if rest.starts_with("::<") || rest.starts_with('<') {
        let open = if rest.starts_with("::<") { at + 2 } else { at };
        let close = generics_end(body, open)?;
        at = close + (body[close..].len() - body[close..].trim_start_matches([' ', '\t']).len());
    }
    if !body[at..].starts_with('(') {
        return None;
    }
    let mut open: Vec<char> = Vec::new();
    let closed = scan_code(
        body,
        at,
        syntax,
        ARGUMENTS_SCAN_MAX_BYTES,
        |offset, character| match character {
            '(' | '[' | '{' => {
                open.push(character);
                ControlFlow::Continue(())
            }
            ')' | ']' | '}' => {
                let Some(opening) = open.pop() else {
                    return ControlFlow::Break(None);
                };
                if !matches!((opening, character), ('(', ')') | ('[', ']') | ('{', '}')) {
                    return ControlFlow::Break(None);
                }
                if open.is_empty() {
                    ControlFlow::Break(Some(offset + 1))
                } else {
                    ControlFlow::Continue(())
                }
            }
            _ => ControlFlow::Continue(()),
        },
    );
    Some(closed.unwrap_or_else(|| body[at..].find('\n').map_or(body.len(), |line| at + line)))
}

/// One past the `>` closing the generic argument list that opens at `open`,
/// when it closes on its line.
fn generics_end(body: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, character) in body[open..].char_indices().take(200) {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset + 1);
                }
            }
            '\n' | ';' | '{' | '}' => return None,
            _ => {}
        }
    }
    None
}

/// The callee token of an expression that ends with a call's argument list,
/// or is a member access, as a range of `expression`: the identifier written
/// last before the list. `None` when the callee is itself an expression.
fn call_head(expression: &str, syntax: &SiteSyntax) -> Option<(usize, usize)> {
    let trimmed = expression.trim_end();
    let mut head = if trimmed.ends_with(')') {
        let close = trimmed.len() - 1;
        let mut depth = 0usize;
        let mut last_open = None;
        let found = scan_code(trimmed, 0, syntax, usize::MAX, |offset, character| {
            match character {
                '(' | '[' | '{' => {
                    if depth == 0 {
                        last_open = Some(offset);
                    }
                    depth += 1;
                }
                ')' | ']' | '}' => {
                    let Some(next) = depth.checked_sub(1) else {
                        return ControlFlow::Break(None);
                    };
                    depth = next;
                    if offset == close {
                        return ControlFlow::Break(last_open.filter(|_| depth == 0));
                    }
                }
                _ => {}
            }
            ControlFlow::Continue(())
        })?;
        trimmed[..found].trim_end()
    } else {
        trimmed
    };
    if let Some(stripped) = head.strip_suffix("?.") {
        head = stripped.trim_end();
    } else if let Some(stripped) = head.strip_suffix('!') {
        head = stripped.trim_end();
    }
    if head.ends_with('>') {
        let mut depth = 0usize;
        let open = head.char_indices().rev().find_map(|(offset, character)| {
            match character {
                '>' => depth += 1,
                '<' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(offset);
                    }
                }
                _ => {}
            }
            None
        })?;
        head = head[..open].trim_end();
        head = head.strip_suffix("::").unwrap_or(head);
    }
    let start = head
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_identifier_char(*character))
        .last()
        .map(|(offset, _)| offset)?;
    let first = head[start..].chars().next()?;
    (first.is_alphabetic() || first == '_' || first == '$').then_some((start, head.len()))
}

/// `raw` on one line: comments dropped, each line break with its indentation
/// read as one space (or none beside a bracket or before a member operator),
/// then elided to `max` characters keeping the range `keep` of `raw`.
fn one_line(raw: &str, keep: Option<(usize, usize)>, syntax: &SiteSyntax, max: usize) -> String {
    let mut out: Vec<(char, usize)> = Vec::new();
    let push_break = |out: &mut Vec<(char, usize)>, at: usize, next: Option<char>| {
        while out
            .last()
            .is_some_and(|(character, _)| *character == ' ' || *character == '\t')
        {
            out.pop();
        }
        let previous = out.last().map(|(character, _)| *character);
        let tight = previous.is_none_or(|previous| matches!(previous, '(' | '[' | '{'))
            || next.is_none_or(|next| matches!(next, ')' | ']' | '}' | '.'));
        if !tight {
            out.push((' ', at));
        }
    };
    let mut at = 0usize;
    while at < raw.len() {
        let rest = &raw[at..];
        if syntax
            .line_comments
            .iter()
            .any(|token| rest.starts_with(token))
        {
            while out
                .last()
                .is_some_and(|(character, _)| *character == ' ' || *character == '\t')
            {
                out.pop();
            }
            at += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if let Some((open, close)) = syntax.block_comment {
            if let Some(inner) = rest.strip_prefix(open) {
                at += inner
                    .find(close)
                    .map_or(rest.len(), |offset| open.len() + offset + close.len());
                let next = raw[at..]
                    .chars()
                    .find(|character| !character.is_whitespace());
                push_break(&mut out, at, next);
                continue;
            }
        }
        let Some(character) = rest.chars().next() else {
            break;
        };
        if character.is_whitespace() {
            let run: usize = rest
                .chars()
                .take_while(|character| character.is_whitespace())
                .map(char::len_utf8)
                .sum();
            if rest[..run].contains('\n') {
                let next = raw[at + run..].chars().next();
                push_break(&mut out, at, next);
            } else {
                out.extend(
                    rest[..run]
                        .char_indices()
                        .map(|(offset, c)| (c, at + offset)),
                );
            }
            at += run;
            continue;
        }
        if syntax.string_quotes.contains(&character) {
            let end = string_end(raw, at, character).unwrap_or(raw.len());
            let mut in_break = false;
            for (offset, character) in raw[at..end].char_indices() {
                if character == '\n' || (in_break && character.is_whitespace()) {
                    if !in_break {
                        out.push((' ', at + offset));
                    }
                    in_break = true;
                    continue;
                }
                in_break = false;
                out.push((character, at + offset));
            }
            at = end;
            continue;
        }
        out.push((character, at));
        at += character.len_utf8();
    }
    while out
        .last()
        .is_some_and(|(character, _)| character.is_whitespace())
    {
        out.pop();
    }
    let leading = out
        .iter()
        .take_while(|(character, _)| character.is_whitespace())
        .count();
    out.drain(..leading);
    let keep = keep.and_then(|(from, to)| {
        let first = out.iter().position(|(_, at)| *at >= from)?;
        let last = out.iter().rposition(|(_, at)| *at < to)? + 1;
        (first < last).then_some((first, last))
    });
    let characters: Vec<char> = out.into_iter().map(|(character, _)| character).collect();
    elide(&characters, keep, max)
}

/// `text` cut to `max` characters with `…` where it was cut, keeping its start,
/// its end and the range `keep` (the callee, when there is one).
fn elide(text: &[char], keep: Option<(usize, usize)>, max: usize) -> String {
    let length = text.len();
    if length <= max {
        return text.iter().collect();
    }
    let (from, to) = keep.unwrap_or((0, 0));
    let joined = |pieces: &[&[char]]| {
        let mut out = String::new();
        for (index, piece) in pieces.iter().enumerate() {
            if index > 0 {
                out.push(ELLIPSIS);
            }
            out.extend(piece.iter());
        }
        out
    };
    // One cut, after the callee, when the callee sits early enough.
    let head = to.max(max * 3 / 5);
    if head + 1 + TAIL_MIN_CHARS <= max {
        let tail = max - head - 1;
        return joined(&[&text[..head], &text[length - tail..]]);
    }
    // One cut after a short start, when the callee sits near the end.
    let lead = max / 4;
    let rest = max - lead - 1;
    if from >= length - rest {
        return joined(&[&text[..lead], &text[length - rest..]]);
    }
    // Two cuts: the start, the callee with what leads into it and a little of
    // what follows, and the end.
    let tail = max / 4;
    let middle = max - lead - tail - 2;
    let window_end = (to + middle.saturating_sub(to - from).min(12)).min(length - tail);
    let window_start = window_end.saturating_sub(middle).max(lead);
    joined(&[
        &text[..lead],
        &text[window_start..window_end],
        &text[length - tail..],
    ])
}

// ── Whether a receiver can hold the focal's owner ─────────────────────────

/// What a site's receiver says about whether the call can reach the focal,
/// read from its text and from what the caller's own scope records. Only an
/// ordering hint: no reading here proves or rules out a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverHint {
    /// The receiver's text names the focal's owner.
    NamesOwner,
    /// The caller's scope records the receiver as holding the focal's owner:
    /// the instance a member of the owner runs on, or a binding declared or
    /// constructed as the owner.
    TypedAsOwner,
    /// Nothing says either way.
    Unknown,
    /// The receiver is a literal.
    Literal,
    /// The receiver is a global the language binds, or a binding declared
    /// or constructed as one.
    Builtin,
}

impl ReceiverHint {
    /// Where a site with this hint is listed: most likely to reach the focal
    /// first.
    pub fn rank(self) -> u8 {
        match self {
            Self::NamesOwner | Self::TypedAsOwner => 0,
            Self::Unknown => 1,
            Self::Literal | Self::Builtin => 2,
        }
    }

    /// The rank of the hint a row serves as `wire`, where no word is
    /// [`Self::Unknown`].
    pub fn rank_of(wire: Option<&str>) -> u8 {
        [
            Self::NamesOwner,
            Self::TypedAsOwner,
            Self::Literal,
            Self::Builtin,
        ]
        .into_iter()
        .find(|hint| hint.wire() == wire)
        .unwrap_or(Self::Unknown)
        .rank()
    }

    /// The word a row serves, or `None` for [`Self::Unknown`], which a row
    /// leaves out.
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Self::NamesOwner => Some("names_owner"),
            Self::TypedAsOwner => Some("typed_as_owner"),
            Self::Unknown => None,
            Self::Literal => Some("literal"),
            Self::Builtin => Some("builtin"),
        }
    }
}

/// The names a receiver of the focal's owner is spelled with.
#[derive(Debug, Clone, Default)]
pub struct FocalOwner {
    /// The owner's own name, for a member: `AppContext` for `AppContext.pop`.
    owner: Option<String>,
    /// Every spelling that names it: the owner's name and its lowercase,
    /// snake case and camel case forms, or for a function its file's stem.
    names: Vec<String>,
}

impl FocalOwner {
    /// Whether the focal is a member of an owner, reached through a value of
    /// it, rather than a declaration of its module.
    pub fn is_member(&self) -> bool {
        self.owner.is_some()
    }

    /// The owner `focal` is declared in.
    pub fn of(focal: &Entity) -> Self {
        let segments: Vec<&str> = focal
            .name
            .split(['.', ':'])
            .filter(|segment| !segment.is_empty())
            .collect();
        let owner = segments
            .len()
            .checked_sub(2)
            .map(|at| segments[at])
            .filter(|owner| owner.chars().next().is_some_and(is_identifier_char));
        let mut names = Vec::new();
        match owner {
            Some(owner) => {
                names.push(owner.to_string());
                names.push(owner.to_lowercase());
                names.push(snake_case(owner));
                let mut camel = owner.to_string();
                if let Some(first) = camel.get(..1) {
                    camel.replace_range(..1, &first.to_lowercase());
                }
                names.push(camel);
            }
            None => {
                let stem = focal
                    .file_origin
                    .as_ref()
                    .and_then(|path| path.0.rsplit('/').next())
                    .and_then(|file| file.split('.').next())
                    .filter(|stem| !stem.is_empty() && stem.chars().all(is_identifier_char));
                if let Some(stem) = stem {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        Self {
            owner: owner.map(str::to_string),
            names,
        }
    }

    /// Whether `caller` is declared directly in the owner.
    fn declares(&self, caller: &Entity) -> bool {
        let Some(owner) = self.owner.as_deref() else {
            return false;
        };
        let segments: Vec<&str> = caller
            .name
            .split(['.', ':'])
            .filter(|segment| !segment.is_empty())
            .collect();
        segments.len() >= 2 && segments[segments.len() - 2] == owner
    }
}

fn snake_case(name: &str) -> String {
    let mut out = String::new();
    let characters: Vec<char> = name.chars().collect();
    for (index, character) in characters.iter().enumerate() {
        if character.is_uppercase() {
            let after_lower = index > 0 && !characters[index - 1].is_uppercase();
            let before_lower = characters
                .get(index + 1)
                .is_some_and(|next| next.is_lowercase());
            if index > 0 && (after_lower || (before_lower && characters[index - 1] != '_')) {
                out.push('_');
            }
            out.extend(character.to_lowercase());
        } else {
            out.push(*character);
        }
    }
    out
}

/// The identifiers a receiver is written with outside its brackets, in order:
/// `flask`, `g`, `seen` for `flask.g.seen`.
fn receiver_segments(receiver: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut start: Option<usize> = None;
    for (at, character) in receiver.char_indices() {
        let at_top = depth == 0;
        match character {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if at_top && is_identifier_char(character) {
            start.get_or_insert(at);
            continue;
        }
        if let Some(from) = start.take() {
            segments.push(&receiver[from..at]);
        }
    }
    if let Some(from) = start {
        segments.push(&receiver[from..]);
    }
    segments
}

/// The type a binding's recorded origin names, following bindings of the same
/// scope: a declared type, or the name a constructor call spells.
fn origin_type<'f>(origin: &'f ValueOrigin, facts: &'f ScopeFacts, depth: u8) -> Option<&'f str> {
    let leaf = |name: &'f str| name.rsplit(['.', ':']).next().unwrap_or(name);
    match origin {
        ValueOrigin::Declared { type_name } => Some(leaf(type_name)),
        ValueOrigin::Call { callee } | ValueOrigin::ImportedCall { callee, .. } => {
            Some(leaf(callee))
        }
        ValueOrigin::Local { name } if depth > 0 => {
            origin_type(facts.bindings.get(name)?, facts, depth - 1)
        }
        _ => None,
    }
}

/// What `receiver`, written in `caller`, says about reaching `owner`.
pub fn receiver_hint(
    receiver: &str,
    caller: &Entity,
    owner: &FocalOwner,
    syntax: &SiteSyntax,
) -> ReceiverHint {
    let receiver = receiver.trim();
    let Some(first) = receiver.chars().next() else {
        return ReceiverHint::Unknown;
    };
    let quoted_after_prefix = receiver
        .char_indices()
        .find(|(_, character)| !character.is_ascii_alphabetic())
        .is_some_and(|(at, character)| at <= 2 && syntax.string_quotes.contains(&character));
    if first.is_ascii_digit()
        || matches!(first, '[' | '{')
        || syntax.string_quotes.contains(&first)
        || quoted_after_prefix
    {
        return ReceiverHint::Literal;
    }
    let segments = receiver_segments(receiver);
    let (Some(root), Some(last)) = (segments.first(), segments.last()) else {
        return ReceiverHint::Unknown;
    };
    if owner.names.iter().any(|name| name.as_str() == *last) {
        return ReceiverHint::NamesOwner;
    }
    let facts = read_value_origins(caller);
    let bound = facts
        .as_ref()
        .and_then(|facts| facts.bindings.get(*root).map(|origin| (origin, facts)));
    if receiver == *root {
        if syntax.instance_receivers.iter().any(|name| name == root) && owner.declares(caller) {
            return ReceiverHint::TypedAsOwner;
        }
        if let Some(named) = bound.and_then(|(origin, facts)| origin_type(origin, facts, 4)) {
            if owner.owner.as_deref() == Some(named) {
                return ReceiverHint::TypedAsOwner;
            }
            if syntax.builtin_receivers.iter().any(|name| *name == named) {
                return ReceiverHint::Builtin;
            }
        }
    }
    if bound.is_none() && syntax.builtin_receivers.iter().any(|name| name == root) {
        return ReceiverHint::Builtin;
    }
    ReceiverHint::Unknown
}

// ── Budgets ───────────────────────────────────────────────────────────────

/// What one list's expression budget served and cut.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExprCut {
    /// Sites that kept their expression fields.
    pub served: usize,
    /// Sites past the budget that dropped them.
    pub elided: usize,
    /// Bytes the served fields add to the reply.
    pub bytes: usize,
}

impl ExprCut {
    /// The disclosure a list carries beside itself when its budget cut, or
    /// `None` when it cut nothing. `unit` names what the list holds.
    pub fn disclosure(&self, budget: usize, unit: &str) -> Option<Value> {
        (self.elided > 0).then(|| {
            json!({
                "elided": self.elided,
                "served": self.served,
                "budget_bytes": budget,
                "detail": format!(
                    "the first {} {unit} carry `expr`, the site as written, within a {budget}-byte \
                     budget; the other {} keep their caller and `line_in_entity`, and \
                     get_entity_source on that caller shows the site",
                    self.served, self.elided
                ),
            })
        })
    }
}

/// The bytes `site`'s expression fields add to a serialized reply.
fn added_bytes(site: &serde_json::Map<String, Value>) -> usize {
    EXPR_FIELDS
        .iter()
        .filter_map(|key| {
            site.get(*key)
                .map(|value| key.len() + 4 + value.to_string().len())
        })
        .sum()
}

/// Keep the expression fields of `sites`, in order, while they fit `budget`
/// bytes, and drop them from every site after the first that does not, so the
/// served ones are always a prefix of the list.
pub fn cap_expressions<'v>(
    sites: impl IntoIterator<Item = &'v mut Value>,
    budget: usize,
) -> ExprCut {
    let mut cut = ExprCut::default();
    let mut open = true;
    for site in sites {
        let Some(site) = site.as_object_mut() else {
            continue;
        };
        let added = added_bytes(site);
        if added == 0 {
            continue;
        }
        if open && cut.bytes + added <= budget {
            cut.bytes += added;
            cut.served += 1;
        } else {
            open = false;
            for key in EXPR_FIELDS {
                site.remove(key);
            }
            cut.elided += 1;
        }
    }
    cut
}

/// Cap the site expressions of reference `rows` together, in row order, and
/// return what the budget did.
pub fn cap_reference_rows(rows: &mut [Value], budget: usize) -> ExprCut {
    cap_expressions(
        rows.iter_mut()
            .filter_map(|row| row.get_mut("sites").and_then(Value::as_array_mut))
            .flatten(),
        budget,
    )
}

/// Put the fields of `expression` on a site row, or say why there is none.
pub fn put_expression(row: &mut Value, expression: Result<&SiteExpression, &'static str>) {
    match expression {
        Ok(expression) => {
            row["expr"] = json!(expression.expr);
            if let Some(receiver) = &expression.receiver {
                row["receiver"] = json!(receiver);
            }
        }
        Err(reason) => row["expr_unavailable"] = json!(reason),
    }
}

// ── Listed escape residue ─────────────────────────────────────────────────

/// The residue lists whose sites are served, each a list of sites.
const RESIDUE_SITE_LISTS: [&str; 5] = [
    "alias_sites",
    "values",
    "bindings",
    "implicit_calls",
    "reflective",
];

/// The kind a census gives a value use that is an argument of a call.
pub const PASSED_SITE_KIND: &str = "argument";

/// Where a listed value site says which call it is handed to, when that can
/// be read: the callee as written, the value's place among the arguments, the
/// repository declaration the call is proven to reach, and the parameter that
/// declaration calls it through.
pub const PASSED_TO_KEY: &str = "passed_to";

/// One listed residue site's line, and the entity a reader checks it in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidueLine {
    /// The line as written, or why it could not be read.
    pub text: Result<String, &'static str>,
    /// The innermost declaration the graph holds over the line, by id and
    /// name, when it is one `source` can read: a site at a file's top level
    /// sits in no such declaration, and names none.
    pub inside: Option<(kin_model::EntityId, String)>,
    /// For a site at a module's top level, which sits in no declaration
    /// `source` can read, the module that holds it, by id and name: a reader
    /// can still place the site, and outline the module with `context`.
    pub in_module: Option<(kin_model::EntityId, String)>,
    /// For a value handed to a call as an argument, the call it is handed
    /// to and what the answer can say about it.
    pub passed: Option<PassedValue>,
}

/// The line text of each residue site `escape` lists within `sample` per
/// list, keyed by file and 1-based line: the line as written, read from the
/// body of the innermost entity the graph holds over it, or why it could not
/// be, with that entity when a reader can read it. Each file's entities are
/// looked up once and each entity's body is read once, through `text`.
pub fn residue_lines<G: GraphStore + ?Sized, T: SiteText + ?Sized>(
    store: &G,
    text: &T,
    escape: &FocalEscape,
    sample: usize,
) -> BTreeMap<(String, u32), ResidueLine> {
    let mut lines = BTreeMap::new();
    let Some(residue) = escape.residue() else {
        return lines;
    };
    let listed = [
        &residue.alias_sites,
        &residue.values,
        &residue.bindings,
        &residue.implicit_calls,
        &residue.reflective,
        &residue.mentions.unmatched,
    ];
    let protocol = residue.protocol.as_ref().map(|protocol| &protocol.sites);
    // The value uses a census records as arguments of a call.
    let arguments: std::collections::HashSet<(&str, u32)> = residue
        .values
        .iter()
        .take(sample)
        .filter(|site| site.kind == PASSED_SITE_KIND)
        .map(|site| (site.file.as_str(), site.line))
        .collect();
    let mut by_file: BTreeMap<&str, Vec<(u32, Option<&str>)>> = BTreeMap::new();
    for sites in listed.into_iter().chain(protocol) {
        for site in sites.iter().take(sample) {
            by_file
                .entry(site.file.as_str())
                .or_default()
                .push((site.line, site.name.as_deref()));
        }
    }
    let mut bodies: HashMap<kin_model::EntityId, Result<String, &'static str>> = HashMap::new();
    for (file, sites) in by_file {
        let entities = store
            .query_entities(&kin_model::graph::EntityFilter {
                file_path: Some(kin_model::FilePathId::new(file)),
                ..Default::default()
            })
            .unwrap_or_default();
        for (line, name) in sites {
            let key = (file.to_string(), line);
            if lines.contains_key(&key) {
                continue;
            }
            let Some(graph_line) = line.checked_sub(1) else {
                lines.insert(
                    key,
                    ResidueLine {
                        text: Err("line_not_addressed"),
                        inside: None,
                        in_module: None,
                        passed: None,
                    },
                );
                continue;
            };
            let holder = entities
                .iter()
                .filter(|entity| {
                    entity.span.as_ref().is_some_and(|span| {
                        span.start_byte < span.end_byte
                            && span.start_line <= graph_line
                            && graph_line <= span.end_line
                    })
                })
                .min_by_key(|entity| {
                    entity
                        .span
                        .as_ref()
                        .map_or(usize::MAX, |span| span.end_byte - span.start_byte)
                });
            let Some(holder) = holder else {
                lines.insert(
                    key,
                    ResidueLine {
                        text: Err("no_entity_holds_line"),
                        inside: None,
                        in_module: None,
                        passed: None,
                    },
                );
                continue;
            };
            let inside = kin_model::require_independent_source(holder)
                .is_ok()
                .then(|| (holder.id, holder.name.clone()));
            let in_module = (inside.is_none() && holder.kind == kin_model::EntityKind::Module)
                .then(|| (holder.id, holder.name.clone()));
            let body = bodies
                .entry(holder.id)
                .or_insert_with(|| whole_text(text, holder));
            let start_line = holder.span.as_ref().map_or(0, |span| span.start_line);
            let line_text = match body {
                Ok(body) => body
                    .split('\n')
                    .nth((graph_line - start_line) as usize)
                    .map(|found| line_expression(found, name, &syntax_of(holder)))
                    .filter(|found| !found.is_empty())
                    .ok_or("line_outside_entity"),
                Err(reason) => Err(*reason),
            };
            // A value handed to a call: which call, read from the same body.
            let passed = match (body, name, arguments.contains(&(file, line))) {
                (Ok(body), Some(name), true) => {
                    let mut offset = 0;
                    body.split('\n')
                        .enumerate()
                        .find_map(|(index, found)| {
                            let at = offset;
                            offset += found.len() + 1;
                            (index == (graph_line - start_line) as usize).then_some((at, found))
                        })
                        .and_then(|(at, found)| {
                            let (start, end) = spelled_at(found, name)?;
                            passed::read(store, text, holder, body, (at + start, at + end))
                        })
                }
                _ => None,
            };
            lines.insert(
                key,
                ResidueLine {
                    text: line_text,
                    inside,
                    in_module,
                    passed,
                },
            );
        }
    }
    lines
}

/// `entity`'s exact text, read through `text`, or why it cannot be.
pub fn whole_text<T: SiteText + ?Sized>(text: &T, entity: &Entity) -> Result<String, &'static str> {
    let span = entity.span.as_ref().ok_or("caller_has_no_span")?;
    text.quote(
        entity,
        &SourceSpan {
            file: span.file.clone(),
            start_byte: span.start_byte,
            end_byte: span.end_byte,
            start_line: 0,
            start_col: 0,
            end_line: 0,
            end_col: 0,
        },
    )
}

/// Put each residue site's line from `lines` on the listed residue in
/// `summary` (a [`FocalEscape::summary_json`]), as `expr`, or say why it could
/// not be read, within [`RESIDUE_EXPR_BYTES_MAX`]. A reflective site whose
/// excerpt already is the whole line keeps the excerpt alone. A site inside a
/// declaration a reader can read also names it, as `inside` and `inside_id`,
/// so checking the site is one `source` call.
pub fn annotate_residue(summary: &mut Value, lines: &BTreeMap<(String, u32), ResidueLine>) {
    let Some(residue) = summary.get_mut("residue").and_then(Value::as_object_mut) else {
        return;
    };
    let mut sites: Vec<&mut Value> = Vec::new();
    let mut protocol = None;
    let mut mentions = None;
    for (key, list) in residue.iter_mut() {
        if RESIDUE_SITE_LISTS.contains(&key.as_str()) {
            if let Some(listed) = list.get_mut("sites").and_then(Value::as_array_mut) {
                sites.extend(listed.iter_mut());
            }
        } else if key == "protocol" {
            protocol = Some(list);
        } else if key == kin_model::call_site_reading::MENTIONS_KEY {
            mentions = Some(list);
        }
    }
    if let Some(listed) = protocol
        .and_then(|protocol| protocol.pointer_mut("/sites/sites"))
        .and_then(Value::as_array_mut)
    {
        sites.extend(listed.iter_mut());
    }
    // The non-call reads no stored row carries, each a site to read.
    if let Some(listed) = mentions
        .and_then(|mentions| mentions.pointer_mut("/unmatched/sites"))
        .and_then(Value::as_array_mut)
    {
        sites.extend(listed.iter_mut());
    }
    for site in sites.iter_mut() {
        let (Some(file), Some(line)) = (site["file"].as_str(), site["line"].as_u64()) else {
            continue;
        };
        let Some(read) = lines.get(&(file.to_string(), line as u32)) else {
            continue;
        };
        if let Some((id, name)) = &read.inside {
            site["inside"] = json!(name);
            site["inside_id"] = json!(id.to_string());
        } else if let Some((id, name)) = &read.in_module {
            site["in_module"] = json!(name);
            site["in_module_id"] = json!(id.to_string());
        }
        match &read.text {
            Ok(expr) if site["detail"].as_str() != Some(expr.as_str()) => {
                site["expr"] = json!(expr);
            }
            Ok(_) => {}
            Err(reason) => site["expr_unavailable"] = json!(reason),
        }
        if let Some(passed) = read
            .passed
            .as_ref()
            .filter(|_| site["kind"].as_str() == Some(PASSED_SITE_KIND))
        {
            let mut to = json!({"callee": passed.callee});
            if let Some(position) = passed.position {
                to["position"] = json!(position);
            }
            if let Some((id, name)) = &passed.target {
                to["target_id"] = json!(id.to_string());
                to["target"] = json!(name);
            }
            if let Some(parameter) = &passed.called_as {
                to["calls_parameter"] = json!(parameter);
            }
            site[PASSED_TO_KEY] = to;
        }
    }
    let cut = cap_expressions(sites, RESIDUE_EXPR_BYTES_MAX);
    if let Some(disclosure) = cut.disclosure(RESIDUE_EXPR_BYTES_MAX, "listed residue sites") {
        residue.insert("expr_elided".to_string(), disclosure);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kin_parser::site_syntax::{ECMASCRIPT, PYTHON};

    /// The expression around the first place `body` spells `token` as a
    /// whole identifier, with the token as its callee.
    fn at(body: &str, token: &str, syntax: &SiteSyntax) -> SiteExpression {
        let (start, end) = spelled_at(body, token).expect("the body spells the token");
        read_expression(body, start, end, syntax).expect("an expression")
    }

    #[test]
    fn a_python_call_reads_as_its_receiver_callee_and_arguments() {
        let body = "def pull_lang_code(endpoint, values):\n    \
                    flask.g.lang_code = values.pop(\"lang_code\", None)\n";
        let site = at(body, "pop", &PYTHON);
        assert_eq!(site.expr, "values.pop(\"lang_code\", None)");
        assert_eq!(site.receiver.as_deref(), Some("values"));

        let body = "def app_teardown1(exc):\n    assert flask.g.seen.pop() == \"app_1\"\n";
        let site = at(body, "pop", &PYTHON);
        assert_eq!(site.expr, "flask.g.seen.pop()");
        assert_eq!(site.receiver.as_deref(), Some("flask.g.seen"));
    }

    #[test]
    fn a_bare_call_has_no_receiver() {
        let site = at("def run():\n    pop(1)\n", "pop", &PYTHON);
        assert_eq!(site.expr, "pop(1)");
        assert_eq!(site.receiver, None);
    }

    #[test]
    fn a_js_call_through_calls_indexes_and_optional_chains_reads_whole() {
        let body = "function run() {\n  const x = stack[depth - 1].items?.pop(`a)`, b);\n}\n";
        let site = at(body, "pop", &ECMASCRIPT);
        assert_eq!(site.expr, "stack[depth - 1].items?.pop(`a)`, b)");
        assert_eq!(site.receiver.as_deref(), Some("stack[depth - 1].items"));

        let body = "function run() {\n  return makeStore<number>(1).get().pop<T>(x);\n}\n";
        let site = at(body, "pop", &ECMASCRIPT);
        assert_eq!(site.expr, "makeStore<number>(1).get().pop<T>(x)");
        assert_eq!(site.receiver.as_deref(), Some("makeStore<number>(1).get()"));
    }

    #[test]
    fn a_multi_line_call_reads_on_one_line_without_its_comments() {
        let body =
            "def run(ctx):\n    result = ctx.pop(\n        first,  # the head (kept)\n        \
                    second,\n    )\n";
        let site = at(body, "pop", &PYTHON);
        assert_eq!(site.expr, "ctx.pop(first, second,)");
        assert_eq!(site.receiver.as_deref(), Some("ctx"));

        let body =
            "function run() {\n  return promise\n    .then(done) /* (a */\n    .pop(\n      \
                    value, // (b\n    );\n}\n";
        let site = at(body, "pop", &ECMASCRIPT);
        assert_eq!(site.expr, "promise.then(done).pop(value,)");
        assert_eq!(site.receiver.as_deref(), Some("promise.then(done)"));
    }

    #[test]
    fn a_site_already_spanning_its_call_is_served_as_it_stands() {
        let body = "def test(ctx):\n    ctx.pop()\n";
        let start = body.find("ctx.pop()").unwrap();
        let site = read_expression(body, start, start + "ctx.pop()".len(), &PYTHON).unwrap();
        assert_eq!(site.expr, "ctx.pop()");
        assert_eq!(site.receiver.as_deref(), Some("ctx"));
        // A callee that is itself an expression has no receiver to name.
        let body = "def test(k):\n    handlers[k](x)\n";
        let start = body.find("handlers").unwrap();
        let site = read_expression(body, start, start + "handlers[k](x)".len(), &PYTHON).unwrap();
        assert_eq!(site.expr, "handlers[k](x)");
        assert_eq!(site.receiver, None);
    }

    #[test]
    fn a_use_without_arguments_reads_as_its_member_access() {
        let body = "def test():\n    expect_exception(flask.session.pop, \"foo\")\n";
        let site = at(body, "pop", &PYTHON);
        assert_eq!(site.expr, "flask.session.pop");
        assert_eq!(site.receiver.as_deref(), Some("flask.session"));
    }

    #[test]
    fn an_unclosed_argument_list_runs_to_the_end_of_its_line() {
        let body = "def run(ctx):\n    ctx.pop(values[\n";
        let site = at(body, "pop", &PYTHON);
        assert_eq!(site.expr, "ctx.pop(values[");
    }

    #[test]
    fn a_long_expression_keeps_its_callee_start_and_end() {
        let arguments = (0..40)
            .map(|n| format!("arg{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        let body = format!("def run(ctx):\n    ctx.pop({arguments})\n");
        let site = at(&body, "pop", &PYTHON);
        assert_eq!(site.expr.chars().count(), EXPR_MAX_CHARS, "{}", site.expr);
        assert!(site.expr.starts_with("ctx.pop(arg0, arg1"), "{}", site.expr);
        assert!(site.expr.ends_with("arg38, arg39)"), "{}", site.expr);
        assert_eq!(site.expr.matches(ELLIPSIS).count(), 1, "{}", site.expr);

        let receiver = (0..30)
            .map(|n| format!("part{n}"))
            .collect::<Vec<_>>()
            .join(".");
        let body = format!("def run(root):\n    {receiver}.pop(1, 2)\n");
        let site = at(&body, "pop", &PYTHON);
        assert_eq!(site.expr.chars().count(), EXPR_MAX_CHARS, "{}", site.expr);
        assert!(site.expr.starts_with("part0.part1"), "{}", site.expr);
        assert!(site.expr.contains(".pop(1, 2)"), "{}", site.expr);
        let served = site.receiver.unwrap();
        assert_eq!(served.chars().count(), RECEIVER_MAX_CHARS, "{served}");
        assert!(served.ends_with("part29"), "{served}");
    }

    #[test]
    fn a_range_outside_the_body_reads_as_none() {
        assert_eq!(read_expression("pop()", 3, 9, &PYTHON), None);
        assert_eq!(read_expression("pop()", 2, 2, &PYTHON), None);
    }

    fn entity(name: &str, file: &str) -> Entity {
        crate::call_sites::fixture::spanned_entity(
            name,
            file,
            kin_model::LanguageId::Python,
            0,
            "def f(): pass\n",
        )
    }

    #[test]
    fn receivers_order_by_whether_they_can_hold_the_owner() {
        let focal = entity("AppContext.pop", "src/flask/ctx.py");
        let owner = FocalOwner::of(&focal);
        let caller = entity("test_run", "tests/test_ctx.py");
        let hint = |receiver: &str| receiver_hint(receiver, &caller, &owner, &PYTHON);
        assert_eq!(hint("AppContext"), ReceiverHint::NamesOwner);
        assert_eq!(hint("self.app_context"), ReceiverHint::NamesOwner);
        assert_eq!(hint("values"), ReceiverHint::Unknown);
        assert_eq!(hint("flask.g.seen"), ReceiverHint::Unknown);
        assert_eq!(hint("[1, 2]"), ReceiverHint::Literal);
        assert_eq!(hint("\"a,b\""), ReceiverHint::Literal);
        assert_eq!(hint("f\"{x}\""), ReceiverHint::Literal);
        assert_eq!(hint("dict(a=1)"), ReceiverHint::Builtin);
        // `self` is the owner only inside the owner's own members.
        assert_eq!(hint("self"), ReceiverHint::Unknown);
        let member = entity("AppContext.__exit__", "src/flask/ctx.py");
        assert_eq!(
            receiver_hint("self", &member, &owner, &PYTHON),
            ReceiverHint::TypedAsOwner
        );
    }

    #[test]
    fn a_binding_the_caller_records_as_the_owner_reads_as_typed() {
        use kin_parser::value_origins::{VALUE_ORIGINS_KEY, VALUE_ORIGINS_VERSION};
        let focal = entity("AppContext.pop", "src/flask/ctx.py");
        let owner = FocalOwner::of(&focal);
        let mut caller = entity("test_run", "tests/test_ctx.py");
        caller.metadata.extra.insert(
            VALUE_ORIGINS_KEY.into(),
            json!({
                "version": VALUE_ORIGINS_VERSION,
                "bindings": {
                    "ctx": {"call": {"callee": "flask.ctx.AppContext"}},
                    "alias": {"local": {"name": "ctx"}},
                    "values": {"declared": {"type_name": "dict"}},
                    "list": {"call": {"callee": "make_list"}},
                },
            }),
        );
        let hint = |receiver: &str| receiver_hint(receiver, &caller, &owner, &PYTHON);
        assert_eq!(hint("ctx"), ReceiverHint::TypedAsOwner);
        assert_eq!(hint("alias"), ReceiverHint::TypedAsOwner);
        assert_eq!(hint("values"), ReceiverHint::Builtin);
        // A local that shadows a builtin's name is not the builtin.
        assert_eq!(hint("list"), ReceiverHint::Unknown);
    }

    #[test]
    fn a_function_is_owned_by_its_module() {
        let focal = entity("url_for", "src/flask/helpers.py");
        let owner = FocalOwner::of(&focal);
        let caller = entity("run", "app.py");
        assert_eq!(
            receiver_hint("helpers", &caller, &owner, &PYTHON),
            ReceiverHint::NamesOwner
        );
        assert_eq!(snake_case("AppContext"), "app_context");
        assert_eq!(snake_case("HTTPAdapter"), "http_adapter");
    }

    /// A reply too large for its budget is paged, and every row keeps its
    /// expression on whichever page carries it: the pages put back together
    /// are the reply, disclosure included.
    #[test]
    fn a_paged_answer_keeps_every_row_s_expression() {
        let _cache_test = crate::trace_pages::cache_test_guard();
        let arguments = HashMap::from([
            ("query".to_string(), json!("AppContext.pop")),
            ("answer_only".to_string(), json!(true)),
        ]);
        let authority =
            json!({"repo": "example", "truth": 4, "source": "committed", "session": "a"});
        let context = crate::reference_pages::context(&arguments, &authority);
        let named: Vec<Value> = (0..40)
            .map(|index| {
                json!({
                    "caller": format!("caller-{index}"),
                    "caller_name": format!("test_{index}"),
                    "line_in_entity": index,
                    "callee": "pop",
                    "state": "unresolved",
                    "expr": format!("values_{index}.pop(\"lang_code\", None)"),
                    "receiver": format!("values_{index}"),
                })
            })
            .collect();
        let references: Vec<Value> = (0..30)
            .map(|index| {
                json!({
                    "entity_id": format!("00000000-0000-4000-8000-{index:012}"),
                    "name": format!("caller_{index}"),
                    "sites": [{
                        "callee": "ctx.pop",
                        "line_in_entity": index,
                        "expr": "ctx.pop()",
                        "receiver": "ctx",
                    }],
                })
            })
            .collect();
        let original = json!({
            "references": references,
            "total_upstream": 30,
            "call_site_candidates": {
                "named": named,
                "named_expr_elided": ExprCut { served: 40, elided: 3, bytes: 1990 }
                    .disclosure(NAMED_EXPR_BYTES_MAX, "rows"),
            },
            "negative": {"safe_to_conclude_absent": false},
            "_kin": {
                "verdict": {"state": "certified_except"},
                "repository": {"root": "/example/project"},
                "graph_as_of": {"generation": 4, "graph_root": "captured-root"},
            },
        });
        let mut public = original.clone();
        crate::semantic_surface::project("find_references", &mut public);
        assert_eq!(public["references"], original["references"]);
        assert_eq!(
            public["call_site_candidates"],
            original["call_site_candidates"]
        );
        assert_eq!(public["negative"], original["negative"]);
        assert_eq!(public["_kin"]["verdict"], original["_kin"]["verdict"]);
        assert_eq!(
            public["_kin"]["graph_as_of"],
            original["_kin"]["graph_as_of"]
        );
        assert!(public["_kin"]["repository"]["authority_context"].is_string());
        assert!(!public.to_string().contains("/example/project"));
        let mut page = crate::trace_pages::start_kind(
            original.clone(),
            context.clone(),
            3000,
            crate::trace_pages::PageKind::References,
        )
        .unwrap();
        let (mut rows, mut leftovers, mut readings) =
            (Vec::new(), Vec::new(), serde_json::Map::new());
        for pages in 1.. {
            assert!(pages < 100);
            assert!(serde_json::to_vec(&page).unwrap().len() <= 3000);
            rows.extend(page["references"].as_array().into_iter().flatten().cloned());
            leftovers.extend(
                page.pointer("/call_site_candidates/named")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            for reading in page["readings"].as_array().into_iter().flatten() {
                readings.insert(
                    reading["key"].as_str().unwrap().into(),
                    reading["value"].clone(),
                );
            }
            let Some(cursor) = page["next_cursor"].as_str() else {
                break;
            };
            page = crate::reference_pages::resume(cursor, &context, 3000).unwrap();
        }
        assert!(leftovers.iter().all(|row| row["expr"].is_string()));
        assert!(rows
            .iter()
            .all(|row| row["sites"][0]["expr"] == "ctx.pop()"));
        let mut reconstructed = Value::Object(readings);
        reconstructed["references"] = json!(rows);
        reconstructed["call_site_candidates"]["named"] = json!(leftovers);
        assert_eq!(reconstructed, public);
    }

    #[test]
    fn the_budget_serves_a_prefix_and_counts_what_it_cut() {
        let mut rows: Vec<Value> = (0..10)
            .map(|index| json!({"caller": index, "expr": "x".repeat(20), "receiver": "r"}))
            .collect();
        let per_row = "expr".len() + 4 + 22 + "receiver".len() + 4 + 3;
        let cut = cap_expressions(rows.iter_mut(), per_row * 4 + 1);
        assert_eq!(
            cut,
            ExprCut {
                served: 4,
                elided: 6,
                bytes: per_row * 4
            }
        );
        assert!(rows[..4].iter().all(|row| row.get("expr").is_some()));
        assert!(rows[4..]
            .iter()
            .all(|row| EXPR_FIELDS.iter().all(|key| row.get(*key).is_none())));
        let disclosure = cut.disclosure(per_row * 4 + 1, "rows").unwrap();
        assert_eq!(disclosure["elided"], 6);
        assert_eq!(disclosure["served"], 4);
        assert!(disclosure["detail"]
            .as_str()
            .unwrap()
            .contains("get_entity_source"));
        let untouched = cap_expressions(rows[..4].iter_mut(), usize::MAX);
        assert_eq!(untouched.disclosure(usize::MAX, "rows"), None);
    }

    /// Serves one caller's exact body, or refuses every read.
    struct OneBody(Option<&'static str>);

    impl SiteText for OneBody {
        fn quote(&self, caller: &Entity, site: &SourceSpan) -> Result<String, &'static str> {
            let body = self.0.ok_or("caller_source_unavailable")?;
            let span = caller.span.as_ref().ok_or("caller_has_no_span")?;
            crate::handlers::external_symbols::quote_site(caller, site, body, span.start_byte)
        }
    }

    /// A trace step quotes the line of the body that holds its site: the file
    /// line counted from the body's first line, trimmed of its comment and
    /// kept around the name it spells. A line outside the body, or one past
    /// the budget, keeps its number alone, and an unreadable body quotes none.
    #[test]
    fn a_trace_site_is_quoted_from_the_line_of_the_body_that_holds_it() {
        const BODY: &str = "def handle_exception(self, ctx, e):\n    # find the handler\n    \
                            handler = self._find_error_handler(e, ctx.request.blueprints)  # here\n    \
                            return handler(e)\n";
        let holder = crate::call_sites::fixture::spanned_entity(
            "Flask.handle_exception",
            "src/flask/app.py",
            kin_model::LanguageId::Python,
            0,
            BODY,
        );
        // The fixture's span starts on 0-based line 10, so the body's third
        // line is file line 13.
        let name = spelled_name("App._find_error_handler");
        assert_eq!(name, "_find_error_handler");
        let mut spent = 0;
        let sites = quote_lines(&OneBody(Some(BODY)), &holder, name, &[13, 40], &mut spent)
            .expect("the body is read");
        let expected = "handler = self._find_error_handler(e, ctx.request.blueprints)";
        assert_eq!(
            sites,
            vec![json!({"line": 13, "expr": expected}), json!({"line": 40})]
        );
        assert_eq!(spent, expected.len());

        let mut spent = TRACE_SITE_EXPR_BYTES_MAX - 5;
        let sites = quote_lines(&OneBody(Some(BODY)), &holder, name, &[13], &mut spent).unwrap();
        assert_eq!(sites, vec![json!({"line": 13})]);

        assert!(quote_lines(&OneBody(None), &holder, name, &[13], &mut 0).is_none());

        assert!(trace_sites_in_parent("callee"));
        for role in ["caller", "referrer", "importer", "type_user"] {
            assert!(!trace_sites_in_parent(role), "{role}");
        }
        assert_eq!(spelled_name("Scheduler::advance_timers"), "advance_timers");
    }
}
