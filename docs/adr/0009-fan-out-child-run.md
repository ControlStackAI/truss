# ADR 0009: Best-of-N Fan-Out on Child-Run

Status: Accepted

## Context

ADR 0008 introduced the child-run substrate as the provenance-preserving
primitive beneath spawn, best-of-N fan-out, and the eventual reviewer-panel
refactor. Step 1 of `docs/self-improvement-impl-plan.md` builds fan-out first
because it unlocks both the lesson-quality engine and paired-eval substrate
without changing the trust model.

The constraint is the same as child-run: fan-out must not execute work through a
side channel. Every branch is a child-run, with its own scoped `Ctx`, budget
slice, child ledger, and parent `child_anchor`.

## Decision

Truss adds `truss_core::fan_out` as a thin orchestration layer over
`child_run::run_child`.

`run_fan_out` launches `k` sibling child-runs of the same task concurrently,
collects the verified `ChildRunOutput` results, and applies a pluggable
`Reducer`. It returns the winning child plus the signed parent-ledger reduction
decision.

The reducer set is:

- `Vote` — strict majority over verified child `overall` verdicts. This is the
  default because it needs no external scorer or model reviewer dependency.
- `JudgePanel` — reuses the existing `delegation::run_review_panel` no-stake
  reviewer machinery to evaluate candidate child results. This PR exposes that
  helper instead of copying or forking the panel logic.
- `BestScore` — calls a caller-supplied `BestScore` trait object and picks the
  highest finite score.
- `FirstSuccess` — picks the first child whose verified verdict is `Pass`, then
  cancels sibling branches.

The serializable reducer knob is `FanOutReducerKind` / `ReducerConfig`; concrete
runtime dependencies such as reviewer providers and scorer trait objects are
bound by the caller.

## Budget and Cancellation Semantics

Fan-out creates a same-run cancellation scope for its sibling branches. That
scope preserves the parent run id, so child-run anchors still land in the parent
ledger, but it can be cancelled independently of the caller. `FirstSuccess` uses
that scope to cancel losing branches after the first passing verified result.

Parent cancellation and deadlines propagate through the same `Ctx` child-token
chain as ordinary child-runs.

Budget remains tree-aware:

- every branch receives a per-child `BudgetLimits` slice;
- branch spend accrues to the shared parent `BudgetMeter`;
- fan-out does not create a separate meter or bypass `child_run`;
- step accounting saturates at the configured step ceiling so concurrent sibling
  branches cannot collectively push the shared step counter past the parent
  limit before enforcement observes it.

## Event Vocabulary

The typed `AgentEvent` stream now includes:

- `fanout_started` — parent run id, fan-out id, task, `k`, and reducer kind;
- `fanout_completed` — reducer kind, per-child anchor/result refs, winner, why,
  and the parent-ledger decision ref.

These events are the future UI/SDK projection point for best-of-N work.

## Ledger and Verification

Child branches are still anchored by the existing `child_anchor` entry from ADR
0008. Fan-out adds one parent-ledger entry:

- `fanout_reduction` — parent run id, fan-out id, reducer kind, all child refs,
  winner, and reducer explanation.

`Store::verify_chain` now validates fan-out decisions in addition to recursively
validating child anchors. A `fanout_reduction` must reference real child anchors
in the same parent ledger; winner refs must point at one of those completed
children and match its recorded `verdict_ref`.

The CLI `--verify` path uses `Store::verify_chain`, so parent-to-fan-out-to-child
provenance is checked by the existing verification command.

## Follow-Up Seams

This PR deliberately leaves two surfaces unbuilt:

- Model-facing spawn tool: expose `child_run::run_child` as a tool that packages
  a model-requested subtask into `ChildRunOptions`.
- Reviewer-panel refactor: the current N-reviewer panel is structurally a
  fan-out instance, but this PR only reuses its helper for `JudgePanel`. A later
  PR should re-express the panel itself on top of fan-out after the primitive is
  reviewed.

## Consequences

Fan-out inherits child-run's trust boundary: branches produce verified results,
not transcripts, and parent provenance remains a signed hash chain. The cost is
that callers must provide a cloneable branch runner and, for advanced reducers,
bind reviewer providers or scoring logic explicitly. That is acceptable because
the model-facing ergonomics are intentionally deferred to the spawn-tool PR.
