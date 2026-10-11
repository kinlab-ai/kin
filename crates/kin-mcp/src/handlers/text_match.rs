// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Request-bounded matching over already-authorized source bytes.
//!
//! Exact matching is the default and includes overlapping occurrences. Case
//! folding is explicit and uses regex's Unicode simple case folding. Patterns
//! use regex syntax, including inline flags that can override the options in
//! their scope. Pattern matches are non-overlapping; case-insensitive literal
//! matches retain overlap. Callers must propagate an error and discard every
//! accumulated result: a budget refusal is never a partial successful search.
//! Context-dependent zero-width patterns are refused when the searched bytes
//! expose an empty match, even after earlier nonempty matches were found.

use std::ops::Range;
use std::time::{Duration, Instant};

use regex::bytes::{Regex, RegexBuilder};

use crate::error::{McpError, Result};

const MAX_PATTERN_BYTES: usize = 4_096;
const MAX_COMPILED_BYTES: usize = 1 << 20;
const MAX_UNIT_BYTES: usize = 8 << 20;
const MAX_TOTAL_BYTES: usize = 64 << 20;
const MAX_MATCHES: usize = 100_000;
const MAX_UNITS: usize = 10_000;
const MAX_ELAPSED: Duration = Duration::from_secs(2);
const EXCERPT_CHARS: usize = 160;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Options {
    pub pattern: bool,
    pub ignore_case: bool,
}

/// One deadline and one set of counters for the entire request, compilation
/// included. Checks are cooperative: a regex engine call is checked before
/// and after it runs, and a late result is refused rather than returned.
pub(crate) struct Budget {
    started: Instant,
    units: usize,
    bytes: usize,
    matches: usize,
    exhausted: bool,
}

impl Budget {
    pub(super) fn new() -> Self {
        Self {
            started: Instant::now(),
            units: 0,
            bytes: 0,
            matches: 0,
            exhausted: false,
        }
    }

    pub(super) fn check(&self) -> Result<()> {
        if self.exhausted {
            return Err(budget_error("a prior search budget was exhausted"));
        }
        if self.started.elapsed() >= MAX_ELAPSED {
            return Err(budget_error(
                "the cooperative 2-second search deadline was exceeded",
            ));
        }
        Ok(())
    }

    pub(super) fn charge_unit(&mut self, bytes: usize) -> Result<()> {
        self.check()?;
        let reason = if bytes > MAX_UNIT_BYTES {
            Some("one source unit exceeds 8 MiB")
        } else if self.units >= MAX_UNITS {
            Some("the request exceeds 10,000 source units")
        } else if bytes > MAX_TOTAL_BYTES - self.bytes {
            Some("the request exceeds 64 MiB of source")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.exhausted = true;
            return Err(budget_error(reason));
        }
        self.units += 1;
        self.bytes += bytes;
        Ok(())
    }

    fn charge_match(&mut self) -> Result<()> {
        self.check()?;
        if self.matches >= MAX_MATCHES {
            self.exhausted = true;
            return Err(budget_error("the request exceeds 100,000 matches"));
        }
        self.matches += 1;
        Ok(())
    }
}

fn budget_error(reason: &str) -> McpError {
    McpError::InvalidParams(format!(
        "text search refused: {reason}; narrow the search and try again"
    ))
}

enum Engine {
    /// A linear-time literal scan; the prefix table also preserves overlaps.
    Exact {
        needle: Vec<u8>,
        prefix: Vec<usize>,
    },
    Regex {
        regex: Regex,
        overlap: bool,
    },
}

pub(super) struct Matcher {
    engine: Engine,
}

impl Matcher {
    pub(super) fn new(query: &str, options: Options, budget: &mut Budget) -> Result<Self> {
        budget.check()?;
        if query.is_empty() {
            return Err(McpError::InvalidParams(
                "text search query must not be empty".into(),
            ));
        }
        if query.len() > MAX_PATTERN_BYTES {
            return Err(McpError::InvalidParams(
                "text search query exceeds 4,096 bytes".into(),
            ));
        }
        let engine = if !options.pattern && !options.ignore_case {
            let needle = query.as_bytes().to_vec();
            let mut prefix = vec![0; needle.len()];
            let mut matched = 0;
            for at in 1..needle.len() {
                while matched > 0 && needle[at] != needle[matched] {
                    matched = prefix[matched - 1];
                }
                if needle[at] == needle[matched] {
                    matched += 1;
                }
                prefix[at] = matched;
            }
            Engine::Exact { needle, prefix }
        } else {
            let pattern = if options.pattern {
                query.to_owned()
            } else {
                regex::escape(query)
            };
            let built = RegexBuilder::new(&pattern)
                .unicode(true)
                .case_insensitive(options.ignore_case)
                .size_limit(MAX_COMPILED_BYTES)
                .dfa_size_limit(MAX_COMPILED_BYTES)
                .build();
            budget.check()?;
            let regex = built.map_err(|error| {
                McpError::InvalidParams(format!(
                    "text search pattern is invalid or exceeds the 1 MiB compiled limit: {error}"
                ))
            })?;
            // Empty-input checks catch nullable patterns and bare anchors.
            // Context-dependent empty matches (for example word boundaries)
            // are additionally refused at every actual occurrence below.
            if regex.is_match(b"") {
                return Err(zero_width_error());
            }
            Engine::Regex {
                regex,
                overlap: !options.pattern,
            }
        };
        budget.check()?;
        Ok(Self { engine })
    }

    pub(super) fn ranges(&self, bytes: &[u8], budget: &mut Budget) -> Result<Vec<Range<usize>>> {
        budget.charge_unit(bytes.len())?;
        let mut ranges = Vec::new();
        match &self.engine {
            Engine::Exact { needle, prefix } => {
                let mut matched = 0;
                for (at, byte) in bytes.iter().copied().enumerate() {
                    if at % 4_096 == 0 {
                        budget.check()?;
                    }
                    while matched > 0 && byte != needle[matched] {
                        matched = prefix[matched - 1];
                    }
                    if byte == needle[matched] {
                        matched += 1;
                    }
                    if matched == needle.len() {
                        budget.charge_match()?;
                        ranges.push(at + 1 - needle.len()..at + 1);
                        matched = prefix[matched - 1];
                    }
                }
            }
            Engine::Regex { regex, overlap } => {
                let mut from = 0;
                loop {
                    budget.check()?;
                    let found = regex.find_at(bytes, from);
                    budget.check()?;
                    let Some(found) = found else { break };
                    if found.is_empty() {
                        return Err(zero_width_error());
                    }
                    budget.charge_match()?;
                    ranges.push(found.range());
                    from = if *overlap {
                        found.start() + 1
                    } else {
                        found.end()
                    };
                    // Search once at the end too: a pattern can have a
                    // nonempty branch followed by an empty boundary branch.
                }
            }
        }
        budget.check()?;
        Ok(ranges)
    }
}

fn zero_width_error() -> McpError {
    McpError::InvalidParams("text search patterns must not produce zero-width matches".into())
}

/// At most 160 Unicode characters around the actual match start, confined to
/// its first line. Decode only a bounded byte window, even on a very long line.
/// Invalid UTF-8 is displayed lossily; match coordinates always remain bytes.
pub(super) fn excerpt(bytes: &[u8], match_start: usize) -> String {
    let mut at = match_start.min(bytes.len());
    // A valid UTF-8 scalar has at most three continuation bytes. Malformed
    // source must not turn excerpt construction into an unbounded backscan.
    for _ in 0..3 {
        if at > 0 && at < bytes.len() && bytes[at] & 0xc0 == 0x80 {
            at -= 1;
        } else {
            break;
        }
    }
    let mut left = at.saturating_sub(EXCERPT_CHARS * 4);
    while left < at && bytes[left] & 0xc0 == 0x80 {
        left += 1;
    }
    let mut right = bytes.len().min(at.saturating_add(EXCERPT_CHARS * 4));
    while right > at && right < bytes.len() && bytes[right] & 0xc0 == 0x80 {
        right -= 1;
    }
    if let Some(newline) = bytes[left..at]
        .iter()
        .rposition(|byte| matches!(byte, b'\n' | b'\r'))
    {
        left += newline + 1;
    }
    if let Some(newline) = bytes[at..right]
        .iter()
        .position(|byte| matches!(byte, b'\n' | b'\r'))
    {
        right = at + newline;
    }
    let before: Vec<char> = String::from_utf8_lossy(&bytes[left..at]).chars().collect();
    let after: Vec<char> = String::from_utf8_lossy(&bytes[at..right]).chars().collect();
    let mut take_before = before.len().min(EXCERPT_CHARS / 2);
    let take_after = after.len().min(EXCERPT_CHARS - take_before);
    take_before = before.len().min(EXCERPT_CHARS - take_after);
    before[before.len() - take_before..]
        .iter()
        .chain(&after[..take_after])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(query: &str, options: Options, bytes: &[u8]) -> Result<Vec<Range<usize>>> {
        let mut budget = Budget::new();
        Matcher::new(query, options, &mut budget)?.ranges(bytes, &mut budget)
    }

    fn pattern() -> Options {
        Options {
            pattern: true,
            ignore_case: false,
        }
    }

    #[test]
    fn exact_matches_are_literal_overlapping_and_byte_addressed() {
        assert_eq!(
            matches("aa", Options::default(), b"aaaa").unwrap(),
            vec![0..2, 1..3, 2..4]
        );
        assert_eq!(
            matches("a.b+", Options::default(), b"axb a.b+ A.B+").unwrap(),
            vec![4..8]
        );
        assert_eq!(
            matches("é", Options::default(), "é🙂é".as_bytes()).unwrap(),
            vec![0..2, 6..8]
        );
        assert_eq!(
            matches("x", Options::default(), b"\xffx\x80").unwrap(),
            vec![1..2]
        );
    }

    #[test]
    fn case_folding_is_explicit_unicode_and_keeps_literal_punctuation() {
        let folded = Options {
            ignore_case: true,
            ..Options::default()
        };
        assert_eq!(
            matches("é.k", folded, "É.K éXk".as_bytes()).unwrap(),
            vec![0..4]
        );
        assert_eq!(
            matches("k", folded, "kKK".as_bytes()).unwrap(),
            vec![0..1, 1..2, 2..5]
        );
        assert_eq!(matches("aa", folded, b"aAa").unwrap(), vec![0..2, 1..3]);
        assert!(matches("é", Options::default(), "É".as_bytes())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn patterns_return_actual_byte_ranges_and_allow_inline_flags() {
        assert_eq!(
            matches(r"v\d+", pattern(), b"v1 v234").unwrap(),
            vec![0..2, 3..7]
        );
        assert_eq!(matches(r"(?i)abc", pattern(), b"ABC").unwrap(), vec![0..3]);
        assert_eq!(
            matches("first\nsecond", pattern(), b"x first\nsecond y").unwrap(),
            vec![2..14]
        );
        assert!(matches("[", pattern(), b"text").is_err());
    }

    #[test]
    fn empty_and_context_dependent_zero_width_matches_are_refused() {
        for query in ["", "a*", "^", "$", r"\b", r"a|\b", r"(?m)^", r"\B"] {
            assert!(matches(query, pattern(), b"a word").is_err(), "{query:?}");
        }
    }

    #[test]
    fn query_and_compiled_limits_fail_closed() {
        assert!(matches(&"x".repeat(MAX_PATTERN_BYTES), Options::default(), b"").is_ok());
        assert!(matches(&"x".repeat(MAX_PATTERN_BYTES + 1), Options::default(), b"").is_err());
        assert!(matches(
            &"é".repeat(MAX_PATTERN_BYTES / 2 + 1),
            Options::default(),
            b""
        )
        .is_err());
        assert!(matches(r"(?:x{1000}){1000}", pattern(), b"").is_err());
    }

    #[test]
    fn source_and_result_limits_are_shared_across_units() {
        let mut budget = Budget::new();
        let matcher = Matcher::new("x", Options::default(), &mut budget).unwrap();
        budget.bytes = MAX_TOTAL_BYTES - 2;
        assert!(matcher.ranges(b"ab", &mut budget).is_ok());
        assert!(matcher.ranges(b"c", &mut budget).is_err());
        assert!(budget.check().is_err());

        let mut budget = Budget::new();
        assert!(matcher
            .ranges(&vec![b'y'; MAX_UNIT_BYTES + 1], &mut budget)
            .is_err());
        let mut budget = Budget::new();
        budget.units = MAX_UNITS - 1;
        assert!(matcher.ranges(b"", &mut budget).is_ok());
        assert!(matcher.ranges(b"", &mut budget).is_err());

        let mut budget = Budget::new();
        budget.matches = MAX_MATCHES - 1;
        assert_eq!(matcher.ranges(b"x", &mut budget).unwrap(), vec![0..1]);
        assert!(matcher.ranges(b"x", &mut budget).is_err());
        assert!(matcher.ranges(b"", &mut budget).is_err());
    }

    #[test]
    fn deadline_applies_to_construction_scanning_and_later_attribution() {
        let mut expired = Budget::new();
        expired.started = Instant::now() - MAX_ELAPSED;
        assert!(Matcher::new("x", Options::default(), &mut expired).is_err());
        let mut budget = Budget::new();
        let matcher = Matcher::new("x", Options::default(), &mut budget).unwrap();
        assert!(matcher.ranges(b"x", &mut expired).is_err());
        assert!(expired.check().is_err());
    }

    #[test]
    fn excerpts_center_at_the_match_and_bound_unicode_and_lines() {
        let source = format!(
            "{}MATCH{}\nnot this line",
            "é".repeat(1_000),
            "🙂".repeat(1_000)
        );
        let shown = excerpt(source.as_bytes(), 2_000);
        assert_eq!(shown.chars().count(), 160);
        assert_eq!(shown.chars().position(|c| c == 'M'), Some(80));
        assert!(!shown.contains('\u{fffd}'));
        assert!(!shown.contains('\n'));
        let source = b"first MATCH\nsecond\r\nthird";
        assert_eq!(excerpt(source, 6), "first MATCH");
        assert_eq!(excerpt(source, 12), "second");
        assert_eq!(excerpt(b"", 0), "");
        assert!(excerpt(&vec![b'x'; 10_000], 9_999).chars().count() <= 160);
        assert!(excerpt(&vec![0xff; 10_000], 5_000).chars().count() <= 160);
    }
}
