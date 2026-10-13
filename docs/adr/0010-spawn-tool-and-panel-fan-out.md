# ADR 0010: Spawn Tool + Reviewer Panel/Fan-Out Unification

Status: Accepted

## Context

ADR 0008 introduced `child_run` as the provenance-preserving substrate beneath
model-facing spawn, fan-out, and reviewer-panel refactors. ADR 0009 added
best-of-N fan-out but deliberately left two follow-up seams: expose child-run as
a model-facing tool, and close the dependency loop between `delegation`'s
N-reviewer panel and fan-out's `JudgePanel` reducer.

The constraints remain unchanged:

- spawned work must not be a side channel;
- tree-wide parent budget ceilings must still apply;
- child work must have a scoped `Ctx`, signed child ledger, parent
  `child_anchor`, and cancellation/deadline propagation;
- the existing delegation `Verdict` shape and reviewer semantics must not
  change.

## Decision

Truss adds an opt-in model-facing tool named `spawn`, gated by the CLI flag
`--spawn-tool`.

The tool input is:

- `task` (required string);
- `budget_slice` (optional `BudgetLimits` object with `max_total_tokens`,
  `max_cost_usd`, and `max_steps`);
- `scope` (optional provenance label).

The tool output is a JSON verified-result summary:

- `child_run_id`;
- `verdict.overall`, `verdict.summary`, and clause results;
- `artifacts` as `ArtifactRef` values;
- `ledger.verdict_ref`;
- `ledger.parent_anchor`;
- `ledger.child_root_hash` and `ledger.child_head_hash`;
- `usage` for the child slice.

`spawn` is registered like the governed memory tool: default-off, added only by
an explicit registration path. The CLI exposes that as `--spawn-tool`, and only
for store-backed durable/contract/daemon runs. Non-durable runs do not have a
signed `Store`, so they reject `--spawn-tool` instead of creating an unverifiable
side ledger.

When the durable loop calls `spawn`, it uses the existing
`Tool::call_with_ledger` provenance pointer to anchor the child into the durable
parent session that produced the tool call. The child itself runs through
`child_run::run_child`; no code path bypasses child-run. The child durable loop
records its own signed model/tool steps, and `child_run` records the
`child_result` plus parent `child_anchor`. Existing `child_started`,
`child_completed`, and `child_failed` events are reused.

Reviewer-panel unification is behavior-preserving: the no-stake panel
implementation now lives behind `fan_out::run_judge_panel`, and both
`delegation::run_review_panel` and fan-out's `Reducer::JudgePanel` call that one
helper. The public delegation API remains intact, and the strict-majority
semantics, adversarial lenses, prompt shape, evidence text, and `ClauseResult`
output are unchanged.

## Consequences

The spawn surface is intentionally narrow: it returns verified child-run
provenance, not a child transcript. Recursive spawn is not registered into child
tool registries by the CLI helper; children get the current non-spawn toolset
(including governed memory when enabled). This avoids accidental recursive
fan-out while preserving the tree-wide budget ceiling as the hard stop.

The panel refactor removes the one-off reviewer implementation without changing
signed verdict semantics. Existing delegation tests continue to assert the same
majority pass/fail behavior and reviewer-context handling, while fan-out tests
continue to cover `JudgePanel` selection.
