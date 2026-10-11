// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The front door's answer: route a plain question, run the verbs that answer
//! it, and compose one reply.
//!
//! The reply says what Kin ran and what in the question it read that as, then
//! what the question asked for that the query did not apply, then the answer
//! each verb gave in its own words, verdict first. Nothing here certifies
//! anything a verb did not: the reply's one verdict is the most pessimistic
//! of the verdicts it carries.
//!
//! Every verb runs through [`SubCalls`], which the server implements with the
//! same path a client's own call takes, so belt defaults, budgets, pages and
//! envelopes are the ones every verb already has.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Map, Value};

use super::extract::is_stop_word;
use super::sub_answer::{last_segment, Declaration, Standing, SubAnswer, COMPLETE, PARTIAL};
use super::{plan, Intent, Plan};
use crate::plain_reply::{COMPLETE_EXCEPT, COMPLETE_EXCEPT_INDIRECT};
use crate::session_handles::SessionHandles;

/// The tool's name, and the CLI subcommand's.
pub const TOOL_NAME: &str = "ask";

/// The most names one question runs a query for; the rest are named as not
/// run.
pub const MAX_TARGETS: usize = 8;
/// The most pages of one `refs` or `trace` answer the reply fetches.
const MAX_PAGES: usize = 6;
/// Paging stops once the answer's text passes this many bytes.
const MAX_PAGED_BYTES: usize = 16_000;
/// The byte budget each `refs` and name lookup is asked for, so an answer of
/// several sections is one page.
const ANSWER_BYTES: u64 = 40_000;
/// What one query's own line takes in a reply that holds several queries,
/// kept out of the bytes each query may answer in.
const QUERY_LINE_BYTES: usize = 240;
/// The most rows a name lookup returns.
const LOOKUP_LIMIT: u64 = 200;
/// How deep a callers walk goes: the most `trace` walks.
const DEPTH: u64 = 8;
/// The reply's default byte budget, and its bounds.
pub const DEFAULT_REPLY_BYTES: usize = 60_000;
pub const MIN_REPLY_BYTES: usize = 2_000;
pub const MAX_REPLY_BYTES: usize = 60_000;
/// The most characters of a not-held declaration's reason a list repeats.
const WHY_CHARS: usize = 110;

/// One `ask` call's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    pub question: String,
    /// Overrides what the question's words say about test code.
    pub include_tests: Option<bool>,
    /// A module prefix as rows spell `module`, applied as written; overrides
    /// a package the question names in words.
    pub scope: Option<String>,
    /// The most bytes the composed reply may take.
    pub max_output_bytes: usize,
    /// Whether the answer carries its plan and every call it made.
    pub explain: bool,
}

impl AskRequest {
    /// The request an `ask` call's arguments make, or the sentence that says
    /// what is wrong with them.
    pub fn from_arguments(arguments: &HashMap<String, Value>) -> Result<Self, String> {
        let question = arguments
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|question| !question.is_empty())
            .ok_or_else(|| {
                "ask needs a question in plain words, for example {\"question\":\"what calls parse_config?\"}."
                    .to_string()
            })?
            .to_string();
        let include_tests = match arguments.get("include_tests") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err("The include_tests option of ask is true or false.".into()),
        };
        let scope = match arguments.get("scope") {
            None | Some(Value::Null) => None,
            Some(Value::String(scope)) if !scope.trim().is_empty() => {
                Some(scope.trim().to_string())
            }
            Some(_) => {
                return Err(
                    "The scope option of ask is a module prefix as rows spell module, such as flask.json."
                        .into(),
                )
            }
        };
        let max_output_bytes = match arguments.get("max_output_bytes") {
            None | Some(Value::Null) => DEFAULT_REPLY_BYTES,
            Some(value) => match value.as_u64() {
                Some(bytes)
                    if (MIN_REPLY_BYTES as u64..=MAX_REPLY_BYTES as u64).contains(&bytes) =>
                {
                    bytes as usize
                }
                _ => {
                    return Err(format!(
                        "The max_output_bytes option of ask is a whole number from {MIN_REPLY_BYTES} to {MAX_REPLY_BYTES}."
                    ))
                }
            },
        };
        let explain = arguments
            .get("explain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(Self {
            question,
            include_tests,
            scope,
            max_output_bytes,
            explain,
        })
    }
}

/// How the front door runs one verb.
pub trait SubCalls: Send {
    /// Run `verb` with `arguments` as a client's call would run, and read its
    /// plain answer.
    fn call<'a>(
        &'a mut self,
        verb: &'a str,
        arguments: Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = SubAnswer> + Send + 'a>>;

    /// The handles a row is written with on this connection.
    fn handles(&self) -> &SessionHandles;
}

/// The composed answer.
#[derive(Debug, Clone)]
pub struct Composite {
    /// The reply, as one text.
    pub text: String,
    /// The most pessimistic verdict word of the answers it carries.
    pub verdict: String,
    pub standing: Standing,
    /// Whether no verb could answer at all.
    pub is_error: bool,
    /// The plan and every call made, for a caller that asked to see them.
    pub record: Value,
}

/// What a name lookup says about whether its rows are every declaration of
/// the name.
#[derive(Debug, Clone)]
struct LookupStanding {
    word: String,
    meaning: String,
    complete: bool,
    why: Vec<String>,
    kin_says_closed: bool,
}

impl LookupStanding {
    fn of(lookup: &SubAnswer) -> Self {
        let word = lookup.verdict().unwrap_or_default().to_string();
        let meaning = lookup.meaning().unwrap_or_default().to_string();
        let mut why = Vec::new();
        if word != COMPLETE && word != COMPLETE_EXCEPT {
            why.push(format!(
                "the answer line is \"{}\", not complete",
                if word.is_empty() { "missing" } else { &word }
            ));
        }
        if lookup.withheld() {
            why.push("rows were withheld".into());
        }
        if lookup.next_cursor().is_some() {
            why.push("more rows follow on a later page".into());
        }
        let held = lookup.declarations().len() as u64;
        if let Some(total) = lookup.total() {
            if total != held {
                why.push(format!("{held} of {total} matching rows were returned"));
            }
        }
        let lowered = meaning.to_lowercase();
        Self {
            complete: why.is_empty(),
            kin_says_closed: lowered.contains("every declaration named")
                || lowered.contains("every declaration whose name"),
            word,
            meaning,
            why,
        }
    }
}

/// What each of several declarations of one name is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EachQuery {
    /// Its direct callers, for a change question.
    Callers,
    /// Every use of it, for a question about whether it is used.
    Uses,
}

/// The question's words for each intent, as the first line repeats them.
fn reading(intent: Intent) -> &'static str {
    match intent {
        Intent::CallersDirect => "who calls it",
        Intent::CallersTransitive => "what can end up calling it",
        Intent::ChangeImpact => "what a change to it affects",
        Intent::IsItUsed => "whether it is used",
        Intent::WhereIsIt => "where it is declared",
        Intent::ShowSource => "its code",
        Intent::Declares => "which modules declare it",
        Intent::Other => "a question that names no single entity",
    }
}

fn tests_phrase(tests: bool) -> &'static str {
    if tests {
        "test code included"
    } else {
        "test code left out"
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The reason a not-held declaration carries, cut to one short clause.
fn short_why(why: Option<&str>) -> String {
    let why = why.unwrap_or("no reason recorded").trim();
    if why.chars().count() > WHY_CHARS {
        format!("{}...", why.chars().take(WHY_CHARS).collect::<String>())
    } else {
        why.to_string()
    }
}

/// Whether a declaration's own name is `name`, whole or as its last segment.
fn named(declaration: &Declaration, name: &str) -> bool {
    declaration.name == name || declaration.own_name() == name
}

/// Whether `module` is `wanted` or ends with it as whole segments.
fn module_is(module: &str, wanted: &str) -> bool {
    module == wanted || module.ends_with(&format!(".{wanted}"))
}

/// The written name's spellings, the most specific first, with the module
/// each puts in front when it puts one.
fn spellings(target: &str) -> Vec<kin_parser::name_syntax::NameForm> {
    let mut forms = kin_parser::name_syntax::written_name_forms(target);
    if forms.is_empty() {
        forms.push(kin_parser::name_syntax::NameForm {
            module: None,
            name: target.to_string(),
        });
    }
    forms
}

/// The rows that are the declaration a written name means: the first of its
/// spellings that matches any row, else every row.
fn same_named(rows: &[Declaration], target: &str) -> Vec<Declaration> {
    for form in spellings(target) {
        let matched: Vec<Declaration> = rows
            .iter()
            .filter(|row| {
                row.name == form.name
                    && form
                        .module
                        .as_deref()
                        .is_none_or(|module| module_is(&row.module, module))
            })
            .cloned()
            .collect();
        if !matched.is_empty() {
            return matched;
        }
    }
    rows.to_vec()
}

/// Lower-cased module tokens: each segment, and each part of a segment
/// joined by `-`.
fn module_tokens(module: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    for segment in module.split('.') {
        let lowered = segment.to_lowercase();
        for part in lowered.split('-') {
            tokens.insert(part.to_string());
        }
        tokens.insert(lowered);
    }
    tokens
}

/// The one row the question means among several of one name, or `None`.
///
/// A module the question qualifies the name with decides first. Then the
/// question's words: a row scores 2 when its module's last segment is written
/// in the question, and 1 for every other module word written there. A word
/// every row's module carries cannot tell them apart and scores nothing, and
/// neither does a question word. A single best row wins; a tie does not.
fn pick(
    rows: &[Declaration],
    target: &str,
    hints: &[String],
    question: &str,
) -> Option<Declaration> {
    let mut exact = same_named(rows, target);
    if exact.len() > 1 {
        for hint in hints {
            let mut hinted: Vec<Declaration> = exact
                .iter()
                .filter(|row| module_is(&row.module, hint))
                .cloned()
                .collect();
            if hinted.is_empty() {
                let stem = hint
                    .rsplit_once('.')
                    .map_or(hint.as_str(), |(stem, _)| stem);
                hinted = exact
                    .iter()
                    .filter(|row| {
                        row.module.rsplit('.').next() == Some(stem) || row.module.contains(stem)
                    })
                    .cloned()
                    .collect();
            }
            if !hinted.is_empty() {
                exact = hinted;
                break;
            }
        }
    }
    if exact.len() > 1 {
        let plain = strip_backticked(question);
        let written: BTreeSet<String> = plain
            .split(|character: char| {
                !(character.is_alphanumeric()
                    || character == '_'
                    || character == '$'
                    || character == '-')
            })
            .filter(|word| !word.is_empty())
            .map(str::to_string)
            .collect();
        let mut words: BTreeSet<String> = BTreeSet::new();
        for word in &written {
            let lowered = word.to_lowercase();
            for part in lowered.split('-') {
                words.insert(part.to_string());
            }
            words.insert(lowered);
        }
        let mut shared: Option<BTreeSet<String>> = None;
        for row in &exact {
            let tokens = module_tokens(&row.module);
            shared = Some(match shared {
                None => tokens,
                Some(shared) => shared.intersection(&tokens).cloned().collect(),
            });
        }
        let shared = shared.unwrap_or_default();
        let discriminating = |token: &str| !shared.contains(token) && !is_stop_word(token);
        let score = |row: &Declaration| -> usize {
            let last = row.module.rsplit('.').next().unwrap_or_default();
            let own = if written.contains(last) && discriminating(&last.to_lowercase()) {
                2
            } else {
                0
            };
            own + module_tokens(&row.module)
                .iter()
                .filter(|token| discriminating(token) && words.contains(*token))
                .count()
        };
        let mut ranked: Vec<(usize, Declaration)> =
            exact.iter().map(|row| (score(row), row.clone())).collect();
        ranked.sort_by(|left, right| right.0.cmp(&left.0));
        if ranked[0].0 > 0 && ranked[0].0 > ranked[1].0 {
            exact = vec![ranked[0].1.clone()];
        }
    }
    (exact.len() == 1).then(|| exact.remove(0))
}

fn strip_backticked(question: &str) -> String {
    let mut out = String::with_capacity(question.len());
    let mut inside = false;
    for character in question.chars() {
        if character == '`' {
            inside = !inside;
            out.push(' ');
        } else if !inside {
            out.push(character);
        }
    }
    out
}

/// The module prefix a package the question names in words stands for,
/// read off the target's own module: the word must be that module's first
/// segment, compared without case.
fn module_scope(module: &str, words: &str) -> Option<String> {
    let first = module.split('.').next()?;
    (module.contains('.') && first.eq_ignore_ascii_case(words.trim())).then(|| first.to_string())
}

/// One composition in progress.
struct Composer<'c> {
    calls: &'c mut dyn SubCalls,
    plan: Plan,
    request: AskRequest,
    tests: bool,
    record: Vec<Value>,
    words: Vec<String>,
}

impl Composer<'_> {
    async fn run(&mut self, verb: &str, arguments: Map<String, Value>) -> SubAnswer {
        let answer = self.calls.call(verb, arguments).await;
        self.record.push(json!({
            "verb": answer.verb,
            "arguments": answer.arguments,
            "seconds": (answer.seconds * 1000.0).round() / 1000.0,
            "bytes": answer.bytes,
            "error": answer.is_error,
        }));
        answer
    }

    /// Note an answer's verdict as one the reply carries.
    fn carries(&mut self, answer: &SubAnswer) {
        if let Some(word) = answer.verdict() {
            self.words.push(word.to_string());
        } else if !answer.is_error {
            self.words.push(String::new());
        }
    }

    fn render(&self, answer: &SubAnswer) -> String {
        answer.rendered(self.calls.handles())
    }

    fn lookup_arguments(&self, query: &str) -> Map<String, Value> {
        let mut arguments = Map::new();
        arguments.insert("query".into(), json!(query));
        arguments.insert("mode".into(), json!("name"));
        arguments.insert("details".into(), json!(true));
        arguments.insert("limit".into(), json!(LOOKUP_LIMIT));
        arguments.insert("max_output_bytes".into(), json!(ANSWER_BYTES));
        if !self.tests {
            arguments.insert("include_tests".into(), json!(false));
        }
        arguments
    }

    /// A name lookup for `target`: as written, then in each stored spelling
    /// its language writes it, then by its last segment, stopping at the
    /// first that finds a held declaration.
    async fn lookup(&mut self, target: &str) -> SubAnswer {
        let mut queries: Vec<String> = vec![target.to_string()];
        for form in spellings(target) {
            if form.module.is_none() && !queries.contains(&form.name) {
                queries.push(form.name);
            }
        }
        let last = last_segment(target).to_string();
        if !queries.contains(&last) {
            queries.push(last);
        }
        let mut first: Option<SubAnswer> = None;
        for query in queries {
            let answer = self.run("locate", self.lookup_arguments(&query)).await;
            if !answer.is_error && !answer.declarations().is_empty() {
                return answer;
            }
            first.get_or_insert(answer);
        }
        first.expect("at least one lookup ran")
    }

    /// Run `verb`, fetching the later pages of a `refs` or `trace` answer
    /// that says more rows follow. A change answer's later pages list what
    /// only depends on the answer and are not fetched; neither are the later
    /// pages of an answer already partial.
    async fn paged(&mut self, verb: &str, arguments: Map<String, Value>) -> (String, bool) {
        self.paged_within(verb, arguments, MAX_PAGED_BYTES).await
    }

    /// [`Self::paged`], paging only until the answer's text passes
    /// `text_bytes`, for a reply that holds several answers.
    async fn paged_within(
        &mut self,
        verb: &str,
        arguments: Map<String, Value>,
        text_bytes: usize,
    ) -> (String, bool) {
        let first = self.run(verb, arguments.clone()).await;
        if first.is_error {
            return (self.render(&first), true);
        }
        self.carries(&first);
        if first.next_cursor().is_some() && (verb == "impact" || first.is_partial()) {
            let sentence = if verb == "impact" {
                "Not listed here: the further entities that only depend on the ones above without calling it."
            } else {
                "More rows exist beyond this page; they were not fetched."
            };
            return (
                format!("{}\n{sentence}", self.render(&first.without_continuation())),
                false,
            );
        }
        let mut pages = vec![first];
        let mut text = self.render(&pages[0]);
        while matches!(verb, "refs" | "trace")
            && pages.len() < MAX_PAGES
            && text.len() < text_bytes.min(MAX_PAGED_BYTES)
        {
            let Some(cursor) = pages.last().and_then(SubAnswer::next_cursor) else {
                break;
            };
            let mut more_arguments = arguments.clone();
            more_arguments.insert("cursor".into(), json!(cursor));
            let more = self.run(verb, more_arguments).await;
            if more.is_error {
                break;
            }
            self.carries(&more);
            pages.push(more);
            let last = pages.len() - 1;
            text = pages
                .iter()
                .enumerate()
                .map(|(index, page)| {
                    if index < last {
                        self.render(&page.without_continuation())
                    } else {
                        self.render(page)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n(next page)\n");
        }
        (text, false)
    }

    fn not_applied_paths(&self) -> Vec<String> {
        self.plan
            .paths_not_applied
            .iter()
            .map(|path| {
                format!("the path {path} (Kin addresses code by module and name, never by path)")
            })
            .collect()
    }

    fn cue_clause(&self) -> String {
        match &self.plan.cue {
            Some(cue) => format!(
                " [read as \"{}\" from \"{cue}\"]",
                reading(self.plan.intent)
            ),
            None => String::new(),
        }
    }

    /// The closure sentence for a list of the declarations of `name`.
    fn closure(
        &self,
        standing: &LookupStanding,
        name: &str,
        held: usize,
        not_held: usize,
        others: usize,
    ) -> String {
        let tests = if self.tests {
            ""
        } else {
            ", test code left out"
        };
        if standing.kin_says_closed {
            return format!(
                "Kin's own answer line already states this: {}",
                standing.meaning
            );
        }
        if standing.complete {
            let extra = if others > 0 {
                format!(
                    " {others} further rows whose names merely contain {name} are left out of this list."
                )
            } else {
                String::new()
            };
            return format!(
                "These are every declaration named {name} in this repository{tests}: the {held} Kin holds as entities and the {not_held} it lists as not held. Kin's name lookup answered \"{}\" and returned every row it matched.{extra}",
                standing.word
            );
        }
        let said = if standing.meaning.is_empty() {
            String::new()
        } else {
            format!(" ({})", standing.meaning.trim_end_matches('.'))
        };
        let why = if standing.why.is_empty() {
            String::new()
        } else {
            format!("; {}", standing.why.join("; "))
        };
        format!(
            "This list is not established as every declaration named {name}: Kin's name lookup answered \"{}\"{said}{why}. Another declaration could be in what it left out.",
            if standing.word.is_empty() {
                "no answer line"
            } else {
                &standing.word
            }
        )
    }

    /// The limit to a package the question asks for: the `scope` option,
    /// else the package its words name.
    fn asked_scope(&self) -> Option<String> {
        self.request
            .scope
            .clone()
            .or_else(|| self.plan.scope_words.clone())
    }

    /// The module prefix a query about a declaration in `module` is limited
    /// to: the `scope` option as written, else the package the question's
    /// words name when it is `module`'s first segment. `None` when the
    /// question asks for no limit, or names a package `module` is not in.
    fn query_scope(&self, module: &str) -> Option<String> {
        match (&self.request.scope, &self.plan.scope_words) {
            (Some(explicit), _) => Some(explicit.clone()),
            (None, Some(words)) => module_scope(module, words),
            (None, None) => None,
        }
    }

    /// The bytes each of `count` queries may answer in when one reply holds
    /// them all after `spent` bytes: what the reply's budget leaves, less
    /// each query's own line, split evenly, never under the least budget a
    /// query takes and never over [`ANSWER_BYTES`]. A later query is then not
    /// cut off by the ones before it.
    fn query_share(&self, spent: usize, count: usize) -> usize {
        let left = self
            .request
            .max_output_bytes
            .saturating_sub(spent + count * QUERY_LINE_BYTES);
        (left / count.max(1)).clamp(crate::budget::RESPONSE_MIN_MAX_CHARS, ANSWER_BYTES as usize)
    }

    /// A name declared in several places, each queried on its own: every
    /// declaration, whether the name lookup establishes that list as all of
    /// them, then one query of each. A change question asks each for its
    /// direct callers, a use question for every use.
    ///
    /// The reply's verdict carries the name lookup's, and is not established
    /// when the lookup does not establish its list, or when a declaration it
    /// lists was not queried or its query failed: "no use" is only said of
    /// every declaration when each was found and each answered. The
    /// question's limit to a package is applied to each query or named as
    /// not applied, as for one declaration, and the reply's byte budget is
    /// split across the queries.
    async fn each_declaration(
        &mut self,
        target: &str,
        same: &[Declaration],
        lookup: &SubAnswer,
        query: EachQuery,
    ) -> (String, bool) {
        let tests = self.tests;
        let lead = match query {
            EachQuery::Callers => format!(
                "A change to that name has to be made in each of the {}:",
                same.len()
            ),
            EachQuery::Uses => "Each is checked on its own:".to_string(),
        };
        let mut head = format!(
            "Kin found {} declarations named {target}. {lead}\n{}",
            same.len(),
            same.iter()
                .enumerate()
                .map(|(index, row)| format!("  {}. {}", index + 1, row.handle()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let exact_name = if target.contains('.') || target.contains("::") {
            target.to_string()
        } else {
            last_segment(target).to_string()
        };
        let not_held: Vec<Declaration> = lookup
            .not_held()
            .into_iter()
            .filter(|row| named(row, &exact_name))
            .collect();
        let held_exact = lookup
            .declarations()
            .iter()
            .filter(|row| named(row, &exact_name))
            .count();
        let others =
            (lookup.declarations().len() - held_exact) + (lookup.not_held().len() - not_held.len());
        if !not_held.is_empty() {
            let unqueried = match query {
                EachQuery::Callers => " (each is a place the name is declared or re-exported, so a change reaches it too; callers are not queried for these)".to_string(),
                EachQuery::Uses => format!(
                    ", so no use query runs for {} and this answer does not say whether {} used",
                    if not_held.len() == 1 { "it" } else { "them" },
                    if not_held.len() == 1 { "it is" } else { "they are" },
                ),
            };
            head.push_str(&format!(
                "\n\nKin also lists {} named {target} where it holds no entity{unqueried}:\n{}",
                plural(
                    not_held.len(),
                    "further declaration",
                    "further declarations"
                ),
                not_held
                    .iter()
                    .map(|row| format!("  - {}  [{}]", row.handle(), short_why(row.why.as_deref())))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            // A declaration no query ran for is not answered for.
            self.words.push(PARTIAL.to_string());
        }
        // Whether these are every declaration is the lookup's to say.
        let standing = LookupStanding::of(lookup);
        self.carries(lookup);
        if !standing.complete {
            self.words.push(PARTIAL.to_string());
        }
        head.push('\n');
        head.push_str(&self.closure(&standing, &exact_name, same.len(), not_held.len(), others));
        let ran = match query {
            EachQuery::Callers => "the direct callers of each",
            EachQuery::Uses => "every use of each",
        };
        head.push_str(&format!(
            "\nKin then ran {ran}, {}{}.",
            tests_phrase(tests),
            self.cue_clause()
        ));
        // The question's limit to a package, applied to each query it maps
        // to and named for each it does not.
        let scopes: Vec<Option<String>> = same
            .iter()
            .map(|row| self.query_scope(&row.module))
            .collect();
        let mut not_applied: Vec<String> = Vec::new();
        if let Some(scope) = self.asked_scope() {
            let unscoped: Vec<String> = same
                .iter()
                .zip(&scopes)
                .filter(|(_, scope)| scope.is_none())
                .map(|(row, _)| row.handle())
                .collect();
            if unscoped.len() == same.len() {
                not_applied.push(format!(
                    "the limit to {scope} (these queries ran without it; rows name their module)"
                ));
            } else if !unscoped.is_empty() {
                let one = unscoped.len() == 1;
                not_applied.push(format!(
                    "the limit to {scope} for {} ({} not under it, so {} without it; rows name their module)",
                    unscoped.join(", "),
                    if one { "its module is" } else { "their modules are" },
                    if one { "its query ran" } else { "their queries ran" },
                ));
            }
        }
        not_applied.extend(self.not_applied_paths());
        if !not_applied.is_empty() {
            head.push_str(&format!(" Not applied: {}.", not_applied.join("; ")));
        }
        let share = self.query_share(head.len(), same.len());
        let mut parts = vec![head];
        let mut failed = false;
        for (row, scope) in same.iter().zip(scopes) {
            let mut arguments = Map::new();
            arguments.insert("entity_id".into(), json!(row.entity_id));
            if query == EachQuery::Callers {
                arguments.insert("relation_kinds".into(), json!(["calls"]));
            }
            arguments.insert("include_tests".into(), json!(tests));
            arguments.insert("max_output_bytes".into(), json!(share));
            let mut said = match query {
                EachQuery::Callers => format!("direct callers of {}", row.handle()),
                EachQuery::Uses => format!(
                    "every use of {} (calls, imports, references), {}",
                    row.handle(),
                    tests_phrase(tests)
                ),
            };
            if let Some(module) = scope {
                said.push_str(&format!(", within modules under {module}"));
                arguments.insert("scope".into(), json!(module));
            }
            let (text, error) = self.paged_within("refs", arguments, share).await;
            if error {
                // A query that failed answered nothing for its declaration.
                self.words.push(PARTIAL.to_string());
            }
            failed |= error;
            parts.push(format!("Kin ran: {said}.\n{text}"));
        }
        (parts.join("\n\n"), failed)
    }

    /// Whether a module declares the function, or whether other declarations
    /// of it exist: yes or no from the name lookup, then every declaration.
    async fn declares(&mut self, target: &str) -> (String, bool) {
        let function = last_segment(target).to_string();
        let lookup = self.run("locate", self.lookup_arguments(&function)).await;
        if lookup.is_error {
            return (self.render(&lookup), true);
        }
        self.carries(&lookup);
        let standing = LookupStanding::of(&lookup);
        let all_held = lookup.declarations();
        let all_not_held = lookup.not_held();
        let held: Vec<Declaration> = all_held
            .iter()
            .filter(|row| named(row, &function))
            .cloned()
            .collect();
        let not_held: Vec<Declaration> = all_not_held
            .iter()
            .filter(|row| named(row, &function))
            .cloned()
            .collect();
        let others = (all_held.len() - held.len()) + (all_not_held.len() - not_held.len());
        let every: Vec<(Declaration, bool)> = held
            .iter()
            .map(|row| (row.clone(), true))
            .chain(not_held.iter().map(|row| (row.clone(), false)))
            .collect();
        let rows_in = |module: &str| -> Vec<(Declaration, bool)> {
            every
                .iter()
                .filter(|(row, _)| module_is(&row.module, module))
                .cloned()
                .collect()
        };
        let describe = |rows: &[(Declaration, bool)], why: bool| -> String {
            let mut listed: Vec<String> = rows
                .iter()
                .take(6)
                .map(|(row, is_held)| {
                    if why && !is_held {
                        format!(
                            "{} (not held: {})",
                            row.handle(),
                            short_why(row.why.as_deref())
                        )
                    } else {
                        row.handle()
                    }
                })
                .collect();
            if rows.len() > 6 {
                listed.push("...".into());
            }
            listed.join("; ")
        };
        let question = self.plan.declares.clone();
        let (modules, known, asked_other) = match &question {
            Some(asked) => (
                asked.modules.clone(),
                asked.known.clone(),
                asked.asked_other,
            ),
            None => (Vec::new(), Vec::new(), false),
        };
        let caveat = if standing.complete {
            ""
        } else {
            " (Kin's list is not established as complete; see the last line.)"
        };
        let mentions_export = {
            let lowered = self.request.question.to_lowercase();
            ["export", "re-export", "reexport", "import"]
                .iter()
                .any(|word| lowered.contains(word))
        };
        let module_line = |module: &str| -> String {
            let rows = rows_in(module);
            if !rows.is_empty() {
                return format!(
                    "Yes. {module} has {} of {function}: {}.",
                    plural(rows.len(), "declaration", "declarations"),
                    describe(&rows, true)
                );
            }
            let last = module.rsplit('.').next().unwrap_or(module);
            let mut extending: Vec<String> = every
                .iter()
                .map(|(row, _)| row.module.clone())
                .filter(|candidate| {
                    let own = candidate.rsplit('.').next().unwrap_or(candidate);
                    own.starts_with(last) && own != last
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            extending.truncate(4);
            let star = if mentions_export {
                " A wildcard re-export or an import is not a declaration of the name and is not in Kin's list, so this does not rule it out."
            } else {
                ""
            };
            let extended = if extending.is_empty() {
                String::new()
            } else {
                format!(
                    " Modules whose names extend it and do declare {function}: {}.",
                    extending.join(", ")
                )
            };
            format!(
                "No. No declaration of {function}, held or not held, sits in {module}.{star}{extended}{caveat}"
            )
        };
        let holders: Vec<String> = modules
            .iter()
            .filter(|module| !rows_in(module).is_empty())
            .cloned()
            .collect();
        let examples: Vec<String> = modules
            .iter()
            .filter(|module| !holders.contains(module))
            .cloned()
            .collect();
        let mut lines: Vec<String> = Vec::new();
        if asked_other || modules.is_empty() {
            let mut known_modules: BTreeSet<String> = known.into_iter().collect();
            known_modules.extend(holders.iter().cloned());
            let beyond: Vec<(Declaration, bool)> = every
                .iter()
                .filter(|(row, _)| {
                    !known_modules
                        .iter()
                        .any(|module| module_is(&row.module, module))
                })
                .cloned()
                .collect();
            let named_known = if known_modules.is_empty() {
                "none named".to_string()
            } else {
                known_modules.iter().cloned().collect::<Vec<_>>().join(", ")
            };
            if every.is_empty() {
                lines.push(format!(
                    "No. Kin lists no declaration named {function}, held or not held.{caveat}"
                ));
            } else if !beyond.is_empty() && known_modules.is_empty() {
                lines.push(format!(
                    "Yes. Kin lists {} named {function} (listed below, with what is not held): {}.",
                    plural(beyond.len(), "declaration", "declarations"),
                    describe(&beyond, false)
                ));
            } else if !beyond.is_empty() {
                lines.push(format!(
                    "Yes. Beyond {named_known}, Kin lists {} named {function} (listed below, with what is not held): {}.",
                    plural(beyond.len(), "more declaration", "more declarations"),
                    describe(&beyond, false)
                ));
            } else {
                lines.push(format!(
                    "No. Every declaration of {function} Kin lists is in {named_known}; there is no other one.{caveat}"
                ));
            }
            lines.extend(examples.iter().map(|module| module_line(module)));
        } else {
            lines.extend(modules.iter().map(|module| module_line(module)));
        }
        lines.push(String::new());
        lines.push(format!(
            "Declarations named {function} that Kin lists ({} held as entities, {} not held):",
            held.len(),
            not_held.len()
        ));
        for (index, (row, is_held)) in every.iter().enumerate() {
            if *is_held {
                lines.push(format!("  {}. {}", index + 1, row.handle()));
            } else {
                lines.push(format!(
                    "  {}. {}  [not held as an entity: {}]",
                    index + 1,
                    row.handle(),
                    short_why(row.why.as_deref())
                ));
            }
        }
        if every.is_empty() {
            lines.push("  (none)".into());
        }
        lines.push(String::new());
        lines.push(format!(
            "Kin ran: locate by name for {function}{}{}.",
            if self.tests {
                ""
            } else {
                ", test code left out"
            },
            self.cue_clause()
        ));
        lines.push(self.closure(&standing, &function, held.len(), not_held.len(), others));
        (lines.join("\n"), false)
    }

    /// The answer for one named target.
    async fn one(&mut self, target: &str) -> (String, bool) {
        let intent = self.plan.intent;
        if intent == Intent::Declares {
            return self.declares(target).await;
        }
        let tests = self.tests;
        let lookup = self.lookup(target).await;
        let rows = lookup.declarations();
        if intent == Intent::WhereIsIt || rows.is_empty() {
            self.carries(&lookup);
            let tail = if tests { "" } else { ", test code left out" };
            return (
                format!(
                    "Kin ran: locate by name for {target}{tail}{}.\n{}",
                    self.cue_clause(),
                    self.render(&lookup)
                ),
                lookup.is_error,
            );
        }
        let mut hints: Vec<String> = self.plan.qualifier.iter().cloned().collect();
        hints.extend(spellings(target).into_iter().filter_map(|form| form.module));
        let mut chosen = pick(&rows, target, &hints, &self.request.question);
        let mut same = same_named(&rows, target);
        let bare = !target.contains('.') && !target.contains("::");
        if intent == Intent::ChangeImpact && bare {
            let ends: Vec<Declaration> = rows
                .iter()
                .filter(|row| row.own_name() == target)
                .cloned()
                .collect();
            if ends.len() > same.len() {
                if ends.len() != 1 {
                    chosen = None;
                }
                same = ends;
            }
        }
        let each = match intent {
            Intent::ChangeImpact => Some(EachQuery::Callers),
            Intent::IsItUsed => Some(EachQuery::Uses),
            _ => None,
        };
        if let Some(query) =
            each.filter(|_| chosen.is_none() && same.len() > 1 && same.len() <= MAX_TARGETS)
        {
            return self.each_declaration(target, &same, &lookup, query).await;
        }
        let Some(chosen) = chosen else {
            self.carries(&lookup);
            return (
                format!(
                    "Kin found more than one match for {target}; ask again naming one of these exactly.\n{}",
                    self.render(&lookup)
                ),
                false,
            );
        };
        let handle = chosen.handle();
        let others: Vec<String> = same
            .iter()
            .filter(|row| row.handle() != handle)
            .map(Declaration::handle)
            .collect();
        let mut not_applied: Vec<String> = Vec::new();
        let mut scope_left = self.asked_scope();
        let mut arguments = Map::new();
        let (verb, mut said) = match intent {
            Intent::CallersDirect => {
                arguments.insert("entity_id".into(), json!(chosen.entity_id));
                arguments.insert("relation_kinds".into(), json!(["calls"]));
                arguments.insert("include_tests".into(), json!(tests));
                arguments.insert("max_output_bytes".into(), json!(ANSWER_BYTES));
                (
                    "refs",
                    format!("direct callers of {handle}, {}", tests_phrase(tests)),
                )
            }
            Intent::CallersTransitive => {
                arguments.insert("focal".into(), json!(chosen.entity_id));
                arguments.insert("direction".into(), json!("callers"));
                arguments.insert("depth".into(), json!(DEPTH));
                let mut said = format!(
                    "every direct and indirect caller of {handle} (callers of callers, depth {DEPTH}, the most this query walks)"
                );
                if !tests {
                    arguments.insert("include_tests".into(), json!(false));
                    said.push_str(", test code left out");
                }
                ("trace", said)
            }
            Intent::ChangeImpact => {
                arguments.insert("entity_ids".into(), json!([chosen.entity_id]));
                let mut said = format!(
                    "what a change to {handle} affects: callers and overriding or implementing methods"
                );
                if !tests {
                    arguments.insert("include_tests".into(), json!(false));
                    said.push_str(", test code left out");
                }
                ("impact", said)
            }
            Intent::IsItUsed => {
                arguments.insert("entity_id".into(), json!(chosen.entity_id));
                arguments.insert("include_tests".into(), json!(tests));
                arguments.insert("max_output_bytes".into(), json!(ANSWER_BYTES));
                (
                    "refs",
                    format!(
                        "every use of {handle} (calls, imports, references), {}",
                        tests_phrase(tests)
                    ),
                )
            }
            Intent::ShowSource | Intent::WhereIsIt | Intent::Declares | Intent::Other => {
                arguments.insert("entity_id".into(), json!(chosen.entity_id));
                scope_left = None;
                ("source", format!("the code of {handle}"))
            }
        };
        if matches!(verb, "refs" | "trace" | "impact") {
            if let Some(module) = self.query_scope(&chosen.module) {
                arguments.insert("scope".into(), json!(module));
                said.push_str(&format!(", within modules under {module}"));
                scope_left = None;
            }
        }
        if let Some(scope) = scope_left {
            not_applied.push(format!(
                "the limit to {scope} (this query ran without it; rows name their module)"
            ));
        }
        if verb != "source" {
            not_applied.extend(self.not_applied_paths());
        }
        let (text, failed) = self.paged(verb, arguments).await;
        let mut head = format!("Kin ran: {said}{}.", self.cue_clause());
        if !not_applied.is_empty() {
            head.push_str(&format!(" Not applied: {}.", not_applied.join("; ")));
        }
        if !others.is_empty() {
            let listed: Vec<String> = others.iter().take(6).cloned().collect();
            head.push_str(&format!(
                " Other declarations with this name, not part of this answer: {}.",
                listed.join(", ")
            ));
        }
        (format!("{head}\n{text}"), failed)
    }

    async fn compose(mut self) -> Composite {
        let targets: Vec<String> = self.plan.targets.clone();
        let mut parts: Vec<String> = Vec::new();
        let mut failed = false;
        if self.plan.intent == Intent::Other || targets.is_empty() {
            let mut arguments = Map::new();
            arguments.insert("question".into(), json!(self.request.question));
            let answer = self.run("context", arguments).await;
            failed = answer.is_error;
            self.carries(&answer);
            let mut head = "Kin ran: a context lookup for the whole question (it names no single entity to query).".to_string();
            let paths = self.not_applied_paths();
            if !paths.is_empty() {
                head.push_str(&format!(" Not applied: {}.", paths.join("; ")));
            }
            parts.push(format!("{head}\n{}", self.render(&answer)));
        } else {
            for target in targets.iter().take(MAX_TARGETS) {
                let (text, error) = self.one(target).await;
                failed |= error;
                parts.push(text);
            }
            if targets.len() > MAX_TARGETS {
                parts.push(format!(
                    "Not run: {}. Ask about them separately.",
                    targets[MAX_TARGETS..].join(", ")
                ));
            }
        }
        let mut text = parts.join("\n\n");
        let limit = self.request.max_output_bytes;
        if text.len() > limit {
            let note = format!(
                "\n(The rest of this answer did not fit in {limit} bytes; ask about fewer names at a time, or ask again with a larger max_output_bytes.)"
            );
            let mut cut = limit.saturating_sub(note.len());
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let cut = text[..cut].rfind('\n').unwrap_or(cut);
            text.truncate(cut);
            text.push_str(&note);
            self.words.push(PARTIAL.to_string());
        }
        let standing = self
            .words
            .iter()
            .map(|word| Standing::of(Some(word.as_str())))
            .max()
            .unwrap_or(Standing::NotEstablished);
        let verdict = match standing {
            Standing::Complete => COMPLETE.to_string(),
            Standing::CompleteExcept => self
                .words
                .iter()
                .find(|word| word.as_str() == COMPLETE_EXCEPT_INDIRECT)
                .or_else(|| {
                    self.words
                        .iter()
                        .find(|word| word.as_str() == COMPLETE_EXCEPT)
                })
                .cloned()
                .unwrap_or_else(|| COMPLETE_EXCEPT.to_string()),
            Standing::NotEstablished => PARTIAL.to_string(),
        };
        let record = json!({
            "intent": self.plan.intent,
            "cue": self.plan.cue,
            "targets": self.plan.targets,
            "include_tests": self.tests,
            "scope": self.asked_scope(),
            "paths_not_applied": self.plan.paths_not_applied,
            "calls": self.record,
        });
        Composite {
            text,
            verdict,
            standing,
            is_error: failed && parts.len() <= 1,
            record,
        }
    }
}

/// Answer `request`, running every verb through `calls`.
pub async fn answer(request: &AskRequest, calls: &mut dyn SubCalls) -> Composite {
    let plan = plan(&request.question);
    let tests = request.include_tests.unwrap_or(plan.include_tests);
    Composer {
        calls,
        plan,
        request: request.clone(),
        tests,
        record: Vec::new(),
        words: Vec::new(),
    }
    .compose()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(module: &str, name: &str) -> Declaration {
        Declaration {
            entity_id: format!("{module}::{name}"),
            module: module.into(),
            name: name.into(),
            why: None,
        }
    }

    #[test]
    fn a_qualifier_or_the_question_s_own_words_pick_one_of_several_declarations() {
        let rows = vec![
            row("flask.app", "_make_timedelta"),
            row("flask.sansio.app", "_make_timedelta"),
        ];
        let picked = pick(&rows, "_make_timedelta", &["flask.app".into()], "").unwrap();
        assert_eq!(picked.module, "flask.app");
        let rows = vec![
            row("react-dom-bindings.ReactDOMEventListener", "setEnabled"),
            row(
                "react-native-renderer.ReactNativeEventEmitter",
                "setEnabled",
            ),
        ];
        let picked = pick(
            &rows,
            "setEnabled",
            &[],
            "Is `setEnabled` in ReactDOMEventListener (react-dom-bindings) used anywhere?",
        )
        .unwrap();
        assert_eq!(picked.module, "react-dom-bindings.ReactDOMEventListener");
    }

    #[test]
    fn a_word_every_candidate_module_carries_does_not_pick() {
        let rows = vec![
            row("react-art.ReactFiberConfigART", "commitTextUpdate"),
            row("react-dom.ReactFiberConfigDOM", "commitTextUpdate"),
        ];
        assert!(pick(
            &rows,
            "commitTextUpdate",
            &[],
            "What calls react's commitTextUpdate?"
        )
        .is_none());
    }

    #[test]
    fn a_written_go_or_rust_name_matches_its_stored_spelling() {
        let rows = vec![
            row("server", "Server.Handle"),
            row("client", "Client.Handle"),
        ];
        assert_eq!(
            same_named(&rows, "(*Server).Handle"),
            vec![row("server", "Server.Handle")]
        );
        let rows = vec![row("store", "Store::get"), row("cache", "Cache::get")];
        assert_eq!(
            same_named(&rows, "Store.get"),
            vec![row("store", "Store::get")]
        );
    }

    #[test]
    fn package_words_become_a_module_scope_only_when_they_name_the_first_segment() {
        assert_eq!(
            module_scope("flask.sansio.scaffold", "flask"),
            Some("flask".into())
        );
        assert_eq!(
            module_scope(
                "react-reconciler.ReactFiberTreeReflection",
                "react-reconciler"
            ),
            Some("react-reconciler".into())
        );
        assert_eq!(
            module_scope("react-reconciler.ReactFiberTreeReflection", "reconciler"),
            None
        );
        assert_eq!(module_scope("toplevel", "toplevel"), None);
    }

    #[test]
    fn the_request_reads_its_options_and_refuses_bad_ones_in_a_sentence() {
        let arguments: HashMap<String, Value> = [
            ("question".to_string(), json!(" who calls run? ")),
            ("include_tests".to_string(), json!(false)),
            ("scope".to_string(), json!("flask")),
        ]
        .into_iter()
        .collect();
        let request = AskRequest::from_arguments(&arguments).unwrap();
        assert_eq!(request.question, "who calls run?");
        assert_eq!(request.include_tests, Some(false));
        assert_eq!(request.scope.as_deref(), Some("flask"));
        assert_eq!(request.max_output_bytes, DEFAULT_REPLY_BYTES);
        let refused = AskRequest::from_arguments(&HashMap::new()).unwrap_err();
        assert!(refused.starts_with("ask needs a question"), "{refused}");
        let bad: HashMap<String, Value> = [
            ("question".to_string(), json!("x")),
            ("max_output_bytes".to_string(), json!(10)),
        ]
        .into_iter()
        .collect();
        assert!(AskRequest::from_arguments(&bad).is_err());
    }

    /// A `SubCalls` that answers a name lookup with `locate` and every other
    /// verb with `answer`, and records each call.
    struct Recording {
        handles: SessionHandles,
        log: Vec<(String, Map<String, Value>)>,
        locate: Value,
        answer: Option<Value>,
        /// The entity whose query fails, when one does.
        failing: Option<String>,
    }

    impl Recording {
        fn new(locate: Value) -> Self {
            Self {
                handles: SessionHandles::default(),
                log: Vec::new(),
                locate,
                answer: None,
                failing: None,
            }
        }

        fn answering(mut self, answer: Value) -> Self {
            self.answer = Some(answer);
            self
        }

        fn calls(&self, verb: &str) -> Vec<&Map<String, Value>> {
            self.log
                .iter()
                .filter(|(called, _)| called == verb)
                .map(|(_, arguments)| arguments)
                .collect()
        }
    }

    impl SubCalls for Recording {
        fn call<'a>(
            &'a mut self,
            verb: &'a str,
            arguments: Map<String, Value>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SubAnswer> + Send + 'a>> {
            self.log.push((verb.to_string(), arguments.clone()));
            let failed = self.failing.as_deref().is_some_and(|failing| {
                arguments.get("entity_id").and_then(Value::as_str) == Some(failing)
            });
            let payload = if verb == "locate" {
                self.locate.as_object().cloned()
            } else if failed {
                None
            } else {
                self.answer.as_ref().and_then(Value::as_object).cloned()
            };
            let answer = SubAnswer {
                verb: verb.to_string(),
                arguments,
                payload,
                text: if failed {
                    "the query failed".into()
                } else {
                    "uses of this declaration".into()
                },
                is_error: failed,
                seconds: 0.0,
                bytes: 0,
            };
            Box::pin(async move { answer })
        }

        fn handles(&self) -> &SessionHandles {
            &self.handles
        }
    }

    /// A complete name lookup that returned `rows` as `(id, module, name)`.
    fn lookup_of(rows: &[(&str, &str, &str)]) -> Value {
        json!({
            "_kin": {"verdict": "complete", "meaning": "the matched declarations"},
            "results": rows
                .iter()
                .map(|(id, module, name)| json!({"entity_id": id, "module": module, "name": name}))
                .collect::<Vec<_>>(),
            "total": rows.len(),
        })
    }

    /// A reference answer that found nothing and says so as complete.
    fn no_use() -> Value {
        json!({
            "_kin": {"verdict": "complete", "meaning": "nothing uses it"},
            "references": [],
            "total_upstream": 0,
        })
    }

    fn asked(question: &str) -> AskRequest {
        AskRequest {
            question: question.into(),
            include_tests: Some(true),
            scope: None,
            max_output_bytes: DEFAULT_REPLY_BYTES,
            explain: false,
        }
    }

    /// A field and a method can share one stored name. A use question checks
    /// each of them. It does not ask the caller to pick, and it does not
    /// narrow the use query to calls alone.
    #[tokio::test]
    async fn a_tied_use_question_checks_each_declaration() {
        let mut calls = Recording::new(lookup_of(&[
            ("field-id", "typed_header", "TypedHeaderRejection::name"),
            ("method-id", "typed_header", "TypedHeaderRejection::name"),
        ]));
        let request = asked("Is `TypedHeaderRejection::name` used anywhere?");
        let composite = answer(&request, &mut calls).await;
        let refs = calls.calls("refs");
        assert_eq!(refs.len(), 2, "{:?}", calls.log);
        assert!(
            calls
                .log
                .iter()
                .all(|(verb, _)| verb == "locate" || verb == "refs"),
            "{:?}",
            calls.log
        );
        let ids: Vec<&str> = refs
            .iter()
            .map(|arguments| arguments["entity_id"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(ids, ["field-id", "method-id"]);
        for arguments in &refs {
            assert!(arguments.get("relation_kinds").is_none(), "{arguments:?}");
            assert_eq!(arguments["include_tests"], json!(true));
        }
        assert!(
            !composite.text.contains("ask again naming one"),
            "{}",
            composite.text
        );
        assert!(
            composite.text.contains("Each is checked on its own"),
            "{}",
            composite.text
        );
        assert!(
            composite
                .text
                .contains("Kin then ran every use of each, test code included"),
            "{}",
            composite.text
        );
    }

    /// "Is it used" over several declarations is complete only when the name
    /// lookup establishes them as every declaration and each was queried and
    /// answered. Each query saying "nothing uses it" proves no absence while
    /// a declaration is left unestablished, unqueried or unanswered.
    #[tokio::test]
    async fn several_declarations_are_complete_only_when_each_is_established_and_answered() {
        let rows = [
            ("html-id", "shop.render_html", "commit_update"),
            ("text-id", "shop.render_text", "commit_update"),
        ];
        let question = "Is `commit_update` used anywhere?";
        for change in [false, true] {
            let question = if change {
                "What has to change if I add a parameter to `commit_update`?"
            } else {
                question
            };
            // Every declaration established, each answered complete.
            let mut calls = Recording::new(lookup_of(&rows)).answering(no_use());
            let composite = answer(&asked(question), &mut calls).await;
            assert_eq!(calls.calls("refs").len(), 2, "{:?}", calls.log);
            assert_eq!(composite.verdict, COMPLETE, "{}", composite.text);

            // The lookup returned 2 of 3 matching rows.
            let mut short = lookup_of(&rows);
            short["total"] = json!(3);
            let mut calls = Recording::new(short).answering(no_use());
            let composite = answer(&asked(question), &mut calls).await;
            assert_eq!(composite.verdict, PARTIAL, "{}", composite.text);
            assert!(
                composite
                    .text
                    .contains("This list is not established as every declaration"),
                "{}",
                composite.text
            );

            // A third declaration Kin holds no entity for is not queried.
            let mut unheld = lookup_of(&rows);
            unheld["not_held"] = json!([{"module": "shop.compat", "name": "commit_update",
                "why": "re-exported by a name Kin holds no entity for"}]);
            let mut calls = Recording::new(unheld).answering(no_use());
            let composite = answer(&asked(question), &mut calls).await;
            assert_eq!(calls.calls("refs").len(), 2, "{:?}", calls.log);
            assert_eq!(composite.verdict, PARTIAL, "{}", composite.text);
            assert!(
                composite.text.contains("shop.compat :: commit_update"),
                "{}",
                composite.text
            );

            // One declaration's query failed.
            let mut calls = Recording::new(lookup_of(&rows)).answering(no_use());
            calls.failing = Some("text-id".into());
            let composite = answer(&asked(question), &mut calls).await;
            assert_eq!(composite.verdict, PARTIAL, "{}", composite.text);
        }
    }

    /// The question's limit to a package is applied to each declaration's
    /// query, as it is to one declaration's, or named as not applied for the
    /// declarations it does not map to. It is never dropped unsaid.
    #[tokio::test]
    async fn several_declarations_apply_or_name_the_asked_scope() {
        // The scope option applies to every query, for both questions.
        for question in [
            "Is `commit_update` used anywhere?",
            "What has to change if I add a parameter to `commit_update`?",
        ] {
            let mut calls = Recording::new(lookup_of(&[
                ("html-id", "shop.render_html", "commit_update"),
                ("text-id", "shop.render_text", "commit_update"),
            ]))
            .answering(no_use());
            let mut request = asked(question);
            request.scope = Some("shop.work".into());
            let composite = answer(&request, &mut calls).await;
            let refs = calls.calls("refs");
            assert_eq!(refs.len(), 2, "{:?}", calls.log);
            for arguments in refs {
                assert_eq!(arguments["scope"], json!("shop.work"), "{arguments:?}");
            }
            assert_eq!(
                composite
                    .text
                    .matches("within modules under shop.work")
                    .count(),
                2,
                "{}",
                composite.text
            );
            assert!(
                !composite.text.contains("Not applied"),
                "{}",
                composite.text
            );
        }

        // A package named in words is the first segment of one declaration's
        // module and not of the other's: that one is limited, and the other
        // is named as run without the limit.
        let mut calls = Recording::new(lookup_of(&[
            ("html-id", "shop.html", "commit_update"),
            ("web-id", "web.shop", "commit_update"),
        ]))
        .answering(no_use());
        let composite = answer(
            &asked("Is `commit_update` used anywhere inside the shop package?"),
            &mut calls,
        )
        .await;
        let refs = calls.calls("refs");
        assert_eq!(refs.len(), 2, "{:?} {}", calls.log, composite.text);
        let scoped: Vec<(&str, Option<&str>)> = refs
            .iter()
            .map(|arguments| {
                (
                    arguments["entity_id"].as_str().unwrap_or(""),
                    arguments.get("scope").and_then(Value::as_str),
                )
            })
            .collect();
        assert_eq!(scoped, [("html-id", Some("shop")), ("web-id", None)]);
        assert!(
            composite
                .text
                .contains("Not applied: the limit to shop for web.shop :: commit_update"),
            "{}",
            composite.text
        );
    }

    /// One reply holds every declaration's answer, so each query is asked
    /// for its share of the reply's budget, and the shares fit in it.
    #[tokio::test]
    async fn several_declarations_split_the_reply_budget() {
        let rows: Vec<(String, String)> = (0..MAX_TARGETS)
            .map(|index| (format!("id-{index}"), format!("shop.render_{index}")))
            .collect();
        let rows: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|(id, module)| (id.as_str(), module.as_str(), "commit_update"))
            .collect();
        let mut calls = Recording::new(lookup_of(&rows)).answering(no_use());
        let request = asked("Is `commit_update` used anywhere?");
        answer(&request, &mut calls).await;
        let budgets: Vec<usize> = calls
            .calls("refs")
            .iter()
            .map(|arguments| arguments["max_output_bytes"].as_u64().unwrap_or(0) as usize)
            .collect();
        assert_eq!(budgets.len(), MAX_TARGETS, "{:?}", calls.log);
        assert!(
            budgets.iter().all(|budget| *budget == budgets[0]),
            "{budgets:?}"
        );
        assert!(
            budgets.iter().sum::<usize>() < request.max_output_bytes,
            "{budgets:?}"
        );
        assert!(
            budgets[0] >= crate::budget::RESPONSE_MIN_MAX_CHARS,
            "{budgets:?}"
        );
    }
}
