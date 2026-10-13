# Truss Self-Improvement Architecture (L2 now, L3 as north star)

> Status: design / roadmap. Captured 2026-06-17 from the #truss
> discussion. Decision (Matt): **L2 is the must-have; L3 is the goal but must not
> block L2.** This doc shows why building L2 correctly *is* building most of L3's
> substrate, so the two share one governance mechanism.

## The ladder

- **L0 — none.** Stateless; repeats mistakes forever. pi-class harnesses.
- **L1 — in-run self-correction.** Model reviews/revises its own work within one
  task (retries, self-review, the verified loop catching a failure and
  re-attempting). **Truss already has this** (contract → execute → adversarial
  N-review → signed verdict).
- **L2 — verified cross-run learning.** A lesson learned on one run changes
  behavior on a later run. **The must-have. This is our wedge.**
- **L3 — self-tuning the harness.** The system adjusts its own config (routing
  thresholds, persona, tool selection, verification policy, salience) from
  measured outcomes. **North star; non-blocking.**
- **L4 — recursive self-modification.** Agent rewrites its own code / spawns
  improved successors. Governance hard-line; explicitly NOT a target.

## Why L2 is the differentiator

Every harness can "write notes to memory." That's the trap: **ungoverned
self-learning poisons itself** — the agent stores a wrong lesson as fact and
compounds it. Most products quietly accept that rot.

Truss already has the antidote (built for other reasons): trust tiers
(unverified→verified), the N-reviewer panel that can gate a belief the same way
it gates a result, and provenance — every belief chains back to the run that
produced it in a signed ledger. So our L2 is not "self-improvement," it is
**verified, auditable, reversible self-improvement**: a learned belief is
quarantined until an independent panel confirms it against evidence; if it later
causes a regression, the ledger lets you trace the exact lesson and revoke it.
That is the enterprise/regulated-industry version of L2. No frontier harness
ships it today.

## The unifying insight: L2 and L3 are the same mechanism, different write-target

- **L2** = an automated proposer writes **beliefs/lessons** → governed store →
  panel-gated promotion → recalled into future runs.
- **L3** = an automated proposer writes **policy/config deltas** → governed store
  → panel-gated promotion → applied to future runs.

Both reuse the *same* rails: trust tiers, `delegation::Verdict` (N-reviewer),
provenance ledger, reversibility (supersede/demote), and the `child_run`
substrate for evaluation. **Therefore: if we give policy/config the same
governed-write shape as memory now, L3 becomes incremental — no new trust model,
no new safety story.** That is exactly how L3 stays a goal without blocking L2.

## L2 — buildable plan (the must-have)

Milestones, each ~1 PR + ADR section, all knobs configurable (per Directive 1):

- **SI-1 Lesson extraction at contract close.** After a verified verdict, a step
  proposes generalizable lessons → written as `EntryKind::Lesson` *Unverified*
  entries with provenance (the run's `LedgerRef`). Policy: `off | on | model-scored`.
- **SI-2 Lesson promotion.** Route lessons through the existing N-reviewer panel
  (`delegation::Verdict`) to gate Unverified→Verified against evidence. Reuses M3
  trust-tier machinery. Reversible: a regression supersedes/demotes the lesson.
- **SI-3 Lesson recall.** Promoted lessons feed the M4 layered L2 injection
  (salience + scope already enumerated knobs). Verified-only by default.
- **SI-4 Paired eval harness.** Run the same contract learning-on vs learning-off
  via `child_run`; compare verdict pass-rate + cost. This *proves* L2 helps
  instead of asserting it — and it is the signal source L3 will optimize against.

Shares substrate with build-order #2 (spawn/fan-out): best-of-N fan-out produces
higher-quality lesson candidates and SI-4's paired evals ride `child_run`.

## L3 — forward seams to bake in now (so it never becomes a rewrite)

Do NOT build the optimizer yet. Just make the target shape right:

- **Tunables become typed, versioned, signed `Policy` objects** living in the
  same governed store with provenance — structurally identical to a memory entry
  / identity amendment. (Routing thresholds, persona `self` layer, tool-set,
  `VerificationPolicy`, salience, `goal_repin_every`, etc.)
- **L3 = "an optimizer proposes a policy delta"** → same panel + provenance +
  reversible-ledger gate before it takes effect. The optimizer can start dumb
  (bandit/grid over enumerated knobs using SI-4 eval results) and later become a
  small **local** routing/tuning model (ties to the local-first + smart-routing
  pillar).
- **Safety lines (hard):** `l0_core` identity + operator-signed boundaries are
  never auto-tunable. Every L3 proposal is reversible, signed, panel-gated, and
  high-stakes deltas can require a human approval gate. Never crosses into L4
  (self-rewriting code).

## Sequencing recommendation

1. **Best-of-N fan-out** on `child_run` (build-order #2) — also the lesson-quality
   and eval engine.
2. **SI-1 → SI-2 → SI-3** (the L2 learning loop).
3. **SI-4** paired eval harness (proves L2; feeds L3).
4. In parallel/throughout: give policy/config the governed-write shape (L3 seam),
   but ship no optimizer until L2 is proven by SI-4.

Result: L2 delivered and *measured*; L3 reachable by adding one proposer against
rails that already exist.
