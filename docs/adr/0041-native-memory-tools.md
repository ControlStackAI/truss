# ADR 0041: Native Model-Pulled Memory Tools

## Status

Accepted

## Context

S1 added semantic durable-memory recall, but recall was still primarily pushed at
run start. Long agent runs need a model-pulled path so the model can ask for L2
memory only when it becomes relevant.

Memory is not a normal scratchpad. Truss keeps five invariants:

1. Model writes enter `Unverified`.
2. `Verified` means a panel-signed or paired-eval verdict promoted the entry.
3. Every entry carries provenance and a `LedgerRef`.
4. Supersede and demote-to-`Stale` remain reversible ledgered state changes.
5. Recall defaults to verified-only and applies the trust floor before ranking.

The model should be able to read memory on demand without gaining a new write,
promote, or demote path.

## Decision

Add two typed native tools, both default-off:

- `memory_query`: query L2 durable memory with the configured recall mode
  (`exact`, `semantic`, or `hybrid`).
- `memory_view`: fetch entries by id.

Enable them with `--memory-tools` or `[tools] memory = true`.

The tools are read-only. The existing governed `memory_20250818` binding remains
the write path and is still controlled by the separate singular `--memory-tool`
flag.

Both read tools default `min_trust` to `Verified`. Operators may explicitly widen
the floor to `stale` or `unverified`, but rejected entries are never surfaced.
For semantic and hybrid recall, trust/kind/key filters are applied before vector
similarity scoring or ranking.

Durable and contract runs record signed `memory_tool_call` observations and memo
their outputs. Replay verifies the memo against a signed ledger entry and returns
the same recall result originally observed. `revalidate = true` bypasses the memo
and records a fresh observation.

## Consequences

- The model can pull durable memory when it needs it, reducing eager L2 context
  pressure.
- Verdicts can cite memory retrievals by signed `memory_tool_call` entries and
  the returned entries' `LedgerRef` provenance.
- Replayed runs do not silently see a mutated memory store unless the caller
  explicitly revalidates.
- Enabling read tools does not enable writes, promotion, rejection, or staling.

## Alternatives Considered

- **Reuse the existing `memory` tool.** That binding includes model-facing write
  commands, so it is too broad for a read-only recall surface.
- **Only push larger run-start recall.** That spends context before the model
  knows what it needs and does not help mid-run topic shifts.
- **Skip memoing because memory reads are local.** The store can mutate between
  resume attempts; replay must preserve the exact observation that influenced
  the original model turn.
