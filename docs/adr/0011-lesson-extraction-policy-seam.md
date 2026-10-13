# ADR 0011: Lesson Extraction + Governed Policy Seam

Status: Accepted

## Context

`docs/self-improvement-impl-plan.md` Step 3 starts SI-1: after a contract
produces a signed, evidence-backed verdict, Truss may propose durable lessons
from that run. The lesson is not trusted on arrival. It is L2 candidate material:
written as `Unverified`, tied to the producing ledger event, and withheld from
trusted recall until a later promotion milestone.

`docs/self-improvement-architecture.md` also identifies the L3 seam: L2 lesson
writes and L3 policy/config writes should use the same governance rails. L2
writes beliefs; L3 later writes policy deltas. Both need trust tiers,
provenance, signed ledger references, panel-gated promotion, and reversibility.

## Decision

Truss adds `LessonExtractionPolicy` to the layered `MemoryConfig`:

- `off` is the default-safe mode. It performs no extractor model call and writes
  no memory.
- `on` runs the extractor after the contract verdict is recorded.
- `model-scored` runs the same extractor, but only writes lessons whose model
  salience is at or above `extraction_min_salience` (default `0.70`).

Extraction runs only after the verdict journal entry lands. Each accepted lesson
is written through the existing `MemoryStore` as `EntryKind::Lesson` with
`Trust::Unverified`. Its `Provenance.produced_by` points at the contract
`verdict` ledger entry, and the same `LedgerRef` is included as a source. The
existing `AgentEvent::MemoryWrite` event is emitted for each lesson.

The extractor uses the existing provider path (`Provider::complete`) and is
bound by the caller via `ContractCloseLearning`. When policy is `off`, Truss
returns before touching the provider, so the feature is zero-cost in the default
configuration.

Truss also adds a governed policy object seam:

- `memory::Policy` is a typed, versioned policy object.
- `memory::PolicyTarget` enumerates tunable policy surfaces such as routing,
  `PersonaSelf`, tool-set, verification, salience, and goal repinning.
- `memory::PolicyRecord` mirrors memory's governance shape: `Trust`,
  `Provenance`, creation time, and supersession pointer.
- `SqliteMemoryStore` stores policy records in the same SQLite-backed governed
  substrate as memory. A policy record's provenance points to a signed journal
  entry, so signature validation remains `Store::verify_chain`.

## Deferred

This ADR does not build SI-2 promotion. Lessons remain unverified and are not
auto-promoted.

This ADR does not build an L3 optimizer, proposer, evaluator, auto-application
path, or policy-delta promotion flow. Policy records can be written and read
back, but nothing applies them to runtime behavior.

The L0 core identity and operator-signed boundaries are never auto-tunable. They
are intentionally absent from `PolicyTarget`; future L3 work must preserve that
boundary even when adding proposers and promotion gates.

## Consequences

SI-1 is useful but conservative: contract runs can now produce durable lesson
candidates, but default trusted memory views still exclude them because
`MemoryQuery::default()` remains verified-only.

The L3 seam is now incremental. A future optimizer can propose policy records
through the same store/provenance/trust shape instead of inventing a separate
configuration trust model.
