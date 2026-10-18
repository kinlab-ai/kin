# Entity edit leases

An entity edit lease coordinates Kin-managed body edits and deletions at one repository receiver. It does not grant write permission, replace source compare-and-swap, lock native files, or cover provider actions. A different receiver has its own authority and lease ledger.

Discover `kin_entity_leases` and `kin_entity_lease` through the existing tool discovery surface. They require an authenticated daemon connection and a live connection session. Control also requires the existing commit capability. Offline execution refuses these operations.

1. Call `kin_entity_leases` with `entity_ids`, containing one to sixteen distinct parsed entity UUIDs. The response includes each entity's exact current `source_hash` and any active lease. It returns no source bodies.
2. Call `kin_entity_lease` with a new `request_id` and `mutation: {operation: "acquire", entity_id, expected_source_hash}`. Copy `expected_source_hash` from the ownership read. The daemon fixes the lifetime at ten minutes and binds the repository, workspace and holder session.
3. Read the entity through the ordinary source tool. Submit a keyed `kin_mutate` with its usual source base and `entity_edit_lease_claims: [claim]`, copying the issued `claim` unchanged. The mutation still checks source freshness and ordinary permissions.
4. Renew or release through `kin_entity_lease`, using another `request_id` and `mutation: {operation: "renew" | "release", entity_id, authority_id, fence}`. Copy the authority and decimal fence from the issued lease. Renew starts another ten-minute interval using server time.

Only independent parsed declarations with current span-to-source provenance qualify. File, module and document containers, derived aliases, missing provenance and stale spans are refused. Source units above 16 MiB are outside this bounded path. Re-derive stale entities through the supported repository lifecycle; there is no whole-file source fallback.

While a lease is live, every publication through the same repository authority checks affected entity bodies. An unclaimed writer, a foreign session or a stale fence cannot publish a body edit or deletion. Ordinary transactions without leases retain their existing behavior. Changes outside the leased body are not locked. A direct filesystem edit can still change projected bytes, but subsequent Kin admission must satisfy the lease fence.

Fences are canonical decimal strings, preserving all 64 bits on JSON transports. The receiver authority UUID and monotonic fence watermark survive release, expiry and reopen. An old claim cannot become valid after reacquisition or on another receiver. First publication strips the receiver-local ledger rather than exporting its ownership.

Preserve the authenticated session, `request_id` and mutation after a lost response. An identical control retry returns its original outcome and expiry, never a renewed lease. A changed mutation under the same request key refuses. Read current ownership before resuming work: a replayed receipt describes what happened, not a promise that the lease is still active. Keyed source edits likewise retain their original mutation receipt after lease release or expiry.

Lease controls are durable repository operations without source changes. They preserve pending semantic derivation evidence. Snapshots and journals carrying this feature require newer readers; legacy stores without lease data retain their existing wire encoding. Acquire, renew and release provide coordination receipts, not additional approval or security authority.
