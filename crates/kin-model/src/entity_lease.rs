// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Receiver-local coordination for parsed entity body edits. A lease never grants
//! repository access, replaces source compare-and-swap, or locks native files.

use crate::{EntityId, Hash256, SessionId, WorkspaceId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Workspace and entity identities are resolved against one repository authority.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLeaseResource {
    pub workspace_id: WorkspaceId,
    pub entity_id: EntityId,
}

/// A claim is meaningful only with the caller's independently verified session.
/// The fence is a decimal string on the agent wire, avoiding JSON integer loss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLeaseClaim {
    pub authority_id: uuid::Uuid,
    pub resource: EntityEditLeaseResource,
    pub fence: String,
}

impl EntityEditLeaseClaim {
    pub fn checked_fence(&self) -> Result<u64, String> {
        if self.authority_id.is_nil() {
            return Err("invalid entity lease authority".into());
        }
        let number = self
            .fence
            .parse::<u64>()
            .map_err(|_| "invalid entity lease fence")?;
        if number == 0 || number.to_string() != self.fence {
            return Err("invalid entity lease fence".into());
        }
        Ok(number)
    }
}

/// An explicitly requested coordination transition. The authenticated caller,
/// repository identity and server time are supplied by the writer, never here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntityEditLeaseMutation {
    Acquire {
        resource: EntityEditLeaseResource,
        expected_source_hash: Hash256,
    },
    Renew {
        claim: EntityEditLeaseClaim,
    },
    Release {
        claim: EntityEditLeaseClaim,
    },
}

impl EntityEditLeaseMutation {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Acquire { resource, .. } => validate_resource(resource),
            Self::Renew { claim } | Self::Release { claim } => {
                validate_resource(&claim.resource)?;
                claim.checked_fence().map(|_| ())
            }
        }
    }
}

/// Durable coordination state. Only live entries are retained; the high-water
/// mark survives pruning so reacquiring an unchanged entity never reuses a fence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLeaseLedger {
    pub authority_id: Option<uuid::Uuid>,
    pub last_fence: u64,
    pub entries: Vec<EntityEditLease>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLease {
    pub authority_id: uuid::Uuid,
    pub resource: EntityEditLeaseResource,
    pub session_id: SessionId,
    pub fence: u64,
    pub source_hash: Hash256,
    pub expires_at_ms: i64,
}

/// Trusted writer context. Agent-facing requests contain only mutation/claims;
/// the daemon constructs this after authenticating its transport session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLeaseContext {
    pub session_id: SessionId,
    pub mutation: Option<EntityEditLeaseMutation>,
    pub claims: Vec<EntityEditLeaseClaim>,
}

/// Exact trusted context and original issued lease retained in the operation
/// log. Replaying a control returns this outcome and never renews its expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EntityEditLeaseEvent {
    pub context: EntityEditLeaseContext,
    pub lease: Option<EntityEditLease>,
}

impl EntityEditLeaseEvent {
    pub fn validate(&self) -> Result<(), String> {
        self.context.validate()?;
        if let Some(lease) = &self.lease {
            validate_resource(&lease.resource)?;
            if lease.session_id != self.context.session_id
                || lease.authority_id.is_nil()
                || lease.fence == 0
                || lease.expires_at_ms <= 0
            {
                return Err("invalid entity lease audit outcome".into());
            }
        }
        let valid = match (&self.context.mutation, &self.lease) {
            (
                Some(EntityEditLeaseMutation::Acquire {
                    resource,
                    expected_source_hash,
                }),
                Some(lease),
            ) => *resource == lease.resource && *expected_source_hash == lease.source_hash,
            (Some(EntityEditLeaseMutation::Renew { claim }), Some(lease)) => {
                claim.resource == lease.resource
                    && claim.authority_id == lease.authority_id
                    && claim.checked_fence()? == lease.fence
            }
            (Some(EntityEditLeaseMutation::Release { .. }), None) => true,
            (None, None) => !self.context.claims.is_empty(),
            _ => false,
        };
        if !valid {
            return Err("entity lease audit outcome does not match its operation".into());
        }
        Ok(())
    }
}

impl EntityEditLeaseContext {
    pub fn validate(&self) -> Result<(), String> {
        if self.session_id.0.is_nil() {
            return Err("entity lease requires an authenticated session".into());
        }
        if self.claims.len() > MAX_ENTITY_EDIT_LEASE_CLAIMS {
            return Err("too many entity lease claims".into());
        }
        let mut resources = std::collections::BTreeSet::new();
        for claim in &self.claims {
            claim.checked_fence()?;
            validate_resource(&claim.resource)?;
            if !resources.insert(claim.resource) {
                return Err("duplicate entity lease claim".into());
            }
        }
        if self.mutation.is_some() && !self.claims.is_empty() {
            return Err("lease control and body editing require separate operations".into());
        }
        match &self.mutation {
            Some(EntityEditLeaseMutation::Acquire { resource, .. }) => validate_resource(resource)?,
            Some(
                EntityEditLeaseMutation::Renew { claim }
                | EntityEditLeaseMutation::Release { claim },
            ) => {
                validate_resource(&claim.resource)?;
                claim.checked_fence()?;
            }
            None => {}
        }
        Ok(())
    }
}

fn validate_resource(resource: &EntityEditLeaseResource) -> Result<(), String> {
    if resource.workspace_id.0.is_nil() || resource.entity_id.0.is_nil() {
        return Err("entity lease requires non-nil workspace and entity identities".into());
    }
    Ok(())
}

/// Refuse file/document fallback entities and derived candidates without an
/// independent parsed source declaration. This is coordination over bodies,
/// never a route to whole-file source knowledge.
pub fn supports_entity_edit_lease(entity: &crate::Entity) -> bool {
    use crate::EntityKind::*;
    crate::entity_derivation(entity).is_ok_and(|derivation| derivation.is_none())
        && entity.file_origin.is_some()
        && entity.span.is_some()
        && matches!(
            entity.kind,
            Function
                | Class
                | Interface
                | TraitDef
                | TypeAlias
                | Test
                | Method
                | EnumDef
                | EnumVariant
                | Constant
                | StaticVar
                | Macro
                | Field
        )
}

impl EntityEditLeaseLedger {
    pub fn validate(&self) -> Result<(), String> {
        if (self.last_fence == 0) != self.authority_id.is_none()
            || self.authority_id.is_some_and(|id| id.is_nil())
        {
            return Err("invalid durable entity lease authority watermark".into());
        }
        if self.entries.len() > MAX_ENTITY_EDIT_LEASES {
            return Err("too many durable entity leases".into());
        }
        let mut previous = None;
        for entry in &self.entries {
            validate_resource(&entry.resource)?;
            if entry.session_id.0.is_nil()
                || Some(entry.authority_id) != self.authority_id
                || entry.fence == 0
                || entry.fence > self.last_fence
            {
                return Err("invalid durable entity lease identity".into());
            }
            if previous.is_some_and(|resource| resource >= entry.resource) {
                return Err("entity leases must be unique and canonically ordered".into());
            }
            previous = Some(entry.resource);
        }
        Ok(())
    }

    /// Apply to a candidate ledger, not published state. `source_hash` is
    /// resolved by the authority from this exact workspace's parsed entity.
    pub fn apply_control(
        &mut self,
        context: &EntityEditLeaseContext,
        source_hash: Hash256,
        now_ms: i64,
    ) -> Result<Option<EntityEditLease>, String> {
        context.validate()?;
        self.validate()?;
        let mutation = context
            .mutation
            .as_ref()
            .ok_or("missing entity lease operation")?;
        let resource = match mutation {
            EntityEditLeaseMutation::Acquire { resource, .. } => *resource,
            EntityEditLeaseMutation::Renew { claim }
            | EntityEditLeaseMutation::Release { claim } => claim.resource,
        };
        let live = self
            .entries
            .iter()
            .find(|entry| entry.resource == resource && entry.expires_at_ms > now_ms);
        let next = match mutation {
            EntityEditLeaseMutation::Acquire {
                expected_source_hash,
                ..
            } => {
                if *expected_source_hash != source_hash {
                    return Err("ENTITY_LEASE_SOURCE_CHANGED".into());
                }
                if live.is_some() {
                    return Err("ENTITY_LEASE_HELD".into());
                }
                if self
                    .entries
                    .iter()
                    .filter(|entry| entry.expires_at_ms > now_ms)
                    .count()
                    >= MAX_ENTITY_EDIT_LEASES
                {
                    return Err("ENTITY_LEASE_LIMIT".into());
                }
                let fence = self
                    .last_fence
                    .checked_add(1)
                    .ok_or("ENTITY_LEASE_FENCE_EXHAUSTED")?;
                Some(EntityEditLease {
                    authority_id: self.authority_id.unwrap_or_else(uuid::Uuid::new_v4),
                    resource,
                    session_id: context.session_id,
                    fence,
                    source_hash,
                    expires_at_ms: now_ms
                        .checked_add(i64::from(ENTITY_EDIT_LEASE_SECONDS) * 1000)
                        .ok_or("invalid lease clock")?,
                })
            }
            EntityEditLeaseMutation::Renew { claim }
            | EntityEditLeaseMutation::Release { claim } => {
                let entry = live.ok_or("ENTITY_LEASE_STALE")?;
                if entry.session_id != context.session_id
                    || entry.authority_id != claim.authority_id
                    || entry.fence != claim.checked_fence()?
                {
                    return Err("ENTITY_LEASE_STALE".into());
                }
                if entry.source_hash != source_hash {
                    return Err("ENTITY_LEASE_SOURCE_CHANGED".into());
                }
                if matches!(mutation, EntityEditLeaseMutation::Renew { .. }) {
                    let mut renewed = entry.clone();
                    renewed.expires_at_ms = now_ms
                        .checked_add(i64::from(ENTITY_EDIT_LEASE_SECONDS) * 1000)
                        .ok_or("invalid lease clock")?;
                    Some(renewed)
                } else {
                    None
                }
            }
        };
        self.entries
            .retain(|entry| entry.expires_at_ms > now_ms && entry.resource != resource);
        if let Some(entry) = &next {
            self.authority_id = Some(entry.authority_id);
            self.last_fence = self.last_fence.max(entry.fence);
            self.entries.push(entry.clone());
            self.entries.sort_by_key(|entry| entry.resource);
        }
        Ok(next)
    }

    /// Fence any actual body change or deletion, irrespective of which writer
    /// constructed it. Unleased writes remain ordinary authorized operations.
    /// A supplied stale claim is always refused, even after release or expiry.
    pub fn check_body_change(
        &mut self,
        resource: EntityEditLeaseResource,
        old_hash: Hash256,
        new_hash: Option<Hash256>,
        context: Option<&EntityEditLeaseContext>,
        now_ms: i64,
    ) -> Result<(), String> {
        if let Some(context) = context {
            context.validate()?;
        }
        let claim = context.and_then(|context| {
            context
                .claims
                .iter()
                .find(|claim| claim.resource == resource)
        });
        let live_index = self
            .entries
            .iter()
            .position(|entry| entry.resource == resource && entry.expires_at_ms > now_ms);
        let Some(index) = live_index else {
            return if claim.is_some() {
                Err("ENTITY_LEASE_STALE".into())
            } else {
                Ok(())
            };
        };
        let entry = &self.entries[index];
        let Some((context, claim)) = context.zip(claim) else {
            return Err("ENTITY_LEASE_HELD".into());
        };
        if context.session_id != entry.session_id
            || claim.authority_id != entry.authority_id
            || claim.checked_fence()? != entry.fence
        {
            return Err("ENTITY_LEASE_STALE".into());
        }
        if old_hash != entry.source_hash {
            return Err("ENTITY_LEASE_SOURCE_CHANGED".into());
        }
        match new_hash {
            Some(hash) => self.entries[index].source_hash = hash,
            None => {
                self.entries.remove(index);
            }
        }
        Ok(())
    }
}

pub const ENTITY_EDIT_LEASE_SECONDS: u32 = 600;
pub const MAX_ENTITY_EDIT_LEASES: usize = 256;
pub const MAX_ENTITY_EDIT_LEASE_CLAIMS: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;

    fn resource() -> EntityEditLeaseResource {
        EntityEditLeaseResource {
            workspace_id: WorkspaceId::new(),
            entity_id: EntityId::new(),
        }
    }
    fn acquire(
        resource: EntityEditLeaseResource,
        owner: SessionId,
        hash: Hash256,
    ) -> EntityEditLeaseContext {
        EntityEditLeaseContext {
            session_id: owner,
            mutation: Some(EntityEditLeaseMutation::Acquire {
                resource,
                expected_source_hash: hash,
            }),
            claims: vec![],
        }
    }
    fn proof(entry: &EntityEditLease) -> EntityEditLeaseContext {
        EntityEditLeaseContext {
            session_id: entry.session_id,
            mutation: None,
            claims: vec![EntityEditLeaseClaim {
                authority_id: entry.authority_id,
                resource: entry.resource,
                fence: entry.fence.to_string(),
            }],
        }
    }
    #[test]
    fn fences_survive_reopen_expiry_release_and_body_cas() {
        let resource = resource();
        let hash = Hash256::from_bytes([1; 32]);
        let next_hash = Hash256::from_bytes([2; 32]);
        let owner = SessionId::new();
        let mut ledger = EntityEditLeaseLedger::default();
        let first = ledger
            .apply_control(&acquire(resource, owner, hash), hash, 10)
            .unwrap()
            .unwrap();
        assert_eq!(first.expires_at_ms, 600_010);
        assert_eq!(
            ledger
                .apply_control(&acquire(resource, owner, hash), hash, 11)
                .unwrap_err(),
            "ENTITY_LEASE_HELD"
        );
        assert_eq!(
            ledger
                .check_body_change(resource, hash, Some(next_hash), None, 11)
                .unwrap_err(),
            "ENTITY_LEASE_HELD"
        );
        let mut foreign = proof(&first);
        foreign.session_id = SessionId::new();
        assert_eq!(
            ledger
                .check_body_change(resource, hash, Some(next_hash), Some(&foreign), 11)
                .unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
        assert_eq!(
            ledger
                .check_body_change(resource, next_hash, Some(hash), Some(&proof(&first)), 11)
                .unwrap_err(),
            "ENTITY_LEASE_SOURCE_CHANGED"
        );
        ledger
            .check_body_change(resource, hash, Some(next_hash), Some(&proof(&first)), 11)
            .unwrap();
        assert_eq!(ledger.entries[0].source_hash, next_hash);
        ledger = serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        let expired = first.expires_at_ms;
        assert_eq!(
            ledger
                .check_body_change(resource, next_hash, None, Some(&proof(&first)), expired)
                .unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
        let second = ledger
            .apply_control(&acquire(resource, owner, next_hash), next_hash, expired)
            .unwrap()
            .unwrap();
        assert!(second.fence > first.fence);
        assert_eq!(
            ledger
                .check_body_change(resource, next_hash, None, Some(&proof(&first)), expired)
                .unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
        let release = EntityEditLeaseContext {
            session_id: owner,
            mutation: Some(EntityEditLeaseMutation::Release {
                claim: proof(&second).claims.remove(0),
            }),
            claims: vec![],
        };
        ledger
            .apply_control(&release, next_hash, expired + 1)
            .unwrap();
        assert!(ledger.entries.is_empty());
        let third = ledger
            .apply_control(&acquire(resource, owner, next_hash), next_hash, expired + 2)
            .unwrap()
            .unwrap();
        assert!(third.fence > second.fence);
        ledger
            .check_body_change(resource, next_hash, None, Some(&proof(&third)), expired + 3)
            .unwrap();
        assert!(ledger.entries.is_empty());
        assert_eq!(ledger.last_fence, third.fence);
        ledger.validate().unwrap();
    }
    #[test]
    fn a_foreign_authority_never_revives_an_identical_owner_resource_and_fence() {
        let resource = resource();
        let owner = SessionId::new();
        let hash = Hash256::from_bytes([1; 32]);
        let mut first = EntityEditLeaseLedger::default();
        let mut receiver = EntityEditLeaseLedger::default();
        let original = first
            .apply_control(&acquire(resource, owner, hash), hash, 0)
            .unwrap()
            .unwrap();
        let received = receiver
            .apply_control(&acquire(resource, owner, hash), hash, 0)
            .unwrap()
            .unwrap();
        assert_eq!(original.fence, received.fence);
        assert_ne!(original.authority_id, received.authority_id);
        assert_eq!(
            receiver
                .check_body_change(resource, hash, None, Some(&proof(&original)), 1)
                .unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
        assert_eq!(receiver.entries.len(), 1);
    }

    #[test]
    fn renewal_checks_owner_fence_source_and_fixed_server_lifetime() {
        let resource = resource();
        let owner = SessionId::new();
        let hash = Hash256::from_bytes([1; 32]);
        let mut ledger = EntityEditLeaseLedger::default();
        let entry = ledger
            .apply_control(&acquire(resource, owner, hash), hash, 0)
            .unwrap()
            .unwrap();
        let mut renew = EntityEditLeaseContext {
            session_id: SessionId::new(),
            mutation: Some(EntityEditLeaseMutation::Renew {
                claim: proof(&entry).claims.remove(0),
            }),
            claims: vec![],
        };
        let before = ledger.clone();
        assert_eq!(
            ledger.apply_control(&renew, hash, 100).unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
        assert_eq!(ledger, before);
        renew.session_id = owner;
        assert_eq!(
            ledger
                .apply_control(&renew, Hash256::from_bytes([2; 32]), 100)
                .unwrap_err(),
            "ENTITY_LEASE_SOURCE_CHANGED"
        );
        let renewed = ledger.apply_control(&renew, hash, 100).unwrap().unwrap();
        assert_eq!(renewed.fence, entry.fence);
        assert_eq!(renewed.expires_at_ms, 600_100);
        assert_eq!(
            ledger.apply_control(&renew, hash, 600_100).unwrap_err(),
            "ENTITY_LEASE_STALE"
        );
    }
    #[test]
    fn invalid_context_and_fence_exhaustion_do_not_change_ledger() {
        let resource = resource();
        let hash = Hash256::from_bytes([1; 32]);
        let mut ledger = EntityEditLeaseLedger {
            authority_id: Some(uuid::Uuid::new_v4()),
            last_fence: u64::MAX,
            entries: vec![],
        };
        assert_eq!(
            ledger
                .apply_control(&acquire(resource, SessionId::new(), hash), hash, 0)
                .unwrap_err(),
            "ENTITY_LEASE_FENCE_EXHAUSTED"
        );
        assert_eq!(ledger.last_fence, u64::MAX);
        let mut context = acquire(resource, SessionId::new(), hash);
        context.session_id = SessionId(uuid::Uuid::nil());
        assert!(context.validate().is_err());
        let mut context = proof(&EntityEditLease {
            authority_id: uuid::Uuid::new_v4(),
            resource,
            session_id: SessionId::new(),
            fence: 1,
            source_hash: hash,
            expires_at_ms: 1,
        });
        context.claims.push(context.claims[0].clone());
        assert!(context.validate().is_err());
    }

    #[test]
    fn fence_wire_never_rounds_or_accepts_ambiguous_numbers() {
        let mut claim = EntityEditLeaseClaim {
            authority_id: uuid::Uuid::new_v4(),
            resource: EntityEditLeaseResource {
                workspace_id: WorkspaceId::new(),
                entity_id: EntityId::new(),
            },
            fence: u64::MAX.to_string(),
        };
        assert_eq!(claim.checked_fence().unwrap(), u64::MAX);
        let wire = serde_json::to_string(&claim).unwrap();
        assert!(wire.contains("\"18446744073709551615\""));
        assert_eq!(
            serde_json::from_str::<EntityEditLeaseClaim>(&wire).unwrap(),
            claim
        );
        for invalid in ["0", "01", "+1", " 1", "1.0", "1e2", "18446744073709551616"] {
            claim.fence = invalid.into();
            assert!(claim.checked_fence().is_err(), "{invalid}");
        }
    }

    #[test]
    fn lease_commands_cannot_supply_an_owner_or_clock() {
        let mutation = EntityEditLeaseMutation::Acquire {
            resource: EntityEditLeaseResource {
                workspace_id: WorkspaceId::new(),
                entity_id: EntityId::new(),
            },
            expected_source_hash: Hash256::from_bytes([7; 32]),
        };
        for key in ["session_id", "expires_at_ms", "user_id"] {
            let mut wire = serde_json::to_value(&mutation).unwrap();
            wire[key] = serde_json::json!("forged");
            assert!(serde_json::from_value::<EntityEditLeaseMutation>(wire).is_err());
        }
    }
}
