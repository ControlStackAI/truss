# Self-Improvement Implementation Plan

> Status: active build plan. Captured 2026-06-17. Companion to
> `self-improvement-architecture.md` (L2 now / L3 north-star, non-blocking) and
> `extensibility-architecture.md` (child-run keystone, merged v0.17.0).
> Each step = ~1 PR + 1 ADR section. All policy knobs configurable (Directive 1).

## Where we start and why

The child-run substrate (v0.17.0) is the keystone. Step 1 is **best-of-N fan-out**
on top of it, because that single primitive is *also* the lesson-quality engine
and the paired-eval engine the L2 loop needs — so it is the shortest path that
serves both the missing base primitive AND self-improvement.

## Steps

### Step 1 — Best-of-N fan-out on child-run  (PR, v0.18.0)  ← BUILD NOW
Run K child-runs of the same task concurrently (reusing `child_run`), collect
their verified results, apply a `Reducer`. Reducers: `Vote`, `JudgePanel`
(reuse `delegation` reviewers), `BestScore`, `FirstSuccess`. New events
`fanout_started` / `fanout_completed` (per-child refs + which child won + why);
ledger records the reduction decision and anchors each child. Hermetic tests.
Explicitly leave as follow-up seams: the model-facing spawn tool, and refactoring
the existing N-reviewer panel onto fan-out.

### Step 2 — Spawn tool + panel refactor  (PR, v0.19.0)
Expose `child_run` as a model-facing **spawn** tool (model can delegate a scoped
sub-task and get a verified result back). Refactor the existing N-reviewer panel
to be a `JudgePanel` fan-out instance (dedup the one-off). No new trust model.

### Step 3 — SI-1 lesson extraction  (PR)
At contract close, after a verified verdict, a step proposes generalizable
lessons → `EntryKind::Lesson` written *Unverified* with provenance (`LedgerRef`
to the producing run). Policy: `extraction = off | on | model-scored`.
**Also start the L3 seam here:** introduce a typed/versioned/signed `Policy`
governed-write object shape (same store/provenance as memory) — no optimizer.

### Step 4 — SI-2 lesson promotion  (PR)
Route lessons through the N-reviewer panel (`delegation::Verdict`) to gate
Unverified→Verified against evidence. Reversible: regression supersedes/demotes.
Reuses M3 trust-tier machinery end-to-end.

### Step 5 — SI-3 lesson recall  (PR)
Promoted lessons feed the M4 layered **L2** injection. Verified-only by default;
salience + scope are the already-enumerated knobs.

### Step 6 — SI-4 paired eval harness  (PR)
Run the same contract learning-on vs learning-off via child-run/fan-out; compare
verdict pass-rate + cost; emit a report. Proves L2 helps; becomes the signal
source an L3 optimizer would later optimize against.

## L3 (north star — NOT built in this plan)
Once L2 is proven by SI-4: add a policy optimizer (dumb bandit/grid over
enumerated knobs → later a small local tuning model) that *proposes policy
deltas* through the SAME panel + provenance + reversible gate. `l0_core` /
operator boundaries never auto-tunable; high-stakes deltas can require a human
gate; never L4 (self-rewriting code).

## Gate discipline (every step)
`cargo fmt --check` · `cargo clippy --all-targets --all-features -- -D warnings` ·
`cargo test` · `cargo test --all-features` · `cargo build --release`. ALINA
re-runs gates on merge; PRs left open for review; ALINA tags on merge.
