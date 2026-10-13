# ADR 0027: Ledger Read Tools LR-1

Status: Accepted

Date: 2026-06-18

## Context

Truss already records a signed, hash-chained durable journal for each run. Before
LR-1, the supported read surface was effectively `truss --verify`, which answers
whether a session verifies but does not show the proof chain itself. The product
direction is to review proofs, not transcripts, so the ledger needs a read API
that can list runs, inspect entries, and retrieve exact payloads without mutating
the store.

## Decision

Add a read-only `LedgerReader` in `truss-core` that composes over the existing
`Store`. It exposes session listing, session lookup, ordered entries, last entry,
head hash, anchor count, and chain verification. The only new store primitive is
`Store::list_sessions`, a read-only query over existing `sessions` and `journal`
tables. No schema changes or write paths are introduced.

Add `truss ledger` CLI commands in `truss-cli`:

- `truss ledger list`
- `truss ledger show <run-id>`
- `truss ledger get <run-id> <seq>`

Each command supports `--json`. JSON responses use schema version
`truss.ledger.v1` and include verification status alongside content. Human output
also shows verification state. Tampered or unsigned entries are surfaced as
`unverified`; readers do not panic or silently present them as trusted.

## JSON Schema

LR-1 intentionally keeps the schema small and stable:

- list: `{ schema_version, command: "list", sessions: [...] }`
- show: `{ schema_version, command: "show", session: { session_id, head_hash, verification, entries } }`
- get: `{ schema_version, command: "get", session_id, entry: { seq, kind, hash, short_hash, signed, payload, verification_status } }`

Session and entry records include hash and verification metadata so future UI and
diligence tooling can consume the read API without interpreting terminal text.

## Consequences

The ledger can now be inspected without running providers or changing durable
state. This unblocks later LR milestones:

- LR-2 verdict bundle and evidence export
- LR-3 recall and verify command consolidation
- LR-4 run diffing

Those later views are explicitly out of scope for LR-1.
