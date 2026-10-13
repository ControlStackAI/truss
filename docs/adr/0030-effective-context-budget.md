# ADR 0030: Effective Context Budget Gauge

Status: Accepted

Date: 2026-06-18

## Context

Provider context windows are nominal ceilings, not reliable attention budgets.
The Compaction Controller previously measured `context_pressure`,
threshold-triggered compaction, and compact-ahead anticipation against
`Provider::context_window()`. That can delay governed compaction until a model is
already in its degraded attention band.

`docs/effective-context-architecture.md` defines EC-1 as the first step: measure
against a per-model effective budget while leaving verified offload, reference
mode, and eval sweeps for later milestones.

## Decision

Add an EC-1 effective-context budget surface:

- `Provider::effective_context_budget()` plus model-aware
  `effective_context_budget_for(model)`, both defaulting to `0.60 * nominal`.
- `EffectiveBudgetPolicy { ratio, overrides }`, where exact model-id overrides
  win over the ratio-derived budget.
- `EffectiveBudgetPolicyLayer` resolved global -> agent -> contract alongside
  the existing memory compaction layers.
- `[memory.effective_budget]` TOML and CLI flags
  `--effective-budget-ratio` / `--effective-budget-override MODEL=TOKENS`.

The Compaction Controller pressure denominator now uses the resolved effective
budget only when governed compaction is enabled. With the default compaction
mode still `off`, existing runs keep their nominal-window pressure events and
request shapes unless an operator opts into compaction.

## Consequences

Effective pressure, compaction thresholds, and compact-ahead events now fire
against the budget Truss expects the model to use reliably, not the provider's
hard limit. The default ratio path is conservative and never expands nominal
headroom. Per-model overrides provide a place for measured budgets such as
Opus-class limits without hardcoding them in the loop.

This is paired-eval-ready: EC-4 can sweep ratios and overrides on real
contracts, then write measured model-id overrides back into config.

Out of scope for this ADR: pressure-triggered verified offload, reference-mode
context, and automatic budget tuning.
