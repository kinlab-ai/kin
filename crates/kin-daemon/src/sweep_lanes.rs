// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! How a cold sweep spreads its files over several language-server instances.
//!
//! tsserver and pyright each answer on one thread, so a sweep that walks every
//! file of a large repository through one instance of each is bounded by that
//! thread however many cores the machine has: on a 4,399-file JavaScript and
//! TypeScript repository one tsserver answered about a thousand questions a
//! second and the sweep took an hour. A sweep runs lanes instead. Each lane
//! owns its own server instances and asks about its own files, and the lanes
//! run at once.
//!
//! What a lane proves about a file is what a single walk proves about it. Each
//! file is asked about by exactly one lane, on a server started the same way
//! and answering under the same proof context, and every graph write still
//! goes through the one coordination gate that orders them.
//!
//! Servers that answer on several threads of their own (rust-analyzer, gopls,
//! jdtls, clangd) and load their whole workspace per instance get one lane
//! each, which walks that language's files in path order. The others share a
//! pool of lanes that take contiguous runs of one language's files at a time,
//! in path order, so an instance stays inside one project for as long as it
//! can, and the runs shrink as a language's files run out so the lanes finish
//! together.
//!
//! Lanes are one of three parts of a single concurrency design, each answering
//! a different limit. Lanes add server processes, which is the only thing that
//! adds compute to a server that answers on one thread. Within a lane, the
//! in-flight window (`kin_lsp::concurrency`, `KIN_DAEMON_LSP_MAX_IN_FLIGHT`)
//! keeps a few of one file's questions queued at its server so the server is
//! never idle waiting on the daemon, and halves when the server falls behind;
//! for a server that answers on several threads it is the whole of the
//! concurrency. The pace (`kin_lsp::pace`) stretches each question's allowance
//! when the server is slow rather than counting its slowness as unanswered
//! questions. So the questions in flight to one language are at most lanes
//! times window, every one of them folded in the order a single walk asks it.

use std::collections::VecDeque;

use kin_model::{FilePathId, LanguageId};

/// The environment variable that sets how many lanes a sweep may run.
pub(crate) const LANES_ENV: &str = "KIN_DAEMON_LSP_SWEEP_LANES";

/// The most lanes a derived budget allows. Past this the daemon's own share
/// of each answer, and the graph writes every lane waits its turn for, take
/// back what another instance adds.
pub(crate) const MAX_DERIVED_LANES: usize = 8;

/// What one pooled instance is budgeted to hold. tsserver on a 4,399-file
/// repository held about 1.5 GB; the rest is headroom for a larger project.
const MEMORY_PER_LANE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// The fewest files that justify starting another pooled instance: starting
/// one costs a project load, which a handful of files does not repay. Eight
/// files of a Python repository take pyright about half a minute.
pub(crate) const MIN_FILES_PER_LANE: usize = 8;

/// The longest run of files a pooled lane takes at once.
const MAX_RUN: usize = 64;

/// How many lanes a sweep runs: [`LANES_ENV`] when it is set to a positive
/// number, and otherwise half the machine's cores, no more than its memory
/// budget allows at [`MEMORY_PER_LANE_BYTES`] each, between one and
/// [`MAX_DERIVED_LANES`].
pub(crate) fn lane_budget() -> usize {
    if let Some(lanes) = std::env::var(LANES_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|lanes| *lanes > 0)
    {
        return lanes;
    }
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let budget = kin_core::memory_pressure::FootprintBudget::resolve(
        kin_core::memory_pressure::ceiling_bytes(),
    )
    .map(|budget| budget.bytes);
    derived_lanes(cores, budget)
}

fn derived_lanes(cores: usize, budget_bytes: Option<u64>) -> usize {
    let by_memory = budget_bytes.map_or(1, |bytes| (bytes / MEMORY_PER_LANE_BYTES) as usize);
    (cores / 2).min(by_memory).clamp(1, MAX_DERIVED_LANES)
}

/// Whether several instances of this language's server may share its files.
/// These servers answer on one thread and hold only the projects their open
/// files belong to.
pub(crate) fn pooled(language: LanguageId) -> bool {
    matches!(
        language,
        LanguageId::TypeScript | LanguageId::JavaScript | LanguageId::Python
    )
}

/// What a lane walks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LaneKind {
    /// Every file, in path order, as a sweep with one lane always has.
    Serial,
    /// Every file of one language whose server is not pooled, in path order.
    Dedicated(LanguageId),
    /// Runs of pooled languages' files, taken from the shared queue.
    Pool,
}

/// One lane's starting point.
#[derive(Debug)]
pub(crate) struct LaneSeed {
    pub(crate) kind: LaneKind,
    /// The files it walks before it takes anything from the queue.
    pub(crate) files: Vec<FilePathId>,
}

/// The pooled languages' files not yet taken by a lane.
#[derive(Debug, Default)]
pub(crate) struct SweepQueue {
    /// Each pooled language's files in path order, languages in the order
    /// their first file sorts.
    languages: Vec<(LanguageId, VecDeque<FilePathId>)>,
    /// How many pooled lanes are working on each language now.
    working: Vec<usize>,
    pool_lanes: usize,
}

impl SweepQueue {
    /// Split `files` into lanes for a budget of `lanes`.
    ///
    /// One lane walks every file in path order, which is the order a sweep
    /// has always taken. More than one gives each language that is not pooled
    /// its own lane, and the pooled languages as many pooled lanes as the
    /// budget and their file count allow. Files of no enriched language go to
    /// the first lane, which counts them.
    pub(crate) fn plan(
        files: impl IntoIterator<Item = (FilePathId, Option<LanguageId>)>,
        lanes: usize,
    ) -> (Self, Vec<LaneSeed>) {
        let mut files: Vec<(FilePathId, Option<LanguageId>)> = files.into_iter().collect();
        files.sort_by(|left, right| left.0 .0.cmp(&right.0 .0));
        if lanes <= 1 {
            return (
                Self::default(),
                vec![LaneSeed {
                    kind: LaneKind::Serial,
                    files: files.into_iter().map(|(file, _)| file).collect(),
                }],
            );
        }
        let mut queue = Self::default();
        let mut dedicated: Vec<(LanguageId, Vec<FilePathId>)> = Vec::new();
        let mut unenriched = Vec::new();
        let mut pooled_files = 0usize;
        for (file, language) in files {
            match language {
                None => unenriched.push(file),
                Some(language) if pooled(language) => {
                    pooled_files += 1;
                    match queue
                        .languages
                        .iter_mut()
                        .find(|(held, _)| *held == language)
                    {
                        Some((_, held)) => held.push_back(file),
                        None => queue.languages.push((language, VecDeque::from([file]))),
                    }
                }
                Some(language) => match dedicated.iter_mut().find(|(held, _)| *held == language) {
                    Some((_, held)) => held.push(file),
                    None => dedicated.push((language, vec![file])),
                },
            }
        }
        queue.working = vec![0; queue.languages.len()];
        queue.pool_lanes = if pooled_files == 0 {
            0
        } else {
            lanes.min(pooled_files.div_ceil(MIN_FILES_PER_LANE)).max(1)
        };
        let mut seeds: Vec<LaneSeed> = (0..queue.pool_lanes)
            .map(|_| LaneSeed {
                kind: LaneKind::Pool,
                files: Vec::new(),
            })
            .collect();
        seeds.extend(dedicated.into_iter().map(|(language, files)| LaneSeed {
            kind: LaneKind::Dedicated(language),
            files,
        }));
        match seeds.first_mut() {
            Some(first) => {
                let rest = std::mem::take(&mut first.files);
                first.files = unenriched;
                first.files.extend(rest);
            }
            None if !unenriched.is_empty() => seeds.push(LaneSeed {
                kind: LaneKind::Serial,
                files: unenriched,
            }),
            None => {}
        }
        (queue, seeds)
    }

    /// The next run of files for a pooled lane working on `current`, and the
    /// language they are in: more of `current` while it lasts, so the lane's
    /// instance stays in the projects it has loaded, and otherwise the
    /// language with the most files left per lane already on it. `None` when
    /// every pooled file has been taken, and the lane stops counting as
    /// working on `current`.
    pub(crate) fn take(
        &mut self,
        current: Option<LanguageId>,
    ) -> Option<(LanguageId, Vec<FilePathId>)> {
        let current_at = current.and_then(|language| {
            self.languages
                .iter()
                .position(|(held, _)| *held == language)
        });
        let at = current_at
            .filter(|at| !self.languages[*at].1.is_empty())
            .or_else(|| {
                (0..self.languages.len())
                    .filter(|at| !self.languages[*at].1.is_empty())
                    // Most files left per lane already working on it; the
                    // earlier language on a tie.
                    .max_by(|left, right| {
                        let share = |at: usize| {
                            self.languages[at].1.len() as u128 * 1_000_000
                                / (self.working[at] as u128 + 1)
                        };
                        share(*left).cmp(&share(*right)).then(right.cmp(left))
                    })
            });
        if current_at != at {
            if let Some(previous) = current_at {
                self.working[previous] = self.working[previous].saturating_sub(1);
            }
            if let Some(next) = at {
                self.working[next] += 1;
            }
        }
        let at = at?;
        let (language, files) = &mut self.languages[at];
        let run = files
            .len()
            .div_ceil(2 * self.pool_lanes.max(1))
            .clamp(1, MAX_RUN);
        Some((*language, files.drain(..run.min(files.len())).collect()))
    }

    /// Whether any pooled file of `language` is still waiting for a lane.
    #[cfg(test)]
    fn holds(&self, language: LanguageId) -> bool {
        self.languages
            .iter()
            .any(|(held, files)| *held == language && !files.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FilePathId {
        FilePathId(path.into())
    }

    fn language(path: &str) -> Option<LanguageId> {
        match path.rsplit('.').next() {
            Some("ts") => Some(LanguageId::TypeScript),
            Some("js") => Some(LanguageId::JavaScript),
            Some("rs") => Some(LanguageId::Rust),
            Some("py") => Some(LanguageId::Python),
            _ => None,
        }
    }

    fn files(paths: &[&str]) -> Vec<(FilePathId, Option<LanguageId>)> {
        paths
            .iter()
            .map(|path| (file(path), language(path)))
            .collect()
    }

    #[test]
    fn one_lane_walks_every_file_in_path_order() {
        let (mut queue, seeds) = SweepQueue::plan(files(&["b.ts", "a.rs", "c.md", "a.js"]), 1);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].kind, LaneKind::Serial);
        assert_eq!(
            seeds[0].files,
            vec![file("a.js"), file("a.rs"), file("b.ts"), file("c.md")]
        );
        assert_eq!(queue.take(None), None);
    }

    #[test]
    fn servers_that_are_not_pooled_get_one_lane_each() {
        let many: Vec<String> = (0..200).map(|n| format!("src/{n:03}.js")).collect();
        let mut paths: Vec<&str> = many.iter().map(String::as_str).collect();
        paths.extend(["b.rs", "a.rs", "notes.md"]);
        let (queue, seeds) = SweepQueue::plan(files(&paths), 4);
        let kinds: Vec<&LaneKind> = seeds.iter().map(|seed| &seed.kind).collect();
        assert_eq!(
            kinds,
            vec![
                &LaneKind::Pool,
                &LaneKind::Pool,
                &LaneKind::Pool,
                &LaneKind::Pool,
                &LaneKind::Dedicated(LanguageId::Rust)
            ]
        );
        assert_eq!(seeds[0].files, vec![file("notes.md")]);
        assert_eq!(seeds[4].files, vec![file("a.rs"), file("b.rs")]);
        assert!(queue.holds(LanguageId::JavaScript));
    }

    #[test]
    fn a_small_repository_starts_no_more_instances_than_its_files_repay() {
        let many: Vec<String> = (0..12).map(|n| format!("{n:02}.py")).collect();
        let paths: Vec<&str> = many.iter().map(String::as_str).collect();
        let (_, seeds) = SweepQueue::plan(files(&paths), 8);
        assert_eq!(seeds.len(), 2);
        let (_, seeds) = SweepQueue::plan(files(&paths[..3]), 8);
        assert_eq!(seeds.len(), 1);
    }

    #[test]
    fn every_pooled_file_is_taken_once_in_contiguous_runs() {
        let js: Vec<String> = (0..300).map(|n| format!("a/{n:03}.js")).collect();
        let ts: Vec<String> = (0..60).map(|n| format!("b/{n:03}.ts")).collect();
        let mut paths: Vec<&str> = js.iter().map(String::as_str).collect();
        paths.extend(ts.iter().map(String::as_str));
        let (mut queue, seeds) = SweepQueue::plan(files(&paths), 4);
        assert_eq!(seeds.len(), 4);
        let mut current = [None; 4];
        let mut seen = Vec::new();
        let mut languages_first = Vec::new();
        'walk: loop {
            for lane in &mut current {
                match queue.take(*lane) {
                    Some((language, run)) => {
                        if lane.is_none() {
                            languages_first.push(language);
                        }
                        *lane = Some(language);
                        assert!(!run.is_empty() && run.len() <= MAX_RUN);
                        // A run is contiguous in path order.
                        assert!(run.windows(2).all(|pair| pair[0].0 < pair[1].0));
                        seen.extend(run);
                    }
                    None => break 'walk,
                }
            }
        }
        while let Some((_, run)) = queue.take(None) {
            seen.extend(run);
        }
        let mut expected: Vec<FilePathId> = paths.iter().map(|path| file(path)).collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        seen.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(seen, expected);
        // The larger language gets most of the lanes at the start, and the
        // smaller one is not left until the larger is done.
        assert!(languages_first.contains(&LanguageId::JavaScript));
        assert!(languages_first.contains(&LanguageId::TypeScript));
        assert!(!queue.holds(LanguageId::JavaScript));
        assert!(!queue.holds(LanguageId::TypeScript));
    }

    #[test]
    fn the_derived_budget_follows_cores_and_memory() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(derived_lanes(18, Some(32 * GIB)), 8);
        assert_eq!(derived_lanes(8, Some(8 * GIB)), 2);
        assert_eq!(derived_lanes(4, Some(32 * GIB)), 2);
        assert_eq!(derived_lanes(2, Some(2 * GIB)), 1);
        assert_eq!(derived_lanes(64, Some(64 * GIB)), MAX_DERIVED_LANES);
        assert_eq!(derived_lanes(16, None), 1);
    }
}
