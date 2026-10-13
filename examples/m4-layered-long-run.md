# M4 Layered Long-Run Example

This example documents the intended long-run prompt shape for a contract with
`goal_repin_every = 8`, `projection.keep_tail = 3`, and the default compaction
exempt tiers.

## Initial Call

Stable prefix:

1. tools
2. L0 core persona
3. L0 self persona
4. L1 principal with Anthropic `cache_control = { type = "ephemeral" }`

Messages:

1. L3 goal ledger: active contract intent and definition of done
2. original user task
3. live history tail

The provider can cache-read the tools + L0/L1 prefix on subsequent calls because
the goal ledger and scratch notes are below the cache breakpoint.

## Step 8 After Compaction

Stable prefix:

1. unchanged tools
2. unchanged L0 core persona
3. unchanged L0 self persona
4. unchanged L1 principal with the same cache breakpoint

Messages:

1. re-pinned L3 goal ledger
2. original user task
3. deterministic context summary of the compacted middle
4. last three history messages
5. L2 delta since the previous cursor, if any
6. L4 scratch, if any

Expected effect: input tokens above the cache breakpoint are stable across the
run, so cache-read tokens dominate once the prefix is warm. The active contract
intent remains visible even when the middle history is summarized.
