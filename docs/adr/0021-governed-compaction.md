# ADR 0021: Governed Compaction

Status: Accepted

Date: 2026-06-18

## Context

ADR 0020 added the context-pressure gauge without changing run behavior. CC-2
activates the next step: when a run opts in, Truss can compact its own projected
history at a run-step boundary before an outer harness or provider-specific
context manager is forced to intervene.

The durable journal is still the source of truth. Compaction must therefore be
presentation-only: it may change the message history sent to the next model call,
but it must never delete or rewrite the signed journal entries that produced the
original context.

## Decision

`MemoryConfig` now includes a layered `CompactionPolicy` with safe defaults:

- `mode = "off"` by default, so existing runs are unchanged.
- `threshold = 0.70` is the default pressure trigger when a caller opts in.
- `min_tail = 6` keeps recent messages verbatim.
- `exempt_tiers` defaults to L0 core, L0 self, L1 principal, and L3 goal.

The durable in-loop executor checks the policy only at the boundary after a
model result has been journaled and context pressure has been measured. If
pressure is at or above the configured threshold, it uses `projection.rs` to
summarize the elided middle of the message history. L0/L1 remain in the stable
system prefix and L3 goal ledger is injected outside the compacted history, so
the existing exempt layers remain protected.

For `summarize` mode, the next request keeps:

- the original head message,
- one deterministic context-summary message,
- the configured verbatim tail.

For `evict` mode, Truss stores the same summary for audit and recall but omits
the summary from the active context. Both modes write `EntryKind::ContextSummary`
through the governed memory substrate. The entry is initially `Unverified`;
promotion remains governed by the existing verification policy rather than
relabeling deterministic summaries as verified without a verdict.

Every compaction appends a signed `compaction_performed` journal entry with:

- before and after token counts,
- the context-summary memory id,
- the inclusive elided journal range,
- the policy mode, threshold, min-tail, exempt tiers, and verification policy.

The before count comes from the already-observed CC-1 projected input-token
count. The after count is a conservative projection estimate from the compacted
message shape, avoiding a second tokenizer pass.

## Consequences

Governed compaction is opt-in and boundary-only, preserving prompt-cache prefix
stability and deterministic replay discipline. Disabled and below-threshold runs
continue to send the same request shapes as before.

The journal remains authoritative and fully auditable. The elided range can be
recovered from signed journal rows, and `Store::verify_chain` covers the
`compaction_performed` entry like every other durable event.

CC-2 intentionally does not add anticipation, `compaction_imminent`, eval
variants, or provider-side `context_management` interop. Those remain CC-3 and
CC-4 work.
