# ADR 0023: Compaction Eval Harness

Status: Accepted

Date: 2026-06-18

## Context

ADR 0020 made context pressure observable, ADR 0021 added opt-in governed
compaction, and ADR 0022 added compact-ahead anticipation. CC-4 needs to put a
number on what those controller primitives buy back without changing runtime
behavior or running live benchmarks.

The eval must be hermetic: no network, no API keys, no Docker, and no
SWE-bench. The journal remains authoritative, and the eval only observes the
normal event stream and signed ledger entries produced by durable runs.

## Decision

`truss-core::compaction_eval` runs the same deterministic scripted workload
under three arms:

- `OFF`: compaction disabled.
- `THRESHOLD`: governed CC-2 summarization at the configured threshold with
  `anticipate = false`.
- `ANTICIPATE`: the same governed policy with CC-3 `anticipate = true`.

The scripted provider does not call any external model. It emits the same tool
use sequence in every arm and reports deterministic input-token usage from the
request shape it receives. Tool results carry explicit synthetic token weights;
a compacted context-summary message has a fixed synthetic weight. That keeps the
run reproducible while still letting existing projection and compaction behavior
change the provider-reported input-token count that `ContextGauge` observes.

Metrics are collected from the existing event and journal stream:

- completed, and completed without crossing the context wall,
- peak observed context pressure,
- `compaction_performed` ledger entries,
- model steps completed,
- total configured verbatim tail messages preserved across compactions,
- tokens summarized away, computed as the signed ledger `before_tokens -
  after_tokens` totals,
- momentum-preserved steps.

The momentum proxy is:

```text
momentum_preserved_steps =
  count of model steps completed before the first context_pressure > 1.0
```

If no overflow pressure is observed, it equals total steps completed. This is a
synthetic pressure-headroom proxy: it says whether the controller kept the
scripted run away from the context wall. It does not prove model quality, task
success on real software work, human momentum, or SWE-bench performance.

A small example prints the comparison table:

```bash
cargo run -p truss-core --example compaction_cc4_eval
```

## Consequences

The eval harness adds no new controller behavior and does not alter existing run
semantics. It reuses `ContextGauge`, `CompactionPolicy`, durable compaction, the
event stream, and signed journal entries.

The default workload is intentionally small enough for unit tests and PR review.
It is useful for regression detection and for comparing controller arms, but its
numbers must be described as synthetic.

To replace the scripted workload later, keep the report shape and metrics but
swap the provider/workload driver:

1. run a live in-loop Truss workload or SWE-bench instance through the same
   durable loop,
2. keep CC-4's arm definitions and metric collection unchanged,
3. derive token and compaction metrics from the real provider's usage events and
   Truss journal, and
4. separately record task-quality verdicts from the live grader.

That live version is intentionally out of scope for CC-4.
