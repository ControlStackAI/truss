# ADR 0012: Lesson Promotion Through the Reviewer Panel

Status: Accepted

## Context

ADR 0011 added SI-1 lesson extraction: contract-close learning can write
`EntryKind::Lesson` records as `Unverified`, with provenance pointing at the
signed contract verdict that produced the candidate lesson. Those records are
not trusted on write.

`docs/self-improvement-impl-plan.md` Step 4 adds SI-2: the L2 learning loop must
be useful without becoming self-poisoning. A model-authored lesson may influence
future runs only after an independent panel verifies the claim against its
evidence, and the decision must be reversible if a later verified lesson
supersedes or contradicts it.

## Decision

Lesson promotion reuses the existing M3 trust-tier machinery and the existing
delegation reviewer panel:

- extracted lessons still land as `Trust::Unverified`;
- the promotion gate calls `delegation::run_review_panel` through the existing
  memory-claim verifier, now including the lesson claim, provenance ledger refs,
  and the producing contract verdict evidence;
- a passing `delegation::Verdict` calls `MemoryStore::promote`, which stores the
  verdict in `Provenance.verified_by`;
- a failing or unverifiable verdict calls `MemoryStore::reject`, retaining the
  record for audit while excluding it from trusted views;
- the promoting/rejecting verdict is also recorded as a signed
  `memory_verdict` journal event when a durable store is available, so normal
  ledger verification validates the promotion trail.

No new policy knob is introduced. Lesson promotion uses the resolved
`MemoryConfig.verification` (`VerificationPolicy`) layered
global → agent → contract:

- `off`: never auto-promote; lessons remain honestly labeled `Unverified`;
- `lazy`: do not promote at extraction time; first recall through the governed
  memory tool runs the same panel before surfacing the lesson as trusted;
- `eager`: run the panel at extraction/write time;
- `tiered`: keeps the existing tiered semantics from M3.

The invariant is explicit: `Verified` always means "the panel signed off." The
policy changes when and which entries are verified, never the meaning of the
label. Unverified lessons are excluded from the default trusted memory query.

## Reversibility

Reversibility stays in the M3 store path. `Fact` and `Lesson` entries supersede
by key. If a newer candidate with the same key is later promoted, the prior
verified entry is demoted to `Trust::Stale` and linked through
`superseded_by`. The stale record, the new verified record, and both signed
verdicts remain in the ledger-backed audit trail.

## Consequences

SI-2 completes the trust moat for extracted lessons: lesson recall can later use
verified lessons without inventing a separate safety story. The cost/control
surface remains the user-selectable `VerificationPolicy`, so operators can pick
eager freshness, lazy amortized verification, or off without weakening the
meaning of `Verified`.

The verifier still judges only the evidence passed into the panel. SI-3 recall
must continue to default to trusted/verified views and avoid pushing unverified
lessons into future prompts.
