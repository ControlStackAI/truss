# ADR 0013: Verified Lesson Recall Into L2 Context

Status: Accepted

## Context

ADR 0011 added SI-1 lesson extraction: contract-close learning can write
`EntryKind::Lesson` records as `Unverified`. ADR 0012 added SI-2 promotion:
lessons become `Verified` only after the reviewer panel signs the claim, and
superseded lessons are demoted to `Stale`.

`docs/self-improvement-impl-plan.md` Step 5 completes the L2 loop. A verified
lesson must influence a later run, but only through the existing M4 layered
context machinery and only under the same trust and provenance rules as other
governed memory.

## Decision

Truss adds verified lesson recall to the existing M4 L2 memory injection path.
At fresh durable run start, the loop asks the configured `MemoryStore` for
active `EntryKind::Lesson` entries using the existing verified-only query
shape:

- `kind = Lesson`;
- `min_trust = Verified`;
- `Rejected`, `Unverified`, and `Stale` entries are excluded by the store/query
  trust invariant;
- no vector store or separate injector is introduced.

The selected lessons are rendered as an L2 message block labeled
`[L2 verified learned lessons]`. The label is intentional: learned lessons are
not silently blended into ground truth or L0/L1 identity. They remain auditable,
trusted L2 context.

## Relevance And Budget

Recall uses the current task context (contract goal/definition of done and the
fresh prompt) and ranks candidates by simple lexical overlap with the lesson
body/key, then by existing salience metadata and recency. This matches the
current store's simple query/scoring level; M5 can improve retrieval behind the
same store trait without changing the injection layer.

Two new layered `MemoryConfig` knobs bound recall:

- `lesson_recall_max_lessons` defaults to `5`;
- `lesson_recall_token_budget` defaults to `512` approximate tokens.

Both resolve through the existing global → agent → contract layering. Setting
either to `0` disables lesson recall. If no verified lesson is relevant, Truss
injects no L2 lesson block, so the run pays no additional prompt-token cost.

## Provenance

Promotion now persists the durable ledger reference for the signed
`memory_verdict` event when a journal is available. `Provenance` therefore keeps
both:

- `verified_by`: the signed reviewer-panel `Verdict`;
- `verified_by_ref`: the `LedgerRef` of the journal event that recorded that
  verdict.

Injected lessons render their producing ledger ref and verifying verdict ledger
ref, so a recalled belief can be traced back to the run and panel that verified
it. Existing `Store::verify_chain` continues to validate the chain.

## Consequences

SI-3 closes the write → promote → recall L2 loop: lessons written by SI-1 and
promoted by SI-2 can now affect future runs through trusted L2 context.

The trust invariant is deliberately narrow. Only `Verified` lessons enter this
trusted injection path. Unverified, rejected, and stale lessons remain available
for audit and governed tool views where policy allows, but they are not injected
as trusted run-start context.

## Deferred

SI-4 is still required to prove that learning helps instead of merely existing.
The next and final L2 step is the paired learning-on/off evaluation harness,
comparing verdict pass-rate and cost with recall enabled versus disabled.
