// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Verification graph types: test cases, assertions, and coverage tracking.
//!
//! Phase 9 introduces first-class graph objects for verification — test cases
//! anchored to semantic scopes, assertions about expected behavior, and
//! coverage summaries that track which entities have proof of correctness.

use crate::ids::{ContractId, EntityId, FilePathId, Hash256};
use crate::timestamp::Timestamp;
use crate::work::{WorkId, WorkScope};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// IDs
// ---------------------------------------------------------------------------

/// Unique identifier for a test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct TestId(pub Hash256);

impl TestId {
    pub fn new() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&bytes);
        Self(Hash256::from_bytes(buf))
    }

    pub fn from_hash(h: Hash256) -> Self {
        Self(h)
    }
}

impl Default for TestId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for an assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct AssertionId(pub Hash256);

impl AssertionId {
    pub fn new() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&bytes);
        Self(Hash256::from_bytes(buf))
    }
}

impl Default for AssertionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AssertionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a verification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct VerificationRunId(pub Hash256);

impl VerificationRunId {
    pub fn new() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&bytes);
        Self(Hash256::from_bytes(buf))
    }

    pub fn from_hash(h: Hash256) -> Self {
        Self(h)
    }
}

impl Default for VerificationRunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for VerificationRunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a mock hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct MockHintId(pub Hash256);

impl MockHintId {
    pub fn new() -> Self {
        let bytes = *uuid::Uuid::new_v4().as_bytes();
        let mut buf = [0u8; 32];
        buf[..16].copy_from_slice(&bytes);
        Self(Hash256::from_bytes(buf))
    }

    pub fn from_hash(h: Hash256) -> Self {
        Self(h)
    }
}

impl Default for MockHintId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MockHintId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TestKind {
    Unit,
    Integration,
    Contract,
    Property,
}

impl std::fmt::Display for TestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unit => write!(f, "unit"),
            Self::Integration => write!(f, "integration"),
            Self::Contract => write!(f, "contract"),
            Self::Property => write!(f, "property"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum TestRunner {
    Cargo,
    Jest,
    Pytest,
    Go,
    JUnit,
    Custom(String),
}

impl std::fmt::Display for TestRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cargo => write!(f, "cargo"),
            Self::Jest => write!(f, "jest"),
            Self::Pytest => write!(f, "pytest"),
            Self::Go => write!(f, "go"),
            Self::JUnit => write!(f, "junit"),
            Self::Custom(s) => write!(f, "{}", s),
        }
    }
}

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TestCase {
    pub test_id: TestId,
    pub name: String,
    pub language: String,
    pub kind: TestKind,
    pub scopes: Vec<WorkScope>,
    pub runner: TestRunner,
    pub file_origin: Option<FilePathId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Assertion {
    pub assertion_id: AssertionId,
    pub summary: String,
    pub expected_behavior: String,
    pub target_scope: WorkScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum VerificationStatus {
    Missing,
    /// No outcome yet. A recorded run is pending when its test command could
    /// not run: Kin could not start it, or the shell could not find or execute
    /// the runner it names. Such a run attests nothing about its tests, and
    /// its exit code and evidence say why it did not run.
    ///
    /// Recorded as this existing variant rather than a new one, so a store
    /// holding such a run still opens in a Kin that predates the reading.
    Pending,
    Passing,
    Failing,
}

impl std::fmt::Display for VerificationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing"),
            Self::Pending => write!(f, "pending"),
            Self::Passing => write!(f, "passing"),
            Self::Failing => write!(f, "failing"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub total_entities: usize,
    pub covered_entities: usize,
    pub coverage_ratio: f64,
    pub missing_proof: Vec<EntityId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CompletionState {
    Incomplete,
    Covered,
    Verified,
}

impl std::fmt::Display for CompletionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Incomplete => write!(f, "incomplete"),
            Self::Covered => write!(f, "covered"),
            Self::Verified => write!(f, "verified"),
        }
    }
}

/// Strategy used to mock a dependency during a test run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum MockStrategy {
    InMemory,
    Stub,
    Fake,
    Recorded,
    Custom(String),
}

impl std::fmt::Display for MockStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InMemory => write!(f, "in_memory"),
            Self::Stub => write!(f, "stub"),
            Self::Fake => write!(f, "fake"),
            Self::Recorded => write!(f, "recorded"),
            Self::Custom(s) => write!(f, "{}", s),
        }
    }
}

// ---------------------------------------------------------------------------
// Verification runs and mock hints (Phase 9 completion)
// ---------------------------------------------------------------------------

/// A single execution of one or more tests with captured evidence.
#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
pub struct VerificationRun {
    pub run_id: VerificationRunId,
    pub test_ids: Vec<TestId>,
    pub status: VerificationStatus,
    pub runner: TestRunner,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub duration_ms: Option<u64>,
    pub evidence_blob: Option<Hash256>,
    pub exit_code: Option<i32>,
    /// The entities the run was tied to, each with the fingerprint of its
    /// source when the run started. A result is current only while each of
    /// them still has that fingerprint. A run recorded before this was kept
    /// carries none, and is never current.
    ///
    /// Omitted only when empty and no proof targets follow it, so every run
    /// written before these fields existed keeps its exact bytes. The
    /// hand-written `Serialize` below decides the omission, because a
    /// field-level skip would drop this slot even when `proof` follows it.
    #[serde(default)]
    pub ran_against: Vec<RunFingerprint>,
    /// The proof links admitted with this run. The graph derives its links
    /// from these durable targets when a workspace is reopened.
    #[serde(default)]
    pub proof: Option<VerificationProof>,
}

impl VerificationRun {
    /// Whether this run's command could not run, so it attests no outcome for
    /// any test in it. See [`VerificationStatus::Pending`].
    pub fn could_not_run(&self) -> bool {
        self.status == VerificationStatus::Pending
    }
}

impl Serialize for VerificationRun {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        // MessagePack persists this as an array. A skipped interior field
        // would decode proof targets as fingerprints, so keep the tenth slot
        // whenever the eleventh is present.
        let fingerprints = !self.ran_against.is_empty() || self.proof.is_some();
        let mut out = serializer.serialize_struct(
            "VerificationRun",
            9 + usize::from(fingerprints) + usize::from(self.proof.is_some()),
        )?;
        out.serialize_field("run_id", &self.run_id)?;
        out.serialize_field("test_ids", &self.test_ids)?;
        out.serialize_field("status", &self.status)?;
        out.serialize_field("runner", &self.runner)?;
        out.serialize_field("started_at", &self.started_at)?;
        out.serialize_field("finished_at", &self.finished_at)?;
        out.serialize_field("duration_ms", &self.duration_ms)?;
        out.serialize_field("evidence_blob", &self.evidence_blob)?;
        out.serialize_field("exit_code", &self.exit_code)?;
        if fingerprints {
            out.serialize_field("ran_against", &self.ran_against)?;
        }
        if let Some(proof) = &self.proof {
            out.serialize_field("proof", proof)?;
        }
        out.end()
    }
}

/// The entities and work items a recorded run provides proof for. These
/// targets are distinct from its fingerprints, which also include tests and
/// dependencies that the run did not claim to prove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct VerificationProof {
    pub entities: Vec<EntityId>,
    pub work_items: Vec<WorkId>,
}

/// One entity a verification run was tied to, and the fingerprint of its
/// source when the run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct RunFingerprint {
    pub entity: EntityId,
    /// The entity's [`crate::SemanticFingerprint::behavior_hash`], which
    /// moves on any edit of its source.
    pub behavior_hash: Hash256,
}

/// Advisory hint that a dependency was mocked during a test run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MockHint {
    pub hint_id: MockHintId,
    pub test_id: TestId,
    pub dependency_scope: WorkScope,
    pub strategy: MockStrategy,
}

/// Contract-level coverage summary (how many contracts have test coverage).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractCoverageSummary {
    pub total_contracts: usize,
    pub covered_contracts: usize,
    pub coverage_ratio: f64,
    pub uncovered_contract_ids: Vec<ContractId>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ContractId, EntityId};

    #[test]
    fn test_id_display_not_empty() {
        let id = TestId::new();
        let s = id.to_string();
        assert!(!s.is_empty());
    }

    #[test]
    fn test_id_from_hash_roundtrip() {
        let h = Hash256::from_bytes([0xaa; 32]);
        let id = TestId::from_hash(h);
        assert_eq!(id.0, h);
    }

    #[test]
    fn assertion_id_display_not_empty() {
        let id = AssertionId::new();
        assert!(!id.to_string().is_empty());
    }

    #[test]
    fn test_kind_display() {
        assert_eq!(TestKind::Unit.to_string(), "unit");
        assert_eq!(TestKind::Integration.to_string(), "integration");
        assert_eq!(TestKind::Contract.to_string(), "contract");
        assert_eq!(TestKind::Property.to_string(), "property");
    }

    #[test]
    fn test_runner_display() {
        assert_eq!(TestRunner::Cargo.to_string(), "cargo");
        assert_eq!(TestRunner::Jest.to_string(), "jest");
        assert_eq!(TestRunner::Pytest.to_string(), "pytest");
        assert_eq!(TestRunner::Go.to_string(), "go");
        assert_eq!(TestRunner::JUnit.to_string(), "junit");
        assert_eq!(TestRunner::Custom("rspec".into()).to_string(), "rspec");
    }

    #[test]
    fn verification_status_display() {
        assert_eq!(VerificationStatus::Missing.to_string(), "missing");
        assert_eq!(VerificationStatus::Pending.to_string(), "pending");
        assert_eq!(VerificationStatus::Passing.to_string(), "passing");
        assert_eq!(VerificationStatus::Failing.to_string(), "failing");
    }

    #[test]
    fn completion_state_display() {
        assert_eq!(CompletionState::Incomplete.to_string(), "incomplete");
        assert_eq!(CompletionState::Covered.to_string(), "covered");
        assert_eq!(CompletionState::Verified.to_string(), "verified");
    }

    #[test]
    fn test_case_serialization_roundtrip() {
        let tc = TestCase {
            test_id: TestId::new(),
            name: "test_checkout_flow".into(),
            language: "rust".into(),
            kind: TestKind::Integration,
            scopes: vec![WorkScope::Entity(EntityId::new())],
            runner: TestRunner::Cargo,
            file_origin: Some(FilePathId::new("tests/checkout.rs")),
        };

        let json = serde_json::to_string(&tc).unwrap();
        let parsed: TestCase = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.test_id, tc.test_id);
        assert_eq!(parsed.name, tc.name);
        assert_eq!(parsed.kind, tc.kind);
        assert_eq!(parsed.runner, tc.runner);
    }

    #[test]
    fn assertion_serialization_roundtrip() {
        let a = Assertion {
            assertion_id: AssertionId::new(),
            summary: "Checkout must validate card".into(),
            expected_behavior: "Returns error on invalid card number".into(),
            target_scope: WorkScope::Entity(EntityId::new()),
        };

        let json = serde_json::to_string(&a).unwrap();
        let parsed: Assertion = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.assertion_id, a.assertion_id);
        assert_eq!(parsed.summary, a.summary);
    }

    #[test]
    fn coverage_summary_serialization_roundtrip() {
        let cs = CoverageSummary {
            total_entities: 100,
            covered_entities: 75,
            coverage_ratio: 0.75,
            missing_proof: vec![EntityId::new(), EntityId::new()],
        };

        let json = serde_json::to_string(&cs).unwrap();
        let parsed: CoverageSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_entities, 100);
        assert_eq!(parsed.covered_entities, 75);
        assert_eq!(parsed.missing_proof.len(), 2);
    }

    #[test]
    fn test_id_serialization_roundtrip() {
        let id = TestId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: TestId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn verification_status_serialization_roundtrip() {
        for status in [
            VerificationStatus::Missing,
            VerificationStatus::Pending,
            VerificationStatus::Passing,
            VerificationStatus::Failing,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let parsed: VerificationStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, parsed);
        }
    }

    // --- Phase 9 completion types ---

    #[test]
    fn verification_run_id_display_not_empty() {
        let id = VerificationRunId::new();
        assert!(!id.to_string().is_empty());
    }

    #[test]
    fn verification_run_id_serialization_roundtrip() {
        let id = VerificationRunId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: VerificationRunId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn mock_hint_id_display_not_empty() {
        let id = MockHintId::new();
        assert!(!id.to_string().is_empty());
    }

    #[test]
    fn mock_hint_id_serialization_roundtrip() {
        let id = MockHintId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: MockHintId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn mock_strategy_display() {
        assert_eq!(MockStrategy::InMemory.to_string(), "in_memory");
        assert_eq!(MockStrategy::Stub.to_string(), "stub");
        assert_eq!(MockStrategy::Fake.to_string(), "fake");
        assert_eq!(MockStrategy::Recorded.to_string(), "recorded");
        assert_eq!(
            MockStrategy::Custom("wiremock".into()).to_string(),
            "wiremock"
        );
    }

    #[test]
    fn mock_strategy_serialization_roundtrip() {
        for strategy in [
            MockStrategy::InMemory,
            MockStrategy::Stub,
            MockStrategy::Fake,
            MockStrategy::Recorded,
            MockStrategy::Custom("wiremock".into()),
        ] {
            let json = serde_json::to_string(&strategy).unwrap();
            let parsed: MockStrategy = serde_json::from_str(&json).unwrap();
            assert_eq!(strategy, parsed);
        }
    }

    #[test]
    fn verification_run_serialization_roundtrip() {
        use crate::timestamp::Timestamp;

        let run = VerificationRun {
            run_id: VerificationRunId::new(),
            test_ids: vec![TestId::new(), TestId::new()],
            status: VerificationStatus::Passing,
            runner: TestRunner::Cargo,
            started_at: Timestamp::now(),
            finished_at: Some(Timestamp::now()),
            duration_ms: Some(1234),
            evidence_blob: Some(Hash256::from_bytes([0xee; 32])),
            exit_code: Some(0),
            ran_against: Vec::new(),
            proof: None,
        };

        let json = serde_json::to_string(&run).unwrap();
        let parsed: VerificationRun = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.run_id, run.run_id);
        assert_eq!(parsed.test_ids.len(), 2);
        assert_eq!(parsed.status, VerificationStatus::Passing);
        assert_eq!(parsed.duration_ms, Some(1234));
        assert_eq!(parsed.exit_code, Some(0));
    }

    /// A run persists as a positional MessagePack array. One recorded before
    /// it carried fingerprints keeps its exact bytes and still decodes, and
    /// one that carries them keeps them.
    #[test]
    fn a_run_keeps_its_bytes_without_fingerprints_and_its_fingerprints_with_them() {
        use crate::timestamp::Timestamp;

        let without = VerificationRun {
            run_id: VerificationRunId::new(),
            test_ids: vec![TestId::new()],
            status: VerificationStatus::Failing,
            runner: TestRunner::Custom("npm test".into()),
            started_at: Timestamp::now(),
            finished_at: None,
            duration_ms: None,
            evidence_blob: None,
            exit_code: Some(1),
            ran_against: Vec::new(),
            proof: None,
        };
        let bytes = rmp_serde::to_vec(&without).unwrap();
        // Nine fields, as every run written before `ran_against` existed.
        assert_eq!(
            bytes[0], 0x99,
            "a run without fingerprints is nine elements"
        );
        let legacy = (
            without.run_id,
            &without.test_ids,
            without.status,
            &without.runner,
            &without.started_at,
            &without.finished_at,
            without.duration_ms,
            without.evidence_blob,
            without.exit_code,
        );
        assert_eq!(bytes, rmp_serde::to_vec(&legacy).unwrap());
        let decoded: VerificationRun = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded, without);

        let with = VerificationRun {
            ran_against: vec![RunFingerprint {
                entity: EntityId::new(),
                behavior_hash: Hash256::from_bytes([0x5a; 32]),
            }],
            ..without.clone()
        };
        let bytes = rmp_serde::to_vec(&with).unwrap();
        assert_eq!(bytes[0], 0x9a, "fingerprints are the tenth element");
        let decoded: VerificationRun = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded, with);

        let fingerprint_only = (
            with.run_id,
            &with.test_ids,
            with.status,
            &with.runner,
            &with.started_at,
            &with.finished_at,
            with.duration_ms,
            with.evidence_blob,
            with.exit_code,
            &with.ran_against,
        );
        assert_eq!(bytes, rmp_serde::to_vec(&fingerprint_only).unwrap());

        let targets = VerificationProof {
            entities: vec![EntityId::new()],
            work_items: vec![WorkId::new()],
        };
        for source in [without, with] {
            let proved = VerificationRun {
                proof: Some(targets.clone()),
                ..source
            };
            let bytes = rmp_serde::to_vec(&proved).unwrap();
            assert_eq!(bytes[0], 0x9b, "proof keeps the fingerprint slot");
            let decoded: VerificationRun = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(decoded, proved);
            let json = serde_json::to_string(&proved).unwrap();
            assert_eq!(
                serde_json::from_str::<VerificationRun>(&json).unwrap(),
                proved
            );
        }
    }

    /// A run whose command could not run is recorded with a status every
    /// released Kin already decodes, in the same positional layout as any
    /// other run, so a store that holds one still opens in an older Kin.
    #[test]
    fn a_run_that_could_not_run_keeps_the_released_encoding() {
        use crate::timestamp::Timestamp;

        let run = VerificationRun {
            run_id: VerificationRunId::new(),
            test_ids: vec![TestId::new(), TestId::new()],
            status: VerificationStatus::Pending,
            runner: TestRunner::Custom("npm test".into()),
            started_at: Timestamp::now(),
            finished_at: Some(Timestamp::now()),
            duration_ms: Some(395),
            evidence_blob: Some(Hash256::from_bytes([0x27; 32])),
            exit_code: Some(127),
            ran_against: vec![RunFingerprint {
                entity: EntityId::new(),
                behavior_hash: Hash256::from_bytes([0x5a; 32]),
            }],
            proof: Some(VerificationProof {
                entities: vec![EntityId::new()],
                work_items: Vec::new(),
            }),
        };
        assert!(run.could_not_run());
        for status in [
            VerificationStatus::Missing,
            VerificationStatus::Passing,
            VerificationStatus::Failing,
        ] {
            assert!(!VerificationRun {
                status,
                ..run.clone()
            }
            .could_not_run());
        }

        let bytes = rmp_serde::to_vec(&run).unwrap();
        assert_eq!(
            bytes[0], 0x9b,
            "eleven positional elements, as any run with proof"
        );
        let positional = (
            run.run_id,
            &run.test_ids,
            "Pending",
            &run.runner,
            &run.started_at,
            &run.finished_at,
            run.duration_ms,
            run.evidence_blob,
            run.exit_code,
            &run.ran_against,
            &run.proof,
        );
        assert_eq!(
            bytes,
            rmp_serde::to_vec(&positional).unwrap(),
            "the status is the released variant name, in the released slot"
        );
        let decoded: VerificationRun = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(decoded, run);
        assert!(decoded.could_not_run());
    }

    #[test]
    fn mock_hint_serialization_roundtrip() {
        let hint = MockHint {
            hint_id: MockHintId::new(),
            test_id: TestId::new(),
            dependency_scope: WorkScope::Entity(EntityId::new()),
            strategy: MockStrategy::InMemory,
        };

        let json = serde_json::to_string(&hint).unwrap();
        let parsed: MockHint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hint_id, hint.hint_id);
        assert_eq!(parsed.strategy, MockStrategy::InMemory);
    }

    #[test]
    fn contract_coverage_summary_serialization_roundtrip() {
        let cs = ContractCoverageSummary {
            total_contracts: 10,
            covered_contracts: 7,
            coverage_ratio: 0.7,
            uncovered_contract_ids: vec![ContractId::new()],
        };

        let json = serde_json::to_string(&cs).unwrap();
        let parsed: ContractCoverageSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.total_contracts, 10);
        assert_eq!(parsed.covered_contracts, 7);
        assert_eq!(parsed.uncovered_contract_ids.len(), 1);
    }
}
