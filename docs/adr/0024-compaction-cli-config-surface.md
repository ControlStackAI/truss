# ADR 0024: Compaction CLI and Config Surface

Status: Accepted

Date: 2026-06-18

## Context

ADR 0020 through ADR 0023 built the compaction controller: context pressure,
governed compaction, compact-ahead anticipation, and a hermetic eval harness. The
runtime policy already resolves through `MemoryConfig`, but operators still had
to hand-edit TOML to enable it, and the supported config surface was not
documented at the CLI boundary.

The invariant remains unchanged: compaction must be safe and opt-in. Existing
runs must keep the same request shape unless a caller selects a compaction mode.

## Decision

Expose the resolved `[memory.compaction]` policy through the normal memory config
layering:

```toml
[memory.compaction]
mode = "off"          # off | summarize | evict
threshold = 0.70      # 0.0 through 1.0
min_tail = 6
anticipate = false
```

The block can be set globally, under `[agent.memory.compaction]`, or in a
contract's `[memory.compaction]`. Resolution stays global → agent → contract,
then CLI flags override the resolved policy:

```bash
truss --compaction-mode summarize \
  --compaction-threshold 0.70 \
  --compaction-min-tail 6 \
  --compaction-anticipate \
  --durable "..."
```

`truss-cli` owns the Clap-specific surface. It mirrors `CompactionMode` with a
CLI-local `ValueEnum`, the same pattern used for reasoning effort, so
`truss-core` remains free of Clap. The threshold flag validates `0.0..=1.0` at
parse time; config resolution keeps its existing defensive clamp.

## Consequences

The default remains `mode = "off"`, so this ADR does not change runtime behavior
for existing users. Operators can now select governed compaction without editing
core code, and tests cover TOML parsing, config precedence, flag precedence,
threshold validation, and a hermetic CLI-flag-to-durable-compaction smoke path.

The CLI does not expose controller internals such as summary construction,
provenance shape, gauge math, eval metrics, or Anthropic context-management
interop. Those remain in the controller/runtime and future integration work.
