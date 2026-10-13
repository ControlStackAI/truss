# ADR 0008: Child-Run Substrate

Status: Accepted

## Context

`docs/extensibility-architecture.md` identifies child-run as the keystone
primitive beneath model-facing spawn, best-of-N fan-out, and the eventual
N-reviewer panel refactor. The core constraint is that child work must not become
a side channel: all lifecycle, budget, and provenance facts must flow through the
existing `Ctx`, events, and signed durable ledger.

## Decision

Truss adds an internal `child_run` substrate in `truss-core`.

A child-run executes a sub-task with a scoped `Ctx`:

- cancellation uses `Ctx` child tokens, so cancelling the parent cancels the
  child;
- the child deadline is the tighter of the parent's inherited deadline and any
  requested child timeout;
- the child can inherit the parent `Principal` or run under an explicitly tighter
  principal/scope label;
- spend is recorded into the parent's shared tree meter while an additional
  per-child `BudgetLimits` slice is enforced.

The result is a verified result, not a transcript:

- `child_run::ChildRunOutput` contains the existing `delegation::Verdict` plus
  produced `ArtifactRef`s;
- the runner records the result in the child ledger as `child_result`;
- the returned `VerifiedChildResult` includes the verdict, artifacts, the child
  root/head hashes, the `verdict_ref`, and the parent anchor ref.

## Event Vocabulary

The typed `AgentEvent` stream now includes:

- `child_started` — parent id, child id, task, scope, and budget slice;
- `child_completed` — parent id, child id, child head hash, verdict ref, and
  artifacts;
- `child_failed` — parent id, child id, and error.

These events are the future UI/SDK projection point for spawn and fan-out.

## Ledger Anchoring

The child ledger is its own signed session. The parent ledger records a
`child_anchor` entry containing:

- `parent_run_id`
- `child_run_id`
- `child_root_hash`
- `child_head_hash`
- `task`
- `scope`
- `budget_slice`
- `status`
- `verdict_ref` when completed
- `error` when failed

`Store::verify_chain` now recursively verifies `child_anchor` payloads: the parent
chain must verify, the child chain must verify, and the child's current root/head
hashes must match the anchor. The CLI `--verify` path uses the same store method,
so parent-to-child provenance is checked by the existing verification command.

## Budget Slice Semantics

Budget governance is tree-aware:

- each child has a local slice meter that enforces its own ceiling;
- every child usage record also increments the parent's shared tree meter;
- multiple child-runs therefore cannot collectively exceed the parent ceiling,
  even when each individual slice is larger than the remaining parent budget.

The existing `BudgetMeter` still has root-run semantics when no slice is present.

## Follow-Up Seams

This ADR deliberately stops before the model-facing surface.

- Spawn tool: expose `child_run::run_child` as a tool that packages a model task
  into `ChildRunOptions` and returns a `VerifiedChildResult`.
- Best-of-N fan-out: launch K child-runs with sibling budget slices and reduce
  their verified results.
- Reviewer panel refactor: the current N-reviewer panel is structurally a fan-out
  instance. It should be re-expressed on top of child-run/fan-out after this
  substrate is reviewed, not changed in this PR.

## Consequences

The primitive is conservative: no new transcript store, no special event bus, and
no parallel reviewer rewrite. The cost is that child-run users must produce a
`Verdict` and artifacts explicitly, but that keeps the trust boundary aligned with
the rest of Truss: signed verified evidence first, model-facing ergonomics later.
