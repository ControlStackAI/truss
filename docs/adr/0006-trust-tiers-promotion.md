# ADR 0006: Trust Tiers + Promotion

Status: Accepted

## Context

M1 added the governed SQLite memory substrate and M2 bound the native
`memory_20250818` tool. M3 turns model-authored memory from "stored" into
"trusted" by reusing the verified-delegation reviewer panel and
`delegation::Verdict` that already gate contract completion.

## Decision

Truss resolves memory policy at session build with layered overrides:
global `[memory]`, then `[agent.memory]`, then contract `[memory]`.

`VerificationPolicy` controls promotion timing:

- `lazy` verifies an unverified entry on first recall, then caches the verdict in
  the entry trust tier.
- `eager` verifies on write and promotes only a passing panel verdict.
- `tiered` runs a deterministic cheap hygiene gate on every write and runs the
  full reviewer panel on write for `fact` and `decision` entries.
- `off` never promotes; recalls may surface unverified entries, but the rendered
  view always tags them as `trust: unverified`.

Promotion and rejection both store the existing `delegation::Verdict`. A verified
replacement for the same supersede key demotes the prior verified entry to
`stale`; a rejected replacement restores the prior verified entry as active.

M4 extends the same layered policy surface with configurable L2 scope and
conflict handling. Defaults are conservative: `scope = "per-agent"` and
`conflict = "verified-wins"`. Other modes (`per-project`, `shared`,
`latest-wins`, `panel-arbitrated`, `keep-both-flagged`) are present for
evaluation, not because the trust model assumes one answer fits every run.

## Consequences

`Verified` keeps one meaning: an N-reviewer panel signed off. Lower-cost modes
change when verification happens, not what the label asserts. Memory views always
render the trust tier, preventing silent trust laundering.
