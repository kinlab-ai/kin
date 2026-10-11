// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Bounded retries inside one enrichment pass. A file still unanswered after
//! these attempts remains owed; its persisted backoff decides the next pass.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use kin_model::{FilePathId, LanguageId};

const RETRIES_PER_FILE: u32 = 1;
const USES_TYPE_RETRIES_PER_FILE: u32 = 2;
/// A live server that ran questions' allowances out is asked again this many
/// times in the pass, each time after the rest of the pass and with the
/// allowances its pace has stretched since (see `kin_lsp::pace`), before the
/// file is left owed to a later pass.
const TIMEOUT_RETRIES_PER_FILE: u32 = 3;
const CRASHES_WITHOUT_COMPLETING_A_FILE: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    Queued,
    Owed,
    LanguageUnavailable,
}

pub(crate) struct SweepFiles {
    pending: VecDeque<FilePathId>,
    retries: HashMap<FilePathId, u32>,
    /// Fresh-server retries, counted apart from `retries`: a file whose turn
    /// ended on a server that failed is asked again on its replacement even
    /// when it already spent its retries on a slow server, because a fresh
    /// server is a different question from a slow one.
    server_retries: HashMap<FilePathId, u32>,
    crashes: HashMap<LanguageId, usize>,
    /// Files asked once more because their reference sites stayed undecided.
    confirmed: HashSet<FilePathId>,
    /// Files a live but overloaded server is asked about again only once
    /// everything else this walk will be given is behind it. `None` when the
    /// walk holds every file it will ever be given in `pending`, where the
    /// back of `pending` is that place already.
    deferred: Option<VecDeque<FilePathId>>,
}

impl SweepFiles {
    pub(crate) fn new(files: impl IntoIterator<Item = FilePathId>) -> Self {
        let mut files: Vec<_> = files.into_iter().collect();
        // Hash-map insertion order must not decide which files get a warm
        // server, or which ones a stopped pass has already finished.
        files.sort_by(|left, right| left.0.cmp(&right.0));
        Self {
            pending: files.into(),
            retries: HashMap::new(),
            server_retries: HashMap::new(),
            crashes: HashMap::new(),
            confirmed: HashSet::new(),
            deferred: None,
        }
    }

    /// A walk that is handed its files in runs as it goes (see
    /// [`crate::sweep_lanes`]), starting with `files` in the order given.
    pub(crate) fn taking_runs(files: impl IntoIterator<Item = FilePathId>) -> Self {
        Self {
            pending: files.into_iter().collect(),
            retries: HashMap::new(),
            server_retries: HashMap::new(),
            crashes: HashMap::new(),
            confirmed: HashSet::new(),
            deferred: Some(VecDeque::new()),
        }
    }

    /// Append a run of files, in the order given.
    pub(crate) fn extend(&mut self, files: impl IntoIterator<Item = FilePathId>) {
        self.pending.extend(files);
    }

    pub(crate) fn next(&mut self) -> Option<FilePathId> {
        self.pending.pop_front()
    }

    /// The next file an overloaded server was left to recover from, once a
    /// walk taking runs has no run left to take.
    pub(crate) fn next_deferred(&mut self) -> Option<FilePathId> {
        self.deferred.as_mut()?.pop_front()
    }

    pub(crate) fn is_retry(&self, file: &FilePathId) -> bool {
        self.retry_count(file) > 0
    }

    pub(crate) fn retry_count(&self, file: &FilePathId) -> u32 {
        self.retries.get(file).copied().unwrap_or(0)
    }

    pub(crate) fn completed(&mut self, language: LanguageId) {
        // A large repository may need several server lifetimes. Only deaths
        // without completing a file spend the language's restart allowance.
        self.crashes.remove(&language);
    }

    /// A file whose questions ran their allowances out on a live server: it
    /// is asked again after everything else this walk holds, up to
    /// [`TIMEOUT_RETRIES_PER_FILE`] times, and then owed.
    pub(crate) fn timed_out(&mut self, file: &FilePathId) -> Retry {
        let retries = self.retries.entry(file.clone()).or_default();
        if *retries >= TIMEOUT_RETRIES_PER_FILE {
            return Retry::Owed;
        }
        *retries += 1;
        match &mut self.deferred {
            Some(deferred) => deferred.push_back(file.clone()),
            None => self.pending.push_back(file.clone()),
        }
        Retry::Queued
    }

    /// A file whose reference sites a finished pass left undecided, in a way
    /// a retry could not change: it is asked once more after everything else
    /// this walk holds, and that pass is the last the bound allows over the
    /// same inputs (see [`crate::owed_enrichment::REFERENCE_SETTLE_AFTER_PASSES`]),
    /// so it records whatever is still undecided as such. Asked once only:
    /// a second request is [`Retry::Owed`].
    pub(crate) fn confirm_undecided(&mut self, file: &FilePathId) -> Retry {
        if !self.confirmed.insert(file.clone()) {
            return Retry::Owed;
        }
        match &mut self.deferred {
            Some(deferred) => deferred.push_back(file.clone()),
            None => self.pending.push_back(file.clone()),
        }
        Retry::Queued
    }

    pub(crate) fn failed(
        &mut self,
        file: &FilePathId,
        language: LanguageId,
        server_died: bool,
        uses_type_timeout_budget: Option<Duration>,
    ) -> Retry {
        if server_died {
            let crashes = self.crashes.entry(language).or_default();
            *crashes += 1;
            if *crashes >= CRASHES_WITHOUT_COMPLETING_A_FILE {
                return Retry::LanguageUnavailable;
            }
        }
        if server_died {
            // The caller retired this server. Retry the interrupted file on
            // its replacement before walking any other file, once, whatever
            // retries a slow server already cost it.
            let retries = self.server_retries.entry(file.clone()).or_default();
            if *retries >= RETRIES_PER_FILE {
                return Retry::Owed;
            }
            *retries += 1;
            self.pending.push_front(file.clone());
            return Retry::Queued;
        }
        // Only an actual aggregate uses-type timeout earns the remaining
        // larger allowance. A dead server and other failed arms retain their
        // original retry limits. The maximum allowance is tried once, even
        // when persisted debt started this sweep at that allowance.
        let limit = match uses_type_timeout_budget {
            Some(budget) if budget < crate::owed_enrichment::USES_TYPE_MAX_BUDGET => {
                USES_TYPE_RETRIES_PER_FILE
            }
            Some(_) => return Retry::Owed,
            None => RETRIES_PER_FILE,
        };
        let retries = self.retries.entry(file.clone()).or_default();
        if *retries >= limit {
            return Retry::Owed;
        }
        *retries += 1;
        // Give a live but overloaded server the rest of the pass before
        // asking the same file again.
        match &mut self.deferred {
            Some(deferred) => deferred.push_back(file.clone()),
            None => self.pending.push_back(file.clone()),
        }
        Retry::Queued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FilePathId {
        FilePathId(path.into())
    }

    #[test]
    fn a_dead_server_retries_its_file_before_advancing() {
        let mut files = SweepFiles::new([file("b.ts"), file("a.ts")]);
        let a = files.next().unwrap();
        assert_eq!(a, file("a.ts"));
        assert_eq!(
            files.failed(&a, LanguageId::TypeScript, true, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(a.clone()));
        files.completed(LanguageId::TypeScript);
        assert_eq!(files.next(), Some(file("b.ts")));
        assert_eq!(files.next(), None);
    }

    #[test]
    fn a_timeout_waits_for_other_files_and_never_loops_on_one_file() {
        let mut files = SweepFiles::new([file("b.py"), file("a.py")]);
        let a = files.next().unwrap();
        assert_eq!(
            files.failed(&a, LanguageId::Python, false, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(file("b.py")));
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(
            files.failed(&a, LanguageId::Python, false, None),
            Retry::Owed
        );
        assert_eq!(files.next(), None);
    }

    #[test]
    fn a_uses_type_timeout_does_not_extend_a_dead_servers_retry_allowance() {
        let mut files = SweepFiles::new([file("a.py")]);
        let a = files.next().unwrap();
        assert_eq!(
            files.failed(&a, LanguageId::Python, true, Some(Duration::from_secs(5))),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(
            files.failed(&a, LanguageId::Python, true, Some(Duration::from_secs(15))),
            Retry::Owed
        );
        assert_eq!(files.next(), None);
    }

    #[test]
    fn poison_files_bound_restarts_without_blocking_other_languages() {
        let mut files = SweepFiles::new([file("a.ts"), file("b.ts"), file("c.py")]);
        let a = files.next().unwrap();
        assert_eq!(
            files.failed(&a, LanguageId::TypeScript, true, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(
            files.failed(&a, LanguageId::TypeScript, true, None),
            Retry::Owed
        );
        let b = files.next().unwrap();
        assert_eq!(
            files.failed(&b, LanguageId::TypeScript, true, None),
            Retry::LanguageUnavailable
        );
        let c = files.next().unwrap();
        assert_eq!(
            files.failed(&c, LanguageId::Python, true, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(c));
    }

    #[test]
    fn a_walk_taking_runs_asks_an_overloaded_server_again_after_every_run() {
        let mut files = SweepFiles::taking_runs([file("a.ts"), file("b.ts")]);
        let a = files.next().unwrap();
        assert_eq!(
            files.failed(&a, LanguageId::TypeScript, false, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(file("b.ts")));
        assert_eq!(files.next(), None);
        files.extend([file("c.ts")]);
        assert_eq!(files.next(), Some(file("c.ts")));
        assert_eq!(files.next(), None);
        assert_eq!(files.next_deferred(), Some(a.clone()));
        assert!(files.is_retry(&a));
        assert_eq!(files.next_deferred(), None);
        // A dead server's file is still asked again before anything else.
        files.extend([file("d.ts")]);
        let d = files.next().unwrap();
        assert_eq!(
            files.failed(&d, LanguageId::TypeScript, true, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(d));
    }

    #[test]
    fn a_file_that_timed_out_is_asked_again_after_the_rest_of_the_walk() {
        let mut files = SweepFiles::new([file("a.py"), file("b.py")]);
        let a = files.next().unwrap();
        assert_eq!(files.timed_out(&a), Retry::Queued);
        assert_eq!(files.next(), Some(file("b.py")));
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(files.timed_out(&a), Retry::Queued);
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(files.timed_out(&a), Retry::Queued);
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(files.timed_out(&a), Retry::Owed);
        assert_eq!(files.next(), None);
    }

    #[test]
    fn an_undecided_file_is_asked_once_more_after_the_rest_of_the_walk() {
        let mut files = SweepFiles::new([file("a.ts"), file("b.ts")]);
        let a = files.next().unwrap();
        assert_eq!(files.confirm_undecided(&a), Retry::Queued);
        assert_eq!(files.next(), Some(file("b.ts")));
        assert_eq!(files.next(), Some(a.clone()));
        assert_eq!(files.confirm_undecided(&a), Retry::Owed);
        assert_eq!(files.next(), None);

        let mut runs = SweepFiles::taking_runs([file("c.ts")]);
        let c = runs.next().unwrap();
        assert_eq!(runs.confirm_undecided(&c), Retry::Queued);
        assert_eq!(runs.next(), None);
        assert_eq!(runs.next_deferred(), Some(c));
    }

    /// On React a file spent its retry on a slow server, then got "Could not
    /// find source file" from a server whose state had failed, and was owed to
    /// a pass fifteen minutes later that answered it on a fresh server in ten
    /// seconds. A server failure now earns its own retry on the replacement,
    /// at once, whatever a slow server already cost the file, and only one.
    #[test]
    fn a_file_slowed_once_still_gets_a_fresh_server_after_a_failure() {
        let mut files = SweepFiles::new([file("a.js"), file("b.js")]);
        let a = files.next().unwrap();
        assert_eq!(
            files.failed(
                &a,
                LanguageId::JavaScript,
                false,
                Some(Duration::from_secs(5))
            ),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(file("b.js")));
        files.completed(LanguageId::JavaScript);
        assert_eq!(files.next(), Some(a.clone()));
        // Its retry ran on a server whose state failed: asked again at once.
        assert_eq!(
            files.failed(&a, LanguageId::JavaScript, true, None),
            Retry::Queued
        );
        assert_eq!(files.next(), Some(a.clone()));
        // A second failure on the replacement too: owed, not a loop.
        assert_eq!(
            files.failed(&a, LanguageId::JavaScript, true, None),
            Retry::Owed
        );
        assert_eq!(files.next(), None);
    }

    #[test]
    fn completed_files_allow_a_large_graph_to_use_more_server_lifetimes() {
        let mut files = SweepFiles::new((0..10).map(|n| file(&format!("{n:02}.ts"))));
        for n in 0..10 {
            let current = files.next().unwrap();
            assert_eq!(current, file(&format!("{n:02}.ts")));
            assert_eq!(
                files.failed(&current, LanguageId::TypeScript, true, None),
                Retry::Queued
            );
            assert_eq!(files.next(), Some(current));
            files.completed(LanguageId::TypeScript);
        }
        assert_eq!(files.next(), None);
    }
}
