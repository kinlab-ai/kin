// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The guides of committed changes this daemon built, kept for the next read.
//!
//! A guide of a recorded change is the most expensive read a daemon serves:
//! measured under load, one of zod's took 30 s and one of axum's 56 to 66 s.
//! It was built again for every request. The page gives a read a minute and
//! asks again, so a guide that took longer than that never reached it, each
//! attempt starting over; and Home and the review page each built the same
//! guide, one after the other.
//!
//! What a guide of committed history reads cannot move while three things
//! hold still: the change it names (and the base of a range), which never
//! moves once recorded; the graph it is read from, whose identity and count
//! of completed writes to truth say whether anything it could read moved;
//! and the repository's publication, which carries the refs a name resolves
//! through and the reviews the guide lists. So a guide is kept with the
//! [`GuideBasis`] it was read at, and served again only while that is still
//! the repository's basis. The guide's format is this build's own: what is
//! kept lives in this process and goes with it.
//!
//! A read of a change whose guide is being built joins that build rather than
//! starting another, and the build runs on a task of its own, so a request
//! that stops waiting does not take the work with it. A guide built while the
//! repository moved on is only handed to a request that asked after it
//! settled when it still describes the repository as that request finds it;
//! otherwise the request builds again. What a guide is answered with marks
//! each step with the agent session that wrote it, read fresh for every
//! request, so it is never kept.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::http::StatusCode;
use kin_cli::commands::review_guide::ReviewGuide;

use crate::api::LocalPublicationIdentity;

/// Guides kept at once, for the changes read most recently.
pub const KEPT_GUIDES_MAX: usize = 16;

/// What a guide of committed history was read against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GuideBasis {
    /// The graph the guide was read from: its identity, never reused, and how
    /// many writes to its truth it had completed.
    pub graph: (u64, u64),
    /// The repository publication it was read against, which holds its refs,
    /// its history and its reviews.
    pub publication: LocalPublicationIdentity,
}

impl GuideBasis {
    /// The basis `graph` stands at now, against `publication`.
    pub fn of(graph: &kin_db::InMemoryGraph, publication: LocalPublicationIdentity) -> Self {
        Self {
            graph: (graph.instance_id(), graph.truth_epoch()),
            publication,
        }
    }

    /// Whether a guide kept at this basis can never be served again from
    /// `now`: it was read from another graph, or from this one before writes
    /// that have since landed. Neither moves backwards.
    fn left_behind_by(&self, now: &GuideBasis) -> bool {
        self.graph.0 != now.graph.0 || self.graph.1 < now.graph.1
    }
}

/// One committed guide a request names: the change, and the base of a range,
/// as written, trimmed and in lower case.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GuideRequest {
    pub change: Option<String>,
    pub base: Option<String>,
}

impl GuideRequest {
    pub fn new(change: Option<&str>, base: Option<&str>) -> Self {
        let spelled = |name: Option<&str>| {
            name.map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
        };
        Self {
            change: spelled(change),
            base: spelled(base),
        }
    }
}

/// How one build ended: the guide and the basis it settled at, or the refusal
/// every request that waited for it is answered with.
pub type Built<T = ReviewGuide> = Result<(GuideBasis, Arc<T>), (StatusCode, String)>;

/// What a request finds for the guide it asks for.
pub enum Found<T = ReviewGuide> {
    /// A guide kept for the basis the request found.
    Kept(Arc<T>),
    /// A build already running: wait for its end.
    Join(tokio::sync::watch::Receiver<Option<Built<T>>>),
    /// Nothing: this request starts the build, and waits for it like any
    /// other.
    Lead(tokio::sync::watch::Sender<Option<Built<T>>>),
}

struct Entry<T> {
    building: Option<tokio::sync::watch::Receiver<Option<Built<T>>>>,
    kept: Option<(GuideBasis, Arc<T>)>,
    used: Option<Instant>,
}

impl<T> Default for Entry<T> {
    fn default() -> Self {
        Self {
            building: None,
            kept: None,
            used: None,
        }
    }
}

/// The committed guides this daemon is building and has kept.
pub struct CommittedGuides<T = ReviewGuide> {
    entries: Mutex<HashMap<GuideRequest, Entry<T>>>,
    builds: AtomicU64,
    kept_served: AtomicU64,
}

impl<T> Default for CommittedGuides<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            builds: AtomicU64::new(0),
            kept_served: AtomicU64::new(0),
        }
    }
}

impl<T> std::fmt::Debug for CommittedGuides<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommittedGuides")
            .field("builds", &self.builds())
            .field("kept_served", &self.kept_served())
            .finish_non_exhaustive()
    }
}

impl<T> CommittedGuides<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GuideRequest, Entry<T>>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What there is for `request` while the repository stands at `basis`.
    pub fn find(&self, request: &GuideRequest, basis: &GuideBasis) -> Found<T> {
        let mut entries = self.lock();
        for entry in entries.values_mut() {
            if entry
                .kept
                .as_ref()
                .is_some_and(|(kept, _)| kept.left_behind_by(basis))
            {
                entry.kept = None;
            }
            // A build whose task ended without finishing, as a panic ends
            // one, answers nobody; the next read builds again.
            if entry.building.as_ref().is_some_and(|building| {
                building.has_changed().is_err() && building.borrow().is_none()
            }) {
                entry.building = None;
            }
        }
        entries.retain(|_, entry| entry.building.is_some() || entry.kept.is_some());
        let entry = entries.entry(request.clone()).or_default();
        entry.used = Some(Instant::now());
        if let Some((kept, guide)) = &entry.kept {
            if kept == basis {
                self.kept_served.fetch_add(1, Ordering::Relaxed);
                return Found::Kept(Arc::clone(guide));
            }
        }
        if let Some(building) = &entry.building {
            return Found::Join(building.clone());
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        entry.building = Some(receiver);
        self.builds.fetch_add(1, Ordering::Relaxed);
        Found::Lead(sender)
    }

    /// End the build of `request`: keep what it built under the basis it
    /// settled at, and answer every request waiting for it.
    pub fn finish(
        &self,
        request: &GuideRequest,
        sender: tokio::sync::watch::Sender<Option<Built<T>>>,
        built: Built<T>,
    ) {
        {
            let mut entries = self.lock();
            let entry = entries.entry(request.clone()).or_default();
            entry.building = None;
            entry.used = Some(Instant::now());
            if let Ok((basis, guide)) = &built {
                entry.kept = Some((basis.clone(), Arc::clone(guide)));
            }
            if entries.len() > KEPT_GUIDES_MAX {
                let mut idle: Vec<(Option<Instant>, GuideRequest)> = entries
                    .iter()
                    .filter(|(_, entry)| entry.building.is_none())
                    .map(|(request, entry)| (entry.used, request.clone()))
                    .collect();
                idle.sort_by_key(|(used, _)| *used);
                let extra = entries.len() - KEPT_GUIDES_MAX;
                for (_, request) in idle.into_iter().take(extra) {
                    entries.remove(&request);
                }
            }
        }
        sender.send_replace(Some(built));
    }

    /// How many builds have started, for a test or a log line.
    pub fn builds(&self) -> u64 {
        self.builds.load(Ordering::Relaxed)
    }

    /// How many reads a kept guide answered.
    pub fn kept_served(&self) -> u64 {
        self.kept_served.load(Ordering::Relaxed)
    }
}

/// Wait for the build `receiver` follows to end.
pub async fn built<T>(
    mut receiver: tokio::sync::watch::Receiver<Option<Built<T>>>,
) -> Option<Built<T>> {
    receiver
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|built| built.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basis(truth: u64, publication: u8) -> GuideBasis {
        GuideBasis {
            graph: (7, truth),
            publication: LocalPublicationIdentity::Published([publication; 32]),
        }
    }

    fn guide() -> Arc<String> {
        Arc::new("a guide".to_string())
    }

    #[tokio::test]
    async fn a_guide_is_built_once_kept_for_its_basis_and_built_again_once_that_moves() {
        let guides = CommittedGuides::<String>::default();
        let request = GuideRequest::new(Some(" ABC "), None);
        assert_eq!(request, GuideRequest::new(Some("abc"), None));
        let Found::Lead(sender) = guides.find(&request, &basis(1, 1)) else {
            panic!("the first read builds");
        };
        let Found::Join(waiting) = guides.find(&request, &basis(1, 1)) else {
            panic!("a read during the build joins it");
        };
        guides.finish(&request, sender, Ok((basis(1, 1), guide())));
        assert!(built(waiting).await.unwrap().is_ok());
        assert!(matches!(
            guides.find(&request, &basis(1, 1)),
            Found::Kept(_)
        ));
        assert!(matches!(
            guides.find(&request, &basis(1, 2)),
            Found::Lead(_)
        ));
        assert_eq!(guides.builds(), 2);
        assert_eq!(guides.kept_served(), 1);
    }

    #[tokio::test]
    async fn a_build_whose_task_ended_without_an_answer_is_started_again() {
        let guides = CommittedGuides::<String>::default();
        let request = GuideRequest::new(Some("abc"), None);
        let Found::Lead(sender) = guides.find(&request, &basis(1, 1)) else {
            panic!()
        };
        let Found::Join(waiting) = guides.find(&request, &basis(1, 1)) else {
            panic!()
        };
        drop(sender);
        assert!(built(waiting).await.is_none());
        assert!(matches!(
            guides.find(&request, &basis(1, 1)),
            Found::Lead(_)
        ));
    }

    #[tokio::test]
    async fn a_refused_build_is_not_kept_and_a_moved_graph_drops_what_was() {
        let guides = CommittedGuides::<String>::default();
        let request = GuideRequest::new(Some("abc"), Some("def"));
        let Found::Lead(sender) = guides.find(&request, &basis(1, 1)) else {
            panic!()
        };
        guides.finish(
            &request,
            sender,
            Err((StatusCode::SERVICE_UNAVAILABLE, "busy".into())),
        );
        let Found::Lead(sender) = guides.find(&request, &basis(1, 1)) else {
            panic!("a refusal is not kept")
        };
        guides.finish(&request, sender, Ok((basis(1, 1), guide())));
        let other = GuideRequest::new(Some("xyz"), None);
        let Found::Lead(_) = guides.find(&other, &basis(2, 1)) else {
            panic!()
        };
        assert!(
            !matches!(guides.find(&request, &basis(1, 1)), Found::Kept(_)),
            "a guide of a graph that has since been written is never served"
        );
    }
}
