// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

/// The commit phases one admission entered, read from their timing events.
fn phases_entered_by(work: impl FnOnce()) -> Vec<String> {
    use std::sync::Mutex;
    use tracing_subscriber::layer::SubscriberExt;

    struct PhaseLayer(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PhaseLayer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Phase<'a>(&'a mut Option<String>);
            impl tracing::field::Visit for Phase<'_> {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "phase" {
                        *self.0 = Some(value.to_string());
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut phase = None;
            event.record(&mut Phase(&mut phase));
            if let Some(phase) = phase {
                self.0.lock().unwrap().push(phase);
            }
        }
    }

    let phases = Arc::new(Mutex::new(Vec::new()));
    {
        let _capture = crate::capture_events_on_this_thread(
            tracing_subscriber::registry().with(PhaseLayer(Arc::clone(&phases))),
        );
        work();
    }
    let entered = std::mem::take(&mut *phases.lock().unwrap());
    entered
}

/// A delta carrying credential-looking files used to derive every changed
/// source and then be refused at publication, naming only the first such file.
/// A fast-forward that brought nine of them took nine complete derivations,
/// one approval at a time. The refusal now comes before any source is derived
/// and names every blocked file with what its approval must carry; once they
/// are approved, the same delta admits.
#[test]
#[serial_test::serial(commit_phase_capture)]
fn sensitive_additions_are_refused_before_derivation_and_named_together() {
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    std::fs::write(repo.path().join("store.py"), b"def store():\n    return 1\n").unwrap();
    // The daemon's own reconciler carries the traffic checker live adoption
    // requires.
    let mut reconciler = state.reconciler.blocking_write();
    exact_tree_admission_on_this_thread(
        &state,
        None,
        TreePublication::Standalone,
        Some(&mut reconciler),
    )
    .unwrap();
    let generation = authority_generation(&state);
    let tree = state.graph.resolved_tree();

    let password = b"def connect():\n    password = \"s3cret-notekeeper-value\"\n    return password\n";
    let key = b"not really a key, but the name alone is sensitive\n";
    std::fs::write(repo.path().join("search.py"), password).unwrap();
    std::fs::create_dir_all(repo.path().join("certs")).unwrap();
    std::fs::write(repo.path().join("certs/server.key"), key).unwrap();
    std::fs::write(
        repo.path().join("caller.py"),
        b"from store import store\n\ndef call():\n    return store()\n",
    )
    .unwrap();

    let mut refusal = None;
    let phases = phases_entered_by(|| {
        refusal = exact_tree_admission_on_this_thread(
            &state,
            None,
            TreePublication::Standalone,
            Some(&mut reconciler),
        )
        .err();
    });
    let refusal = refusal
        .expect("unapproved sensitive additions must be refused")
        .to_string();
    assert!(
        refusal.contains("untracked sensitive content at 2 paths"),
        "one refusal names every blocked file: {refusal}"
    );
    for (path, body, finding) in [
        ("certs/server.key", key.as_slice(), "SensitivePath"),
        ("search.py", password.as_slice(), "CredentialAssignment"),
    ] {
        assert!(
            refusal.contains(&format!(
                "{path} ({finding}, digest {}, blob)",
                Hash256(kin_blobs::digest(body).0)
            )),
            "the refusal names {path} with what its approval must carry: {refusal}"
        );
    }
    assert!(!refusal.contains("caller.py"), "{refusal}");
    assert!(
        phases.iter().any(|phase| phase == "scan_introduced_sensitive_content"),
        "the verdict is reached by the scan before derivation: {phases:?}"
    );
    for derivation in ["stage_live_graph", "prepare_changed_sources"] {
        assert!(
            !phases.iter().any(|phase| phase == derivation),
            "a refused delta derives nothing, but it entered {derivation}: {phases:?}"
        );
    }
    assert_eq!(authority_generation(&state), generation);
    assert_eq!(state.graph.resolved_tree(), tree);

    // The approvals the refusal asked for, in the tracked file `kin allow`
    // writes. The same delta then admits in one pass.
    let approval = |path: &str, body: &[u8]| {
        format!(
            "{path}\t{}\tblob\tfixture\tfixture bytes, not a credential\n",
            Hash256(kin_blobs::digest(body).0)
        )
    };
    std::fs::write(
        repo.path().join(".kin-allowances"),
        format!(
            "kin-allowances 1\n{}{}",
            approval("certs/server.key", key),
            approval("search.py", password)
        ),
    )
    .unwrap();
    exact_tree_admission_on_this_thread(
        &state,
        None,
        TreePublication::Standalone,
        Some(&mut reconciler),
    )
    .expect("the approved delta admits");
    assert_eq!(authority_generation(&state), generation + 1);
    for path in ["search.py", "certs/server.key", "caller.py"] {
        assert!(
            state
                .graph
                .resolved_tree()
                .artifact_at_path(&test_repo_path(path))
                .is_some(),
            "{path} is admitted"
        );
    }
}

/// One blocked file keeps the refusal it always had, word for word, and is
/// also reached before derivation.
#[test]
#[serial_test::serial(commit_phase_capture)]
fn a_single_sensitive_addition_keeps_its_refusal() {
    let repo = tempfile::tempdir().unwrap();
    let state = open_test_state(&repo);
    let password = b"def connect():\n    password = \"s3cret-notekeeper-value\"\n    return password\n";
    std::fs::write(repo.path().join("search.py"), password).unwrap();
    let mut reconciler = state.reconciler.blocking_write();
    let refusal = exact_tree_admission_on_this_thread(
        &state,
        None,
        TreePublication::Standalone,
        Some(&mut reconciler),
    )
    .expect_err("an unapproved sensitive addition must be refused")
    .to_string();
    let expected = kin_db::admission::SensitiveAdmissionError::Blocked {
        path: test_repo_path("search.py"),
        content_hash: Hash256(kin_blobs::digest(password).0),
        kind: kin_model::SensitiveArtifactKind::Blob { executable: false },
        finding: kin_db::admission::SensitiveFindingKind::CredentialAssignment,
    }
    .to_string();
    assert!(refusal.ends_with(&expected), "{refusal}");
}
