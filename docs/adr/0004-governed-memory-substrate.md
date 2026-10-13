# ADR 0004 — Governed memory substrate

- **Status:** Proposed
- **Date:** 2026-06-16
- **Deciders:** Matt (owner), ALINA
- **Builds on:** [Memory & Identity Architecture](../memory-identity-architecture.md)

## Context

Truss needs durable memory that is typed, trust-tiered, and provenance-tracked
instead of flat markdown injection. M1 establishes the substrate only: storage,
content-addressing, trust state transitions, and deltas. Tool binding,
verification drivers, persona injection, salience policy, L2 scope, and conflict
resolution are layered on by later milestones.

## Decision

Add `truss_core::memory` with a substrate-agnostic `MemoryStore` trait and an M1
`SqliteMemoryStore` implementation. The SQLite implementation shares the durable
execution database connection, stores model writes as `Unverified`, persists
ledger provenance, persists promotion verdict JSON, and uses a monotonic
`write_seq` cursor for delta rehydration.

`MemoryId` is content-addressed as lowercase SHA-256 hex over `kind || key ||
body`. `Fact` and `Lesson` entries supersede the active prior entry with the same
kind/key; superseded entries remain auditable but are not returned by default
queries. Default queries return only active `Verified` entries.

## Consequences

This gives later milestones a governed storage boundary without changing the
agent loop yet. M4 resolves the formerly-deferred L2 scope and verified-conflict
questions as configurable policy knobs rather than one hard-coded answer:
`scope = "per-agent"` by default and `conflict = "verified-wins"` by default.
Salience remains an M5 implementation seam with a default-off configurable field.
