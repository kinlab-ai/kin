// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! The one endpoint grammar every read surface speaks.
//!
//! `kin diff`, `kin blame --ref` and `kin history --ref` all name a point in
//! repository history, and until FIR-3015 each of the first two parsed that name
//! with its own code. `diff.rs` had a `resolve_endpoint` that knew `@`, `ref:`
//! and `ref-hex:`; `ref_lookup.rs` had a `resolve_ref_core` that knew `HEAD~N`,
//! `kin:` and `branch:`. Neither knew the other's forms, and neither knew a
//! short id at all, which is the form `kin history` prints.
//!
//! The npm-mode stranger run met all three halves of that in one sitting and
//! called it the dominant friction of its version control arm. Each refusal was
//! correct about its own resolver and useless to the operator, because the
//! grammar you learn on one surface is not the grammar the next one speaks:
//!
//! ```text
//! kin diff 1971f659d7aa 479aa9b94288
//!   diff endpoint '1971f659d7aa' is not an authority ref, semantic change,
//!   Git object, HEAD, or WORKSPACE
//! ```
//!
//! That twelve-hex string came out of `kin history` one command earlier.
//!
//! So resolution lives here once and both surfaces call it. The unification
//! needs no trait: `ActiveRepositoryAuthority::manager().read_authority()`
//! yields exactly the `AuthorityReadLease` diff already holds, so one function
//! taking a lease and a graph serves both callers.
//!
//! `WORKSPACE` stays in `diff.rs` on purpose. It names the uncommitted working
//! tree rather than a point in history, there is no change id behind it, and
//! blame and history have nothing to do with it.

use anyhow::{anyhow, bail, Context, Result};
use kin_model::{
    GitObjectId, GraphStore, Hash256, RefName, RefTarget, SemanticChangeId, WorkspaceId,
};

use super::repository_authority::{parse_git_object_id, parse_ref_name};

/// The shortest prefix that may stand in for a change id.
///
/// Four, matching Git's floor. Shorter than that is far more likely to be a
/// branch name than an id, and the bare-name arm reaches it first anyway.
pub(crate) const MIN_PREFIX: usize = 4;

/// The full width of a semantic change id in hexadecimal.
pub(crate) const CHANGE_ID_HEX: usize = 64;

/// How many candidates an ambiguous-prefix refusal names before it stops.
const AMBIGUITY_PREVIEW: usize = 4;

/// The sentence a selector that matched no arm of the grammar is refused with.
///
/// A constant rather than two literals, and both surfaces' tests assert on this
/// symbol rather than on its text. A surface that forked back to a private
/// parser would write its own wording and those tests would go red, which is the
/// only cheap way to prove the two are still joined.
pub(crate) const UNRESOLVABLE: &str =
    "is not a ref, a semantic change or its unique prefix, an imported Git object, or HEAD";

/// Repository authority, reached only if an arm of the grammar asks for it.
///
/// The two callers arrive differently and the difference is load-bearing.
/// `kin diff` already holds an open lease, so re-opening would be waste. `kin
/// blame` and `kin history` hold a [`RequestRepositoryAuthority`]: the daemon's
/// shared arm, which hands over the authority it keeps for the current
/// publication, or a binding a one-shot caller opens for itself.
/// `explicit_semantic_change_and_parent_hops_need_no_file_or_git_fallback` pins
/// the property that a `kin:<id>` or `change:<id>` selector resolves from the
/// graph alone: an explicit semantic change is graph-owned truth and reaching
/// for the authority envelope to confirm it would be a file-first fallback on a
/// path that does not need one.
///
/// Opening eagerly here is what broke that test, and the test was right.
///
/// [`RequestRepositoryAuthority`]: super::repository_authority::RequestRepositoryAuthority
pub(crate) enum Authority<'a> {
    Held {
        lease: &'a kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
        workspace_id: &'a WorkspaceId,
    },
    Deferred {
        source: &'a super::repository_authority::RequestRepositoryAuthority,
        opened: std::cell::OnceCell<OpenedAuthority>,
    },
}

/// An authority this module reached for, kept alive beside its lease.
pub(crate) struct OpenedAuthority {
    lease: kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    workspace_id: WorkspaceId,
    // Held so the manager outlives the lease taken from it, rather than relying
    // on the lease's Arc alone.
    _authority: std::sync::Arc<super::repository_authority::ActiveRepositoryAuthority>,
    cost: AuthorityOpen,
}

/// What reaching repository authority cost the thread that resolved a selector.
///
/// Measured where the authority is handed over rather than inferred afterwards,
/// because the two ways of getting one differ by the size of the store: a server
/// that already holds the current publication hands it over at once, and a
/// whole-store open re-verifies every persisted body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AuthorityOpen {
    /// How long handing the authority over took.
    pub(crate) waited: std::time::Duration,
    /// Whether this thread performed a whole-store open to get it, read off the
    /// per-thread counter the tests bound, rather than being handed one that was
    /// already open.
    pub(crate) opened_here: bool,
    /// Whether the authority came from a server that keeps it for its current
    /// publication, so an open paid for here is not paid again by the next read.
    pub(crate) shared: bool,
}

impl<'a> Authority<'a> {
    pub(crate) fn held(
        lease: &'a kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
        workspace_id: &'a WorkspaceId,
    ) -> Self {
        Self::Held {
            lease,
            workspace_id,
        }
    }

    pub(crate) fn deferred(
        source: &'a super::repository_authority::RequestRepositoryAuthority,
    ) -> Self {
        Self::Deferred {
            source,
            opened: std::cell::OnceCell::new(),
        }
    }

    /// What reaching the authority cost, when an arm of the grammar needed it.
    ///
    /// `None` for a lease the caller already held, and for a selector the graph
    /// answered alone, since neither reached for anything.
    pub(crate) fn open_cost(&self) -> Option<AuthorityOpen> {
        match self {
            Self::Held { .. } => None,
            Self::Deferred { opened, .. } => opened.get().map(|held| held.cost),
        }
    }

    fn parts(
        &self,
        original: &str,
    ) -> Result<(
        &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
        &WorkspaceId,
    )> {
        match self {
            Self::Held {
                lease,
                workspace_id,
            } => Ok((lease, workspace_id)),
            Self::Deferred { source, opened } => {
                if opened.get().is_none() {
                    // Counted on kin-core's funnel rather than on this crate's
                    // own wrapper counter. Every path into KinDB's recovery
                    // reaches that funnel, and a server resolving this authority
                    // may open through another crate's wrapper, which this
                    // crate's counter cannot see. Read from the wrapper counter,
                    // the first read at a publication reported that nothing was
                    // opened while a whole-store open had just been paid for it.
                    let opens_before = kin_core::authority_opens();
                    let started = std::time::Instant::now();
                    let authority = source.open().map_err(|error| {
                        ref_error(
                            original,
                            format!("this repository's authority could not be opened: {error:#}"),
                        )
                    })?;
                    let cost = AuthorityOpen {
                        waited: started.elapsed(),
                        opened_here: kin_core::authority_opens() > opens_before,
                        shared: source.is_shared(),
                    };
                    let lease = authority.manager().read_authority();
                    let workspace_id = authority.workspace_id.clone();
                    let _ = opened.set(OpenedAuthority {
                        lease,
                        workspace_id,
                        _authority: authority,
                        cost,
                    });
                }
                let held = opened
                    .get()
                    .expect("repository authority was just placed in the cell");
                Ok((&held.lease, &held.workspace_id))
            }
        }
    }
}

/// Which arm of the grammar answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectorKind {
    Head,
    Ref,
    Change,
    GitObject,
}

/// One resolved endpoint, with enough provenance for a caller to describe it.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedRef {
    pub change_id: SemanticChangeId,
    /// The ref this came from, when it came from one. `None` after a parent hop,
    /// because the ref names the tip and the hop walked away from it.
    pub ref_name: Option<RefName>,
    pub target: Option<RefTarget>,
    pub kind: SelectorKind,
}

/// A `~N` or `^N` step off the core selector.
#[derive(Debug, Clone, Copy)]
struct ParentHop(usize);

fn ref_error(reference: &str, reason: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("cannot resolve '{reference}': {reason}")
}

/// Whether `value` could be a change id or a prefix of one.
///
/// Lowercase only. A change id is rendered lowercase everywhere Kin prints one,
/// so accepting uppercase here would mean accepting a string Kin never emits and
/// then having to decide what a mixed-case ref name means.
pub(crate) fn is_change_prefix(value: &str) -> bool {
    (MIN_PREFIX..=CHANGE_ID_HEX).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value == value.to_ascii_lowercase()
}

/// Split trailing `~N` and `^N` steps off a selector, innermost last.
///
/// Lifted from `ref_lookup.rs` unchanged, which is the point: it used to be
/// reachable from `kin blame` and not from `kin diff`.
fn split_relative_hops(reference: &str) -> Result<(&str, Vec<ParentHop>)> {
    let mut hops = Vec::new();
    let mut rest = reference;
    while let Some(&last) = rest.as_bytes().last() {
        if last == b'^' || last == b'~' {
            hops.push(ParentHop(1));
            rest = &rest[..rest.len() - 1];
            continue;
        }
        if last.is_ascii_digit() {
            let digits_start = rest
                .rfind(|character: char| !character.is_ascii_digit())
                .map(|index| index + 1)
                .unwrap_or(0);
            if digits_start == 0 {
                break;
            }
            let marker = rest.as_bytes()[digits_start - 1];
            if marker != b'^' && marker != b'~' {
                break;
            }
            let count: usize = rest[digits_start..]
                .parse()
                .map_err(|_| ref_error(reference, "parent index is out of range"))?;
            if marker == b'^' {
                if count == 0 {
                    break;
                }
                hops.push(ParentHop(count));
            } else {
                hops.extend(std::iter::repeat_n(ParentHop(1), count));
            }
            rest = &rest[..digits_start - 1];
            continue;
        }
        break;
    }
    Ok((rest, hops))
}

fn apply_parent_hops<G>(
    graph: &G,
    original: &str,
    mut head: SemanticChangeId,
    hops: &[ParentHop],
) -> Result<SemanticChangeId>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    for hop in hops {
        let change = graph
            .get_change(&head)
            .map_err(|error| anyhow!(error.to_string()))?
            .ok_or_else(|| ref_error(original, format!("change {head} not found in history")))?;
        head = *change.parents.get(hop.0 - 1).ok_or_else(|| {
            ref_error(
                original,
                format!(
                    "{head} has {} parent(s), no parent #{}",
                    change.parents.len(),
                    hop.0
                ),
            )
        })?;
    }
    Ok(head)
}

pub(crate) fn parse_change_id(input: &str) -> Result<SemanticChangeId> {
    Ok(SemanticChangeId::from_hash(
        Hash256::from_hex(input).map_err(|error| anyhow!("invalid change hash: {error}"))?,
    ))
}

/// The one change whose id starts with `prefix`, or a refusal that says why not.
///
/// `candidates` is the authority snapshot's own change map, which both callers
/// already hold a lease on. That map IS the repository's history, so a prefix is
/// matched against every change kin holds rather than against the slice
/// reachable from one branch: an id an operator read out of
/// `kin history --ref <branch>` resolves here whatever branch they are standing
/// on. Nothing on disk is consulted; the ids come from graph-owned authority.
///
/// It arrives as an iterator of ids rather than as the lease itself so that the
/// refusal below can be graded on a collision that was CONSTRUCTED. Change ids
/// are hashes, so two of a small repository's own ids sharing four hexadecimal
/// characters is a one-in-tens-of-thousands accident, and a check that waits for
/// one is a check that never runs. Taking the ids alone lets the tests below
/// hand this two that collide by construction, which is the only reason the
/// ambiguity arm is graded at all.
///
/// Ambiguity is refused rather than resolved to whichever candidate came first.
/// Picking one would be the worst outcome available: it answers, it looks right,
/// and it silently describes a different point in history than the operator
/// meant.
fn resolve_change_prefix(
    candidates: impl Iterator<Item = SemanticChangeId>,
    original: &str,
    prefix: &str,
) -> Result<SemanticChangeId> {
    // Every match is collected before any is named, and the sort is what makes
    // the refusal reproducible. The early exit this used to take once
    // `AMBIGUITY_PREVIEW` candidates were in hand ran BEFORE that sort, so which
    // ids a refusal named came from the change map's iteration order, which a
    // HashMap does not promise: two runs of one command could name two different
    // sets and read like two different failures. The filter is a string prefix
    // over a set bounded by the repository's own history, so collecting it whole
    // costs nothing worth that ambiguity, and it makes the count an exact number
    // rather than a floor.
    let mut matches: Vec<SemanticChangeId> = candidates
        .filter(|change_id| change_id.to_string().starts_with(prefix))
        .collect();
    matches.sort();
    match matches.len() {
        0 => Err(ref_error(
            original,
            format!(
                "no semantic change in this repository's history begins with '{prefix}'; run \
                 `kin log` to see the changes kin holds"
            ),
        )),
        1 => Ok(matches[0]),
        count => {
            let preview = matches
                .iter()
                .take(AMBIGUITY_PREVIEW)
                .map(|candidate| candidate.to_string()[..12].to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let unnamed = count.saturating_sub(AMBIGUITY_PREVIEW);
            let elision = if unnamed == 0 {
                String::new()
            } else {
                format!(", and {unnamed} more")
            };
            Err(ref_error(
                original,
                format!(
                    "'{prefix}' is ambiguous: {count} semantic changes begin with it \
                     ({preview}{elision}); use more characters"
                ),
            ))
        }
    }
}

/// The change the workspace's committed base names, if it has one.
fn workspace_head(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    workspace_id: &WorkspaceId,
) -> Result<Option<SemanticChangeId>> {
    let workspace = lease
        .metadata()
        .workspaces
        .iter()
        .find(|workspace| &workspace.workspace_id == workspace_id)
        .ok_or_else(|| {
            anyhow!("this repository has no workspace {workspace_id} in its authority")
        })?;
    workspace
        .base_target
        .as_ref()
        .map(|target| lease.resolve_target_change_id(target))
        .transpose()
        .context("resolve the workspace's committed base")
}

/// Resolve a ref name the authority is expected to hold.
fn resolve_named(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    original: &str,
    name: RefName,
) -> Result<ResolvedRef> {
    let target = lease
        .resolve_ref_target(&name)
        .with_context(|| format!("resolve repository ref '{name}'"))?
        .ok_or_else(|| ref_error(original, format!("repository ref '{name}' was not found")))?;
    let change_id = lease
        .resolve_target_change_id(&target)
        .with_context(|| format!("resolve repository ref '{name}' semantic target"))?;
    Ok(ResolvedRef {
        change_id,
        ref_name: Some(name),
        target: Some(target),
        kind: SelectorKind::Ref,
    })
}

/// Resolve an imported Git object.
///
/// Reads `external_objects` as well as `aliases`. `diff.rs` already read both
/// and `ref_lookup.rs` read only the aliases, so unifying on the superset means
/// `kin blame --ref` gains the external-object arm it never had.
fn resolve_git_object(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    original: &str,
    oid: GitObjectId,
) -> Result<ResolvedRef> {
    let target = lease
        .metadata()
        .external_objects
        .iter()
        .find(|record| record.object.oid == oid)
        .map(|record| RefTarget::external_object(record.object))
        .or_else(|| {
            lease
                .metadata()
                .aliases
                .iter()
                .find(|alias| alias.oid == oid)
                .map(|alias| RefTarget::change(alias.change_id))
        })
        .ok_or_else(|| {
            ref_error(
                original,
                format!(
                    "Git object '{oid}' was never imported into this repository, so kin holds no \
                     semantic change for it; import it with `kin init` in the source checkout, or \
                     name a ref kin already holds"
                ),
            )
        })?;
    let change_id = lease
        .resolve_target_change_id(&target)
        .with_context(|| format!("resolve Git object '{oid}' semantic target"))?;
    Ok(ResolvedRef {
        change_id,
        ref_name: None,
        target: Some(target),
        kind: SelectorKind::GitObject,
    })
}

/// The width of a whole SHA-1 Git object id in hexadecimal. Anything shorter
/// after `git:` is a prefix, the form `git log --oneline` prints.
const GIT_SHA1_HEX: usize = 40;

/// Whether `value` is a short Git object id rather than a whole one.
///
/// Lowercase only, for the reason [`is_change_prefix`] gives: Git prints its ids
/// lowercase, and a whole id keeps its own parser either way.
fn is_git_object_prefix(value: &str) -> bool {
    (MIN_PREFIX..GIT_SHA1_HEX).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && value == value.to_ascii_lowercase()
}

/// Every imported Git object that can name a point in history, from both
/// records that name one.
///
/// Commits and tags only. A tree or a blob never resolves to a semantic change,
/// so letting one share a short id with a commit would refuse as ambiguous a
/// selector that has one possible answer, which is Git's own commit-ish rule.
fn imported_git_objects(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
) -> impl Iterator<Item = GitObjectId> + '_ {
    let metadata = lease.metadata();
    metadata
        .external_objects
        .iter()
        .filter(|record| {
            matches!(
                record.object.kind,
                kin_model::ExternalObjectKind::Commit | kin_model::ExternalObjectKind::Tag
            )
        })
        .map(|record| record.object.oid)
        .chain(metadata.aliases.iter().map(|alias| alias.oid))
}

/// The one imported Git object whose id starts with `prefix`, or a refusal.
///
/// Matched against the objects the import recorded, never against a Git object
/// database: the store answers for what it admitted. One commit can appear in
/// both records, so candidates are deduplicated before they are counted, and
/// ambiguity is refused for the reason [`resolve_change_prefix`] gives.
fn resolve_git_object_prefix(
    candidates: impl Iterator<Item = GitObjectId>,
    original: &str,
    prefix: &str,
) -> Result<GitObjectId> {
    let matches: std::collections::BTreeSet<GitObjectId> = candidates
        .filter(|oid| oid.to_string().starts_with(prefix))
        .collect();
    let mut matches = matches.into_iter();
    match (matches.next(), matches.len()) {
        (None, _) => Err(ref_error(
            original,
            format!(
                "no Git object imported into this repository begins with '{prefix}'; import it \
                 with `kin init` in the source checkout, or name a ref kin already holds"
            ),
        )),
        (Some(only), 0) => Ok(only),
        (Some(first), rest) => {
            let count = rest + 1;
            let preview = std::iter::once(first)
                .chain(matches)
                .take(AMBIGUITY_PREVIEW)
                .map(|oid| oid.to_string()[..12].to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let unnamed = count.saturating_sub(AMBIGUITY_PREVIEW);
            let elision = if unnamed == 0 {
                String::new()
            } else {
                format!(", and {unnamed} more")
            };
            Err(ref_error(
                original,
                format!(
                    "'{prefix}' is ambiguous: {count} imported Git objects begin with it \
                     ({preview}{elision}); use more characters"
                ),
            ))
        }
    }
}

/// A change that must already be in the graph the caller will read it from.
fn resolve_present_change<G>(
    graph: &G,
    original: &str,
    change_id: SemanticChangeId,
) -> Result<ResolvedRef>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    if graph
        .get_change(&change_id)
        .map_err(|error| anyhow!(error.to_string()))?
        .is_none()
    {
        return Err(ref_error(
            original,
            format!(
                "semantic change {change_id} is not in this repository's authority; run `kin log` \
                 to see the changes kin holds"
            ),
        ));
    }
    Ok(ResolvedRef {
        change_id,
        ref_name: None,
        target: Some(RefTarget::change(change_id)),
        kind: SelectorKind::Change,
    })
}

/// Resolve one endpoint selector against repository authority.
///
/// This is the whole grammar, in one place, for every surface that names a point
/// in history. The arms are ordered, and the order is load-bearing:
///
///   1. `HEAD` and `@`, the workspace's committed base
///   2. explicitly tagged forms, `ref-hex:`, `ref:`, `branch:`, `kin:`,
///      `change:` and `git:`, which say what they are and are never guessed at
///   3. a bare value, which prefers an exact ref, then a semantic change, then
///      an imported Git object, then a unique change-id prefix
///
/// A bare value prefers a ref because that matches Git's ordinary branch UX and
/// because a branch is a thing a person named on purpose. The prefix arm is last
/// for the same reason in reverse: it is the only arm that can be ambiguous, so
/// nothing that can be resolved exactly should ever reach it.
pub(crate) fn resolve<G>(authority: &Authority<'_>, graph: &G, input: &str) -> Result<ResolvedRef>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    if input.contains("@{") {
        return Err(ref_error(
            input,
            "reflog and upstream '@{...}' syntax is not supported",
        ));
    }

    let (core, hops) = split_relative_hops(input)?;
    let resolved = resolve_core(authority, graph, input, core)?;
    if hops.is_empty() {
        return Ok(resolved);
    }

    // Past a parent hop the ref no longer describes the answer: the ref names a
    // tip and the hop walked away from it. Reporting `main` for `main~2` would
    // put a true name on a false claim.
    let change_id = apply_parent_hops(graph, input, resolved.change_id, &hops)?;
    Ok(ResolvedRef {
        change_id,
        ref_name: None,
        target: Some(RefTarget::change(change_id)),
        kind: SelectorKind::Change,
    })
}

fn resolve_core<G>(
    authority: &Authority<'_>,
    graph: &G,
    original: &str,
    core: &str,
) -> Result<ResolvedRef>
where
    G: GraphStore,
    <G as GraphStore>::Error: std::fmt::Display + Send + Sync + 'static,
{
    // First, and deliberately: an explicit semantic change is graph-owned truth.
    // Its full-id form is answered without opening repository authority at all,
    // which is the property `explicit_semantic_change_and_parent_hops_need_no_
    // file_or_git_fallback` pins. Only the prefix half needs the authority
    // snapshot, because a prefix is a search rather than a name.
    if let Some(value) = core
        .strip_prefix("kin:")
        .or_else(|| core.strip_prefix("change:"))
    {
        let change_id = if value.len() == CHANGE_ID_HEX {
            parse_change_id(value).map_err(|error| ref_error(original, error.to_string()))?
        } else if is_change_prefix(value) {
            let (lease, _) = authority.parts(original)?;
            resolve_change_prefix(lease.snapshot().changes.keys().copied(), original, value)?
        } else {
            return Err(ref_error(
                original,
                format!(
                    "'{value}' is not a semantic change id or a prefix of one; ids are \
                     {CHANGE_ID_HEX} lowercase hexadecimal characters and a prefix must be at \
                     least {MIN_PREFIX}"
                ),
            ));
        };
        return resolve_present_change(graph, original, change_id);
    }

    if matches!(core, "HEAD" | "@") {
        let (lease, workspace_id) = authority.parts(original)?;
        let change_id = workspace_head(lease, workspace_id)?.ok_or_else(|| {
            ref_error(
                original,
                "this workspace has no commits yet, so HEAD names nothing; make one with \
                 `kin commit`",
            )
        })?;
        return Ok(ResolvedRef {
            change_id,
            ref_name: None,
            target: Some(RefTarget::change(change_id)),
            kind: SelectorKind::Head,
        });
    }

    if let Some(hex_name) = core.strip_prefix("ref-hex:") {
        if hex_name.is_empty()
            || hex_name != hex_name.to_ascii_lowercase()
            || !hex_name.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("ref-hex selector must use non-empty canonical lowercase hexadecimal bytes");
        }
        let bytes = hex::decode(hex_name).context("decode ref-hex selector")?;
        let name = RefName::from_bytes(bytes)
            .map_err(|error| anyhow!("invalid ref-hex selector: {error}"))?;
        let (lease, _) = authority.parts(original)?;
        return resolve_named(lease, original, name);
    }

    if let Some(value) = core
        .strip_prefix("ref:")
        .or_else(|| core.strip_prefix("branch:"))
    {
        let (lease, _) = authority.parts(original)?;
        return resolve_named(lease, original, parse_ref_name(value)?);
    }

    if let Some(value) = core.strip_prefix("git:") {
        let (lease, _) = authority.parts(original)?;
        if is_git_object_prefix(value) {
            let oid = resolve_git_object_prefix(imported_git_objects(lease), original, value)?;
            return resolve_git_object(lease, original, oid);
        }
        return resolve_git_object(lease, original, parse_git_object_id(value)?);
    }

    // A bare value prefers an exact ref, so a branch someone named stays a
    // branch even when its name happens to look like hexadecimal.
    if let Ok(name) = parse_ref_name(core) {
        let (lease, _) = authority.parts(original)?;
        if lease.resolve_ref_target(&name)?.is_some() {
            return resolve_named(lease, original, name);
        }
    }

    if core.len() == CHANGE_ID_HEX && core.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        if let Ok(change_id) = parse_change_id(core) {
            if graph
                .get_change(&change_id)
                .map_err(|error| anyhow!(error.to_string()))?
                .is_some()
            {
                return resolve_present_change(graph, original, change_id);
            }
        }
    }

    // Exactly 40 or 64 hexadecimal characters is a whole Git object id and
    // nothing else: a change id of that width was already tried above, and a
    // prefix arm cannot help a value that is already full width.
    if matches!(core.len(), 40 | CHANGE_ID_HEX) && core.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        let (lease, _) = authority.parts(original)?;
        return resolve_git_object(lease, original, parse_git_object_id(core)?);
    }

    if is_change_prefix(core) {
        let (lease, _) = authority.parts(original)?;
        let change_id = resolve_bare_prefix(lease, original, core)?;
        return resolve_present_change(graph, original, change_id);
    }

    Err(ref_error(original, UNRESOLVABLE))
}

/// A bare short hexadecimal value: the start of a change id, or the short
/// Git commit id `git log --oneline` prints, which is what a developer types.
///
/// Both are searched. One that only one of them holds resolves there, with
/// the refusals each search already gives, and a prefix both hold resolves
/// only when every match names the same change, as a commit and the change
/// its import recorded do. Otherwise it is refused with the tagged forms that
/// say which was meant, never resolved to whichever was found first.
fn resolve_bare_prefix(
    lease: &kin_db::AuthorityReadLease<kin_db::RepositoryAuthorityState>,
    original: &str,
    prefix: &str,
) -> Result<SemanticChangeId> {
    let changes: Vec<SemanticChangeId> = lease
        .snapshot()
        .changes
        .keys()
        .copied()
        .filter(|change_id| change_id.to_string().starts_with(prefix))
        .collect();
    let git: std::collections::BTreeSet<GitObjectId> = if is_git_object_prefix(prefix) {
        imported_git_objects(lease)
            .filter(|oid| oid.to_string().starts_with(prefix))
            .collect()
    } else {
        std::collections::BTreeSet::new()
    };
    let mut commits = Vec::with_capacity(git.len());
    for oid in git {
        commits.push((oid, resolve_git_object(lease, original, oid)?.change_id));
    }
    choose_bare_prefix(original, prefix, changes, commits)
}

/// The change a bare prefix names, from what each search matched.
fn choose_bare_prefix(
    original: &str,
    prefix: &str,
    changes: Vec<SemanticChangeId>,
    commits: Vec<(GitObjectId, SemanticChangeId)>,
) -> Result<SemanticChangeId> {
    if changes.is_empty() && commits.is_empty() {
        return Err(ref_error(
            original,
            format!(
                "no semantic change and no imported Git commit begins with '{prefix}'; run \
                 `kin log` to see the changes kin holds"
            ),
        ));
    }
    if commits.is_empty() {
        return resolve_change_prefix(changes.into_iter(), original, prefix);
    }
    if changes.is_empty() {
        let oid = resolve_git_object_prefix(commits.iter().map(|(oid, _)| *oid), original, prefix)?;
        return Ok(commits
            .into_iter()
            .find(|(candidate, _)| *candidate == oid)
            .map(|(_, change)| change)
            .expect("the chosen object is one of the candidates"));
    }
    let named: std::collections::BTreeSet<SemanticChangeId> = changes
        .iter()
        .copied()
        .chain(commits.iter().map(|(_, change)| *change))
        .collect();
    if named.len() == 1 {
        return Ok(*named.iter().next().expect("one change"));
    }
    Err(ref_error(
        original,
        format!(
            "'{prefix}' is ambiguous: it begins {} and {}; name one with `change:{prefix}` or \
             `git:{prefix}`, or use more characters",
            if changes.len() == 1 {
                "1 semantic change id".to_string()
            } else {
                format!("{} semantic change ids", changes.len())
            },
            if commits.len() == 1 {
                "1 imported Git commit id".to_string()
            } else {
                format!("{} imported Git commit ids", commits.len())
            }
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two files that must not grow a parser of their own again.
    ///
    /// Embedded at compile time rather than read at run time, so the test grades
    /// the exact bytes that were compiled. A path read would grade whichever
    /// checkout the runner happened to be standing in, which is the same class
    /// of mistake as the drift this module exists to end.
    const SURFACES: &[(&str, &str)] = &[
        ("diff.rs", include_str!("diff.rs")),
        ("ref_lookup.rs", include_str!("ref_lookup.rs")),
    ];

    /// Tagged forms this module owns. A surface that recognised any of these on
    /// its own would be a second grammar, which is the defect FIR-3015 fixed.
    const OWNED_PREFIXES: &[&str] = &[
        "\"kin:\"",
        "\"change:\"",
        "\"branch:\"",
        "\"ref:\"",
        "\"ref-hex:\"",
        "\"git:\"",
    ];

    #[test]
    fn parent_hops_split_off_every_shape_git_accepts() {
        for (input, core, hops) in [
            ("HEAD", "HEAD", 0),
            ("HEAD~", "HEAD", 1),
            ("HEAD^", "HEAD", 1),
            ("HEAD~1", "HEAD", 1),
            ("HEAD~3", "HEAD", 3),
            ("HEAD^2", "HEAD", 1),
            ("HEAD~2^", "HEAD", 3),
            ("main~1", "main", 1),
        ] {
            let (parsed_core, parsed_hops) = split_relative_hops(input).unwrap();
            assert_eq!(parsed_core, core, "core of {input}");
            assert_eq!(parsed_hops.len(), hops, "hop count of {input}");
        }
    }

    /// A name that merely ends in digits is not a hop, or `v2` would resolve to
    /// the parent of `v`.
    #[test]
    fn a_trailing_digit_without_a_marker_is_part_of_the_name() {
        let (core, hops) = split_relative_hops("release2").unwrap();
        assert_eq!(core, "release2");
        assert!(hops.is_empty(), "release2 is a branch name, not a hop");
    }

    #[test]
    fn a_change_prefix_is_lowercase_hex_of_a_workable_width() {
        assert!(is_change_prefix("dead"), "four characters is the floor");
        assert!(
            is_change_prefix("1971f659d7aa"),
            "the width kin history prints"
        );
        assert!(
            is_change_prefix(&"a".repeat(CHANGE_ID_HEX)),
            "a full id is its own prefix"
        );
        assert!(
            !is_change_prefix("dea"),
            "three characters is below the floor"
        );
        assert!(
            !is_change_prefix(&"a".repeat(CHANGE_ID_HEX + 1)),
            "longer than an id is not a prefix of one"
        );
        assert!(
            !is_change_prefix("DEADBEEF"),
            "kin never prints uppercase ids"
        );
        assert!(!is_change_prefix("deadbeeg"), "g is not hexadecimal");
        assert!(!is_change_prefix("main"), "a branch name is not a prefix");
    }

    /// The join: `kin diff` and `kin blame --ref` resolve through this module
    /// and hold no grammar of their own.
    ///
    /// FIR-3015 happened because they each had a parser and nothing compared
    /// them. This reads the two files and requires one call into `resolve` and
    /// zero of the tagged prefixes this module owns. Mutating either surface
    /// back to a private copy turns it red, which is the only cheap way to prove
    /// the two are still joined.
    ///
    /// The needles cannot match this test itself, because the files read are the
    /// other two. The positive control is the assertion that this module DOES
    /// carry every needle: without it, a typo in a needle would report both
    /// surfaces clean and the check would pass over any code at all.
    #[test]
    fn diff_and_blame_resolve_through_this_module_and_hold_no_grammar_of_their_own() {
        let grammar = include_str!("ref_grammar.rs");
        for needle in OWNED_PREFIXES {
            assert!(
                grammar.contains(needle),
                "CONTROL: {needle} is not in ref_grammar.rs, so the absence checks below \
                 are searching for something that exists nowhere and cannot fail"
            );
        }

        for (path, source) in SURFACES {
            assert_eq!(
                source.matches("ref_grammar::resolve(").count(),
                1,
                "{path} must reach repository history through exactly one call to the \
                 shared resolver"
            );
            for needle in OWNED_PREFIXES {
                assert!(
                    !source.contains(needle),
                    "{path} recognises {needle} on its own, which is a second grammar; \
                     FIR-3015 is exactly what two of those cost"
                );
            }
        }
    }

    /// A full-width change id that starts with `prefix` and is padded with `fill`.
    ///
    /// Built rather than mined. Two of a three-commit repository's own ids
    /// sharing four hexadecimal characters is a one-in-tens-of-thousands
    /// accident, so a test that seeds a fixture and hopes for a collision is a
    /// test that almost never reaches the code it was written for. These ids are
    /// never stored or replayed; they exist only to be matched against a prefix,
    /// which is the whole of what the function under test does.
    ///
    /// The padding runs to the full width rather than sitting in the last
    /// character on purpose: a refusal previews twelve characters of each
    /// candidate, so ids that differ only in their sixty-fourth would preview
    /// identically and an assertion that both were named would hold over a
    /// message that named one of them twice.
    fn id_with_prefix(prefix: &str, fill: char) -> SemanticChangeId {
        let mut hex = String::from(prefix);
        while hex.len() < CHANGE_ID_HEX {
            hex.push(fill);
        }
        parse_change_id(&hex).expect("constructed a full-width change id")
    }

    /// Ambiguity is refused, and the refusal says which selector and how many.
    ///
    /// This is the arm an operator meets when a short id they typed names more
    /// than one point in history. Resolving it would be the worst outcome
    /// available: it answers, it looks right, and it describes a change the
    /// operator did not mean. `kin diff`, `kin blame --ref` and
    /// `kin history --ref` all reach this through `resolve`, and the test above
    /// pins that they hold no resolver of their own, so a refusal here is their
    /// refusal.
    #[test]
    fn an_ambiguous_short_prefix_is_refused_naming_the_selector_and_the_count() {
        let first = id_with_prefix("adfc", 'a');
        let second = id_with_prefix("adfc", 'b');
        let other = id_with_prefix("beef", 'c');

        let error = resolve_change_prefix([first, other, second].into_iter(), "adfc", "adfc")
            .expect_err("two changes begin with adfc, so it names no single point in history");
        let message = format!("{error:#}");

        assert!(
            message.contains("'adfc'"),
            "the refusal must quote the selector the operator typed, or they cannot tell \
             which endpoint to lengthen: {message}"
        );
        assert!(
            message.contains("2 semantic changes"),
            "the refusal must say how many changes the prefix matches: {message}"
        );
        for candidate in [first, second] {
            assert!(
                message.contains(&candidate.to_string()[..12]),
                "the refusal must name candidate {candidate} so the operator can pick one: \
                 {message}"
            );
        }
    }

    /// The positive control: a prefix that names one change still resolves.
    ///
    /// Without it, a `resolve_change_prefix` that refused every prefix outright
    /// would pass the ambiguity test above, and the widening FIR-3015 landed
    /// would be gone with nothing red.
    #[test]
    fn a_unique_short_prefix_still_resolves() {
        let first = id_with_prefix("adfc", 'a');
        let second = id_with_prefix("adfc", 'b');
        let other = id_with_prefix("beef", 'c');

        let resolved = resolve_change_prefix([first, other, second].into_iter(), "beef", "beef")
            .expect("exactly one change begins with beef");
        assert_eq!(resolved, other);
    }

    /// A prefix nothing begins with is a different refusal from an ambiguous one.
    ///
    /// One says lengthen what you typed and the other says kin does not hold it.
    /// Collapsing them would send an operator hunting for a longer id that does
    /// not exist.
    #[test]
    fn a_prefix_no_change_begins_with_is_refused_as_absent_rather_than_ambiguous() {
        let error =
            resolve_change_prefix([id_with_prefix("adfc", 'a')].into_iter(), "dead", "dead")
                .expect_err("no change begins with dead");
        let message = format!("{error:#}");
        assert!(
            message.contains("no semantic change") && message.contains("'dead'"),
            "an absent prefix must say kin holds nothing under it: {message}"
        );
        assert!(
            !message.contains("ambiguous"),
            "an absent prefix is not an ambiguous one: {message}"
        );
    }

    /// One command, one refusal, whatever order the change map hands its keys in.
    ///
    /// The candidate ids come from a `HashMap`'s keys, whose iteration order is
    /// not promised between two runs of one binary. This used to stop collecting
    /// at `AMBIGUITY_PREVIEW` matches BEFORE sorting them, so the set a refusal
    /// named was whichever ones iteration reached first and two runs could print
    /// two different failures for one command. Six candidates, so the elision
    /// arm is exercised too.
    #[test]
    fn an_ambiguous_refusal_reads_the_same_whatever_order_the_candidates_arrive_in() {
        let ids: Vec<SemanticChangeId> = "abcdef"
            .chars()
            .map(|tail| id_with_prefix("adfc", tail))
            .collect();
        let mut reversed = ids.clone();
        reversed.reverse();

        let forward = format!(
            "{:#}",
            resolve_change_prefix(ids.iter().copied(), "adfc", "adfc")
                .expect_err("six changes begin with adfc")
        );
        let backward = format!(
            "{:#}",
            resolve_change_prefix(reversed.into_iter(), "adfc", "adfc")
                .expect_err("six changes begin with adfc")
        );

        assert_eq!(
            forward, backward,
            "the refusal must not depend on key order"
        );
        assert!(
            forward.contains("6 semantic changes"),
            "the count is exact, not a floor: {forward}"
        );
        assert!(
            forward.contains("and 2 more"),
            "a refusal that names only {AMBIGUITY_PREVIEW} of 6 must say so: {forward}"
        );
    }

    fn oid_with_prefix(prefix: &str, fill: char) -> GitObjectId {
        let mut hex = String::from(prefix);
        while hex.len() < GIT_SHA1_HEX {
            hex.push(fill);
        }
        parse_git_object_id(&hex).expect("constructed a whole SHA-1 object id")
    }

    /// `git:<short>` is the form `git log --oneline` prints, so it resolves when
    /// exactly one imported object begins with it, and a commit that both
    /// import records name is one candidate rather than two.
    #[test]
    fn a_short_git_id_resolves_to_the_one_imported_object_it_names() {
        let commit = oid_with_prefix("1971f65", 'a');
        let other = oid_with_prefix("479aa9b", 'b');
        let resolved = resolve_git_object_prefix(
            [commit, other, commit].into_iter(),
            "git:1971f65",
            "1971f65",
        )
        .expect("one imported object begins with 1971f65, named by both records");
        assert_eq!(resolved, commit);
        assert!(is_git_object_prefix("1971f65"));
        assert!(!is_git_object_prefix("197"), "shorter than Git's own floor");
        assert!(!is_git_object_prefix("1971F65"), "Git prints lowercase");
        assert!(
            !is_git_object_prefix(&commit.to_string()),
            "a whole id keeps the whole-id parser"
        );
    }

    fn change_with_prefix(prefix: &str, fill: char) -> SemanticChangeId {
        let mut hex = String::from(prefix);
        while hex.len() < CHANGE_ID_HEX {
            hex.push(fill);
        }
        parse_change_id(&hex).expect("constructed a whole change id")
    }

    /// A bare short value is what a developer types: the short Git id `git
    /// log --oneline` prints, or the start of a change id `kin log` prints.
    /// Either resolves without a tag when only one search holds it.
    #[test]
    fn a_bare_short_id_resolves_as_a_change_prefix_or_a_short_git_id() {
        let change = change_with_prefix("b54bd354", 'a');
        let imported = change_with_prefix("0cf20dd9", 'b');
        let commit = oid_with_prefix("f966a62", 'c');

        let by_git = choose_bare_prefix("f966a62", "f966a62", vec![], vec![(commit, imported)])
            .expect("a short Git id resolves to the change its import recorded");
        assert_eq!(by_git, imported);

        let by_change = choose_bare_prefix("b54bd354", "b54bd354", vec![change], vec![])
            .expect("a change-id prefix still resolves");
        assert_eq!(by_change, change);

        // A commit and the change recorded for it can share a prefix only by
        // accident, and when every match names one change that is the answer.
        let same = choose_bare_prefix("0cf2", "0cf2", vec![imported], vec![(commit, imported)])
            .expect("both searches name the same change");
        assert_eq!(same, imported);
    }

    /// A bare prefix both searches hold for different changes is refused with
    /// the tagged forms, and one neither holds says that both were searched.
    #[test]
    fn a_bare_short_id_both_searches_hold_is_refused_with_the_tagged_forms() {
        let change = change_with_prefix("adfc", 'a');
        let imported = change_with_prefix("1234", 'b');
        let commit = oid_with_prefix("adfc", 'c');
        let message = format!(
            "{:#}",
            choose_bare_prefix("adfc", "adfc", vec![change], vec![(commit, imported)])
                .expect_err("two different changes begin with adfc")
        );
        assert!(message.contains("ambiguous"), "{message}");
        assert!(
            message.contains("`change:adfc`") && message.contains("`git:adfc`"),
            "{message}"
        );

        let message = format!(
            "{:#}",
            choose_bare_prefix("dead", "dead", vec![], vec![])
                .expect_err("nothing begins with dead")
        );
        assert!(
            message.contains("no semantic change and no imported Git commit"),
            "{message}"
        );
    }

    /// A short Git id two imported objects share is refused with the count and
    /// the candidates, and one nothing begins with is refused as absent.
    #[test]
    fn a_short_git_id_is_refused_when_ambiguous_or_absent() {
        let first = oid_with_prefix("adfc", 'a');
        let second = oid_with_prefix("adfc", 'b');
        let message = format!(
            "{:#}",
            resolve_git_object_prefix([first, second].into_iter(), "git:adfc", "adfc")
                .expect_err("two imported objects begin with adfc")
        );
        assert!(message.contains("'git:adfc'"), "{message}");
        assert!(message.contains("2 imported Git objects"), "{message}");
        for candidate in [first, second] {
            assert!(message.contains(&candidate.to_string()[..12]), "{message}");
        }

        let message = format!(
            "{:#}",
            resolve_git_object_prefix([first].into_iter(), "git:dead", "dead")
                .expect_err("no imported object begins with dead")
        );
        assert!(
            message.contains("no Git object imported") && !message.contains("ambiguous"),
            "{message}"
        );
    }
}
