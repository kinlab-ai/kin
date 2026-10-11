// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Questions whose options carry caller values.
//!
//! A [`TypedQuestion`] pairs each option text with a value of the caller's type, so an answer
//! comes back as that value rather than an index. [`TypedQuestion::yes_no`] asks a yes-or-no
//! question as the two options "yes" and "no".

use super::{ChoiceQuestion, Decision, DecisionError, Verdict};

/// A [`ChoiceQuestion`] whose options each carry a value of type `T`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypedQuestion<T> {
    question: ChoiceQuestion,
    values: Vec<T>,
}

impl<T> TypedQuestion<T> {
    /// A question over `options`, each a value and the text the model reads for it, that must
    /// be answered with one of them.
    pub fn new<S: Into<String>>(
        question: impl Into<String>,
        options: impl IntoIterator<Item = (T, S)>,
    ) -> Self {
        let (values, texts): (Vec<T>, Vec<String>) = options
            .into_iter()
            .map(|(value, text)| (value, text.into()))
            .unzip();
        Self {
            question: ChoiceQuestion::new(question, texts),
            values,
        }
    }

    /// Appends the plain "none of these" choice after the options.
    pub fn with_none_of_these(mut self) -> Self {
        self.question = self.question.with_none_of_these();
        self
    }

    /// Appends a "none of these" choice with a caller-chosen text after the options.
    pub fn with_none_text(mut self, text: impl Into<String>) -> Self {
        self.question = self.question.with_none_text(text);
        self
    }

    /// The question as the model is asked it.
    pub fn choice_question(&self) -> &ChoiceQuestion {
        &self.question
    }

    /// The option values, in the order the options are shown first.
    pub fn values(&self) -> &[T] {
        &self.values
    }

    /// Reads a decision on [`choice_question`](Self::choice_question) as an answer in values.
    pub fn answer(&self, decision: Decision) -> Result<TypedAnswer<'_, T>, DecisionError> {
        let offers_none = self.question.none_of_these.is_some();
        if decision.probabilities.len() != self.values.len()
            || decision.none_probability.is_some() != offers_none
        {
            return Err(DecisionError::InvalidInput(format!(
                "a decision over {} options does not answer this question of {}",
                decision.probabilities.len(),
                self.values.len()
            )));
        }
        Ok(TypedAnswer {
            decision,
            values: &self.values,
        })
    }
}

impl TypedQuestion<bool> {
    /// A yes-or-no question: option `a` is "yes" (`true`) and `b` is "no" (`false`). It has no
    /// "none of these"; read the answer through [`TypedAnswer::verdict`] to abstain when the
    /// model is unsure.
    pub fn yes_no(question: impl Into<String>) -> Self {
        Self::new(question, [(true, "yes"), (false, "no")])
    }
}

/// The answer to a [`TypedQuestion`], in the question's values.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedAnswer<'q, T> {
    decision: Decision,
    values: &'q [T],
}

impl<'q, T> TypedAnswer<'q, T> {
    /// The underlying decision, with probabilities in option order.
    pub fn decision(&self) -> &Decision {
        &self.decision
    }

    /// The most probable value, or `None` when "none of these" is most probable.
    pub fn value(&self) -> Option<&'q T> {
        match self.decision.choice {
            super::Choice::Option(i) => self.values.get(i),
            super::Choice::NoneOfThese => None,
        }
    }

    /// The probability of the most probable choice.
    pub fn confidence(&self) -> f32 {
        self.decision.confidence
    }

    /// Every value with its probability, in option order.
    pub fn probabilities(&self) -> impl Iterator<Item = (&'q T, f32)> + '_ {
        let values: &'q [T] = self.values;
        values
            .iter()
            .zip(self.decision.probabilities.iter().copied())
    }

    /// The probability of "none of these", when the question offers it.
    pub fn none_probability(&self) -> Option<f32> {
        self.decision.none_probability
    }

    /// The answer read against `min_confidence` (see [`Verdict`]).
    pub fn verdict(&self, min_confidence: f32) -> Verdict<&'q T> {
        let values: &'q [T] = self.values;
        Verdict::read(
            self.decision.choice,
            self.decision.confidence,
            min_confidence,
            |i| &values[i],
        )
    }
}

impl TypedAnswer<'_, bool> {
    /// The probability of every option whose value is `true`: for [`TypedQuestion::yes_no`],
    /// the probability of "yes".
    pub fn yes_probability(&self) -> f32 {
        self.probabilities()
            .filter(|(value, _)| **value)
            .map(|(_, p)| p)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::Choice;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Layer {
        Storage,
        Api,
        Cli,
    }

    fn layers() -> TypedQuestion<Layer> {
        TypedQuestion::new(
            "Which layer owns this function?",
            [
                (Layer::Storage, "the storage layer"),
                (Layer::Api, "the HTTP API"),
                (Layer::Cli, "the command line"),
            ],
        )
        .with_none_of_these()
    }

    #[test]
    fn a_typed_question_asks_its_option_texts() {
        let q = layers();
        assert_eq!(q.values(), &[Layer::Storage, Layer::Api, Layer::Cli]);
        assert_eq!(
            q.choice_question().prompt("fn save()").unwrap(),
            "fn save()\n\nQuestion: Which layer owns this function?\na. the storage layer\n\
             b. the HTTP API\nc. the command line\nd. none of these\nAnswer:"
        );
    }

    #[test]
    fn a_decision_reads_back_as_values() {
        let q = layers();
        let orders = q.choice_question().orders(false);
        let d = Decision::from_letter_logits(
            q.choice_question(),
            &orders,
            &[vec![0.0, 3.0, 0.5, 0.0]],
            1.0,
        )
        .unwrap();
        let answer = q.answer(d).unwrap();
        assert_eq!(answer.value(), Some(&Layer::Api));
        assert_eq!(answer.verdict(0.5), Verdict::Chosen(&Layer::Api));
        assert_eq!(answer.verdict(0.99), Verdict::Unsure);
        let listed: Vec<Layer> = answer.probabilities().map(|(v, _)| *v).collect();
        assert_eq!(listed, vec![Layer::Storage, Layer::Api, Layer::Cli]);
        assert!(answer.none_probability().is_some());

        let none = Decision::from_letter_logits(
            q.choice_question(),
            &orders,
            &[vec![0.0, 0.0, 0.0, 4.0]],
            1.0,
        )
        .unwrap();
        let answer = q.answer(none).unwrap();
        assert_eq!(answer.value(), None);
        assert_eq!(answer.verdict(0.5), Verdict::NoneOfThese);
    }

    #[test]
    fn a_decision_for_another_question_is_refused() {
        let q = layers();
        let other = ChoiceQuestion::new("q", vec!["x".into(), "y".into()]);
        let d = Decision::from_letter_logits(&other, &other.orders(false), &[vec![1.0, 0.0]], 1.0)
            .unwrap();
        assert!(q.answer(d).is_err());
    }

    #[test]
    fn yes_no_lists_yes_then_no() {
        let q = TypedQuestion::yes_no("Does this function write to disk?");
        assert_eq!(
            q.choice_question().prompt("fn save()").unwrap(),
            "fn save()\n\nQuestion: Does this function write to disk?\na. yes\nb. no\nAnswer:"
        );
        let orders = q.choice_question().orders(true);
        assert_eq!(orders, vec![vec![0, 1], vec![1, 0]]);
        // The reversed order lists "no" first, and the model prefers letter a in both orders.
        let d = Decision::from_letter_logits(
            q.choice_question(),
            &orders,
            &[vec![2.0, 0.0], vec![2.0, 0.0]],
            1.0,
        )
        .unwrap();
        let answer = q.answer(d).unwrap();
        assert!((answer.yes_probability() - 0.5).abs() < 1e-6);
        assert_eq!(answer.decision().choice, Choice::Option(0));
        assert_eq!(answer.verdict(0.6), Verdict::Unsure);
    }
}
