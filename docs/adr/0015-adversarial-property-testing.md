# ADR 0015: Adversarial and Property Testing for Trust Claims

Status: Accepted

Date: 2026-06-17

## Context

Truss sells trust: verified results, tamper-evident durable ledgers, bounded
child execution, and governed memory that does not silently promote unreviewed
claims. Happy-path tests are not enough for that claim. Diligence needs negative
tests that prove forged or corrupted evidence is rejected, and randomized
invariant tests that keep core trust properties from regressing as orchestration
paths multiply.

## Decision

Add two standing trust-test categories to `truss-core`:

- An adversarial tamper suite that starts from valid signed ledgers, mutates the
  persisted state, and asserts verification rejects the result.
- Modest-count `proptest` invariant tests that randomize operation histories and
  assert the trust ladder remains coherent.

The adversarial suite covers:

- corrupt stored entry hash
- flipped signature byte
- reordered ledger entries
- replayed/duplicated ledger entry
- truncated chain with dropped head
- forged verdict signature mismatch
- repointed child anchor head

The property suite covers:

- `Verified` memory implies an attached panel verdict and signed
  `memory_verdict` ledger ref
- ledger hash chains validate after arbitrary append sequences
- branch step usage never exceeds the parent step ceiling for arbitrary branch
  counts and slice sizes
- default memory queries surface only `Verified` entries
- stored entries carry resolvable ledger provenance

## Consequences

These tests become part of the normal PR gate and are intentionally hermetic:
in-memory SQLite, fake providers where needed, and no network. Test-only ledger
mutation hooks are allowed only to simulate persistence-layer tampering; they do
not weaken append-time production invariants.

The suite also identified and fixed one verification gap: `verify_chain` now
checks the session head hash against the final journal entry, so deleting the
tail/head entry is detectable instead of verifying the remaining prefix.
