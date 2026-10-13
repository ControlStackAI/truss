# ADR 0020: Context Pressure Gauge

Status: Accepted

Date: 2026-06-18

## Context

Long in-loop runs can approach a model's context limit before the harness has a
first-class signal that it is near the wall. The compaction-controller design
splits the work into milestones; CC-1 must be measurement-only so existing runs
do not change behavior before governed compaction is designed and tested.

Truss already assembles the request at each model step and records provider
usage. The provider-reported input token count is the best available
already-computed projected prompt size for the request that just ran, so CC-1
can observe pressure without adding a second tokenizer or changing projection.

## Decision

Providers expose a `context_window(model)` capability in tokens. Known model
families are mapped centrally from the existing pricing/capability metadata:
Anthropic Claude families use 200k, GPT-5/Codex-family models use 256k, and
existing OpenAI-compatible families retain the prior 128k default. Unknown
models fall back to the conservative `DEFAULT_CONTEXT_WINDOW` of 128k tokens.

`truss-core` adds `ContextGauge`, which computes:

```text
pressure = projected_tokens / context_window
```

At each model step boundary, the agent loop emits:

```json
{"event":"context_pressure","tokens":50000,"window":200000,"pressure":0.25}
```

The durable loop emits the same semantic event after each live model call. This
is observability only: no compaction policy, summarization, elision, ledger
`compaction_performed` entry, or run-control decision is introduced in CC-1.

## Consequences

Operators and future controllers can watch context headroom using the same
`AgentEvent` stream as usage, tool, and lifecycle events.

The journal remains authoritative and unchanged. CC-1 only measures the prompt
that was already sent; it does not alter history, projection, tool execution, or
completion behavior.

Future milestones can build threshold policy, anticipation, governed summaries,
and eval on this signal without changing the provider abstraction again.
