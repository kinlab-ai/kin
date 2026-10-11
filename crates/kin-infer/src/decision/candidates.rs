// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Scoring a list of candidate texts against a query, with "none of these".
//!
//! A [`CandidateQuery`] holds a state (the query, or a call site described in text), a question,
//! the candidate texts and the text of the "none of these" choice. It is asked as one or more
//! [`ChoiceQuestion`]s of at most `per_question` options each, every one ending in "none of
//! these", because a model reads a handful of lettered options far better than dozens and a
//! tokenizer has only so many single-token letters.
//!
//! [`CandidateScores::from_answers`] combines the questions' answers into one distribution over
//! every candidate and "none of these". Question `q` saw only its own candidates, so its "none
//! of these" means "not one of mine". Treating the questions as independent experts on the one
//! target, a candidate `i` asked in question `q` gets weight
//! `p_q(i) * prod over the other questions r of p_r(none)`, "none of these" gets
//! `prod over all questions of p_q(none)`, and the weights are normalized. A query asked as one
//! question keeps that question's probabilities exactly.

use std::ops::Range;

use super::{
    first_max, Choice, ChoiceQuestion, Decision, DecisionError, Verdict, CALL_TARGET_NONE_OF_THESE,
    NONE_OF_THESE,
};

/// The question a [`CandidateQuery::new`] asks about its query.
pub const DEFAULT_CANDIDATE_QUESTION: &str = "Which of these candidates matches the query?";

/// The most candidates asked in one question unless a model's manifest or the caller says
/// otherwise. Kin's call-target model was trained on questions of 2 to 12 candidates.
pub const DEFAULT_OPTIONS_PER_QUESTION: usize = 12;

/// Candidate texts to score against a query, with a "none of these" choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateQuery {
    /// The text every question states first: the query, or a call site described in text. It is
    /// used verbatim and may span lines.
    pub state: String,
    /// The question asked about the state, without the `Question: ` prefix. A single line.
    pub question: String,
    /// The candidate texts, in the order the caller ranks them. A line break inside a candidate
    /// becomes a space in the prompt, so no candidate can pass for another lettered line.
    pub candidates: Vec<String>,
    /// The text of the "none of these" choice every question ends with. A single line.
    pub none_text: String,
}

impl CandidateQuery {
    /// Asks which of `candidates` matches a search query, stated as `Query: <query>`, with the
    /// plain [`NONE_OF_THESE`] choice.
    pub fn new<S: Into<String>>(query: &str, candidates: impl IntoIterator<Item = S>) -> Self {
        Self::about(
            format!("Query: {query}"),
            DEFAULT_CANDIDATE_QUESTION,
            candidates,
        )
    }

    /// Asks `question` about a caller-written `state`, with the plain [`NONE_OF_THESE`] choice.
    pub fn about<S: Into<String>>(
        state: impl Into<String>,
        question: impl Into<String>,
        candidates: impl IntoIterator<Item = S>,
    ) -> Self {
        Self {
            state: state.into(),
            question: question.into(),
            candidates: candidates.into_iter().map(Into::into).collect(),
            none_text: NONE_OF_THESE.to_owned(),
        }
    }

    /// Asks which candidate definition a call to `callee` executes, in the fixed wording of
    /// [`ChoiceQuestion::call_target`], about a call site described in `state`.
    pub fn call_target<S: Into<String>>(
        state: impl Into<String>,
        callee: &str,
        candidates: impl IntoIterator<Item = S>,
    ) -> Self {
        let wording = ChoiceQuestion::call_target(callee, Vec::new());
        Self {
            state: state.into(),
            question: wording.question,
            candidates: candidates.into_iter().map(Into::into).collect(),
            none_text: CALL_TARGET_NONE_OF_THESE.to_owned(),
        }
    }

    /// Replaces the text of the "none of these" choice.
    pub fn with_none_text(mut self, text: impl Into<String>) -> Self {
        self.none_text = text.into();
        self
    }

    /// The questions that ask this query with at most `per_question` candidates each.
    ///
    /// The candidates are split, in order, into the fewest contiguous runs that fit, with
    /// lengths as even as possible, so 13 candidates at 12 a question become runs of 7 and 6.
    /// Each entry holds the candidate indices a question lists and the question itself, whose
    /// options are those candidates followed by "none of these".
    pub fn questions(
        &self,
        per_question: usize,
    ) -> Result<Vec<(Range<usize>, ChoiceQuestion)>, DecisionError> {
        if per_question == 0 {
            return Err(DecisionError::InvalidInput(
                "a question must list at least one candidate".into(),
            ));
        }
        let n = self.candidates.len();
        if n == 0 {
            return Err(DecisionError::InvalidInput(
                "a query needs at least one candidate".into(),
            ));
        }
        let count = n.div_ceil(per_question);
        let (base, longer) = (n / count, n % count);
        let mut questions = Vec::with_capacity(count);
        let mut start = 0;
        for q in 0..count {
            let range = start..start + base + usize::from(q < longer);
            start = range.end;
            let options = self.candidates[range.clone()]
                .iter()
                .map(|text| single_line(text))
                .collect();
            let question = ChoiceQuestion::new(self.question.clone(), options)
                .with_none_text(self.none_text.clone());
            question.validate()?;
            questions.push((range, question));
        }
        Ok(questions)
    }
}

/// `text` on one line: a text with a line break becomes its trimmed, non-empty lines joined by
/// single spaces. A text without one is returned unchanged.
fn single_line(text: &str) -> String {
    if !text.contains(['\n', '\r']) {
        return text.to_owned();
    }
    text.split(['\n', '\r'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The answer to a [`CandidateQuery`]: a probability for every candidate and for "none of
/// these", summing to one.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateScores {
    /// The probability of each candidate, in [`CandidateQuery::candidates`] order.
    pub probabilities: Vec<f32>,
    /// The probability that no candidate matches.
    pub none_probability: f32,
    /// The most probable choice. A tie goes to the earlier candidate, and "none of these" last.
    pub choice: Choice,
    /// The probability of `choice`.
    pub confidence: f32,
    /// How many questions the query was asked as.
    pub questions: usize,
}

impl CandidateScores {
    /// Combines the answers to the questions a query of `candidates` candidates was asked as
    /// (see the module documentation).
    ///
    /// Each answer pairs the candidate indices a question listed with its [`Decision`], which
    /// must offer "none of these". The ranges must cover `0..candidates` in order without
    /// overlap, as [`CandidateQuery::questions`] returns them.
    pub fn from_answers(
        candidates: usize,
        answers: &[(Range<usize>, Decision)],
    ) -> Result<Self, DecisionError> {
        let mut next = 0;
        for (range, decision) in answers {
            if range.start != next || range.end <= range.start {
                return Err(DecisionError::InvalidInput(format!(
                    "answer ranges must cover the candidates in order, got {range:?} at {next}"
                )));
            }
            if decision.probabilities.len() != range.len() || decision.none_probability.is_none() {
                return Err(DecisionError::InvalidInput(format!(
                    "the answer for candidates {range:?} must score {} options and \"none of these\"",
                    range.len()
                )));
            }
            next = range.end;
        }
        if next != candidates || candidates == 0 {
            return Err(DecisionError::InvalidInput(format!(
                "answers cover {next} of {candidates} candidates"
            )));
        }

        // Work in log space, so products over many questions neither underflow nor divide by a
        // probability that rounded to zero.
        let ln = |p: f32| f64::from(p).max(f64::MIN_POSITIVE).ln();
        let ln_none_of = |d: &Decision| ln(d.none_probability.unwrap_or_default());
        let ln_all_none: f64 = answers.iter().map(|(_, d)| ln_none_of(d)).sum();
        let mut weights = Vec::with_capacity(candidates + 1);
        for (_, decision) in answers {
            let ln_others_none = ln_all_none - ln_none_of(decision);
            weights.extend(
                decision
                    .probabilities
                    .iter()
                    .map(|&p| ln(p) + ln_others_none),
            );
        }
        weights.push(ln_all_none);
        let max = weights.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let exp: Vec<f64> = weights.iter().map(|w| (w - max).exp()).collect();
        let sum: f64 = exp.iter().sum();
        let total: Vec<f64> = exp.into_iter().map(|e| e / sum).collect();

        let best = first_max(&total);
        Ok(Self {
            probabilities: total[..candidates].iter().map(|&p| p as f32).collect(),
            none_probability: total[candidates] as f32,
            choice: if best < candidates {
                Choice::Option(best)
            } else {
                Choice::NoneOfThese
            },
            confidence: total[best] as f32,
            questions: answers.len(),
        })
    }

    /// The probability of `choice`, or `None` for a candidate index out of range.
    pub fn probability(&self, choice: Choice) -> Option<f32> {
        match choice {
            Choice::Option(i) => self.probabilities.get(i).copied(),
            Choice::NoneOfThese => Some(self.none_probability),
        }
    }

    /// Candidate indices, most probable first. Equal probabilities keep candidate order.
    pub fn ranked(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.probabilities.len()).collect();
        order.sort_by(|&a, &b| self.probabilities[b].total_cmp(&self.probabilities[a]));
        order
    }

    /// The scores read against `min_confidence`: the most probable choice when its probability
    /// reaches the floor, and [`Verdict::Unsure`] otherwise. Candidates are named by index.
    pub fn verdict(&self, min_confidence: f32) -> Verdict<usize> {
        Verdict::read(self.choice, self.confidence, min_confidence, |i| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(n: usize) -> CandidateQuery {
        CandidateQuery::new("parse the config", (0..n).map(|i| format!("candidate {i}")))
    }

    fn decision(probabilities: &[f32], none: f32) -> Decision {
        let mut all: Vec<f64> = probabilities.iter().map(|&p| f64::from(p)).collect();
        all.push(f64::from(none));
        let best = first_max(&all);
        Decision {
            probabilities: probabilities.to_vec(),
            none_probability: Some(none),
            choice: if best < probabilities.len() {
                Choice::Option(best)
            } else {
                Choice::NoneOfThese
            },
            confidence: all[best] as f32,
        }
    }

    fn close(a: f32, b: f64) -> bool {
        (f64::from(a) - b).abs() < 1e-6
    }

    #[test]
    fn candidates_split_into_even_contiguous_questions() {
        let lens = |n: usize, per: usize| -> Vec<usize> {
            query(n)
                .questions(per)
                .unwrap()
                .iter()
                .map(|(r, _)| r.len())
                .collect()
        };
        assert_eq!(lens(5, 12), vec![5]);
        assert_eq!(lens(12, 12), vec![12]);
        assert_eq!(lens(13, 12), vec![7, 6]);
        assert_eq!(lens(24, 12), vec![12, 12]);
        assert_eq!(lens(50, 12), vec![10, 10, 10, 10, 10]);
        assert_eq!(lens(7, 3), vec![3, 2, 2]);
        assert_eq!(lens(3, 1), vec![1, 1, 1]);

        let questions = query(7).questions(3).unwrap();
        let ranges: Vec<Range<usize>> = questions.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, vec![0..3, 3..5, 5..7]);
        let (_, second) = &questions[1];
        assert_eq!(second.options, vec!["candidate 3", "candidate 4"]);
        assert_eq!(second.none_of_these.as_deref(), Some(NONE_OF_THESE));
        assert_eq!(
            second.prompt("Query: parse the config").unwrap(),
            "Query: parse the config\n\nQuestion: Which of these candidates matches the query?\n\
             a. candidate 3\nb. candidate 4\nc. none of these\nAnswer:"
        );
    }

    #[test]
    fn bad_queries_are_refused() {
        assert!(query(0).questions(12).is_err());
        assert!(query(3).questions(0).is_err());
        let two_line_question = CandidateQuery::about("s", "which\none?", ["x"]);
        assert!(two_line_question.questions(12).is_err());
        // 26 letters hold at most 25 candidates and "none of these".
        assert!(query(26).questions(26).is_err());
        assert!(query(26).questions(25).is_ok());
    }

    #[test]
    fn multi_line_candidates_become_one_line() {
        let q = CandidateQuery::new(
            "q",
            [
                "fn parse(\n    raw: &str,\n) -> Config",
                "plain",
                " spaced ",
            ],
        );
        let (_, question) = q.questions(12).unwrap().remove(0);
        assert_eq!(
            question.options,
            vec!["fn parse( raw: &str, ) -> Config", "plain", " spaced "]
        );
        assert!(question.validate().is_ok());
    }

    #[test]
    fn call_target_query_uses_the_trained_wording() {
        let q = CandidateQuery::call_target("site", "parse", ["src/a.rs: parse"]);
        let (_, question) = q.questions(12).unwrap().remove(0);
        assert_eq!(
            question,
            ChoiceQuestion::call_target("parse", vec!["src/a.rs: parse".into()])
        );
    }

    #[test]
    fn one_question_keeps_its_probabilities() {
        let d = decision(&[0.5, 0.2, 0.1], 0.2);
        let scores = CandidateScores::from_answers(3, &[(0..3, d.clone())]).unwrap();
        for (got, want) in scores.probabilities.iter().zip(&d.probabilities) {
            assert!(close(*got, f64::from(*want)));
        }
        assert!(close(scores.none_probability, 0.2));
        assert_eq!(scores.choice, Choice::Option(0));
        assert_eq!(scores.questions, 1);
    }

    #[test]
    fn several_questions_combine_as_independent_experts() {
        let first = decision(&[0.6, 0.1], 0.3);
        let second = decision(&[0.2], 0.8);
        let scores = CandidateScores::from_answers(3, &[(0..2, first), (2..3, second)]).unwrap();
        // Weights: candidate 0 = 0.6 * 0.8, 1 = 0.1 * 0.8, 2 = 0.2 * 0.3, none = 0.3 * 0.8.
        let w = [0.48, 0.08, 0.06, 0.24];
        let z: f64 = w.iter().sum();
        for (got, want) in scores.probabilities.iter().zip(&w) {
            assert!(close(*got, want / z), "{got} vs {}", want / z);
        }
        assert!(close(scores.none_probability, w[3] / z));
        let total: f32 = scores.probabilities.iter().sum::<f32>() + scores.none_probability;
        assert!((total - 1.0).abs() < 1e-6);
        assert_eq!(scores.choice, Choice::Option(0));
        assert_eq!(scores.ranked(), vec![0, 1, 2]);
        assert_eq!(scores.questions, 2);
    }

    #[test]
    fn none_wins_when_every_question_says_none() {
        let scores = CandidateScores::from_answers(
            4,
            &[
                (0..2, decision(&[0.05, 0.05], 0.9)),
                (2..4, decision(&[0.02, 0.08], 0.9)),
            ],
        )
        .unwrap();
        assert_eq!(scores.choice, Choice::NoneOfThese);
        assert_eq!(scores.verdict(0.5), Verdict::NoneOfThese);
    }

    #[test]
    fn a_certain_question_takes_all_the_mass() {
        // The second question is sure its candidate matches: its "none" rounded to zero.
        let scores = CandidateScores::from_answers(
            3,
            &[
                (0..2, decision(&[0.5, 0.3], 0.2)),
                (2..3, decision(&[1.0], 0.0)),
            ],
        )
        .unwrap();
        assert_eq!(scores.choice, Choice::Option(2));
        assert!(scores.confidence > 0.999_999);
        assert!(scores.probabilities.iter().all(|p| p.is_finite()));
        assert!(scores.none_probability.is_finite());
    }

    #[test]
    fn answers_must_cover_the_candidates_in_order() {
        let d2 = || decision(&[0.4, 0.4], 0.2);
        assert!(CandidateScores::from_answers(4, &[(0..2, d2())]).is_err());
        assert!(CandidateScores::from_answers(4, &[(2..4, d2()), (0..2, d2())]).is_err());
        assert!(CandidateScores::from_answers(3, &[(0..3, d2())]).is_err());
        assert!(CandidateScores::from_answers(0, &[]).is_err());
        let no_none = Decision {
            none_probability: None,
            ..d2()
        };
        assert!(CandidateScores::from_answers(2, &[(0..2, no_none)]).is_err());
    }

    #[test]
    fn ranking_and_verdicts() {
        let scores =
            CandidateScores::from_answers(4, &[(0..4, decision(&[0.1, 0.35, 0.1, 0.35], 0.1))])
                .unwrap();
        // Ties keep candidate order, and the earlier tied candidate is the choice.
        assert_eq!(scores.ranked(), vec![1, 3, 0, 2]);
        assert_eq!(scores.choice, Choice::Option(1));
        assert_eq!(scores.verdict(0.3), Verdict::Chosen(1));
        assert_eq!(scores.verdict(0.9), Verdict::Unsure);
        assert_eq!(scores.verdict(f32::NAN), Verdict::Unsure);
        assert_eq!(scores.verdict(0.0).chosen(), Some(1));
        assert_eq!(scores.probability(Choice::Option(9)), None);
    }
}
