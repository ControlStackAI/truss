# ADR 0022: Compaction Anticipation

Status: Accepted

Date: 2026-06-18

## Context

ADR 0020 made context pressure observable from the already-projected provider
input token count. ADR 0021 added opt-in governed compaction at step boundaries
when pressure reaches the configured threshold. CC-3 answers the next control
question: whether Truss can warn and compact one boundary ahead of threshold
crossing without introducing a second compaction authority or changing existing
runs.

## Decision

`ContextGrowthEstimator` tracks a short moving average of recent per-step input
token deltas. It is fed only with provider-reported projected input token counts,
the same counts used by `ContextGauge`, so it does not run a tokenizer or inspect
message bodies. If the moving average is zero or unavailable, no
steps-to-threshold estimate is produced.

`CompactionPolicy` gains two safe-default knobs:

- `anticipate = false`, preserving CC-2 behavior unless explicitly enabled.
- `lookahead_steps = 1`, the horizon used when anticipation is enabled.

When compaction mode is enabled and `anticipate` is true, the durable loop emits:

```json
{"event":"compaction_imminent","steps_to_threshold":1,"pressure":0.60,"window":100}
```

at a step boundary when the estimate is within the configured horizon. If the
current pressure is still below threshold, the same CC-2 `maybe_compact_history`
path runs immediately, producing the normal signed `compaction_performed` journal
entry and governed context-summary memory entry.

## Consequences

Default runs, including CC-2 users who enabled threshold compaction but did not
enable anticipation, keep identical behavior: no `compaction_imminent` event and
no early compaction. Threshold-triggered compaction remains unchanged.

The journal remains authoritative. CC-3 does not delete journal entries, compact
mid-step, add an eval harness, or configure provider-side `context_management`.
Those evaluation and interop questions remain CC-4.
