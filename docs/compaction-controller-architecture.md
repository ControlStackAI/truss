# Compaction Controller — proactive, governed, auditable context compaction

Status: design (pre-build). Target milestone arc: CC-1 → CC-4. ADR: 0020 (added with CC-1).

## Problem

Long agent runs hit the model's context window. When they do, *something* compacts the
transcript — and today that something is **outside Truss's control**: the host harness
(Claude Code, OpenClaw, Codex) or an opted-in provider feature (Anthropic
`context_management` / memory tool) summarizes or drops messages on its own schedule, by its
own rules, with no record of what it threw away. The result is the momentum-killer Matt
flagged: the run silently loses a decision, a goal, or a constraint mid-stream and slows to a
crawl while it re-derives what it already knew.

## Reframe: we don't *predict* provider compaction — we *own* it

The LLM API does not silently compact. The context window is a hard wall; the API rejects an
over-long request, it does not quietly trim it. What "compacts" is always a layer *around*
the model. And in Truss's design, **we are that layer** — `agent.rs` + `projection.rs`
assemble every request, so at every step we already know:

- exact projected token count (we build the prompt; `BudgetMeter` meters it),
- the model's context window (a provider capability, see CC-1),
- therefore **headroom to the wall** as a first-class signal — a gauge, not a surprise.

So the goal is not to guess when an opaque provider will compact. It is to make Truss perform
a **governed compaction on our own terms, before** any host/provider compaction can fire —
and to record exactly what happened so nothing is ever truly lost.

## Why Truss is uniquely positioned (the moat)

- **Journal is authoritative.** Every turn is in the signed, hash-chained journal
  (`store.rs`); `replay.rs::reconstruct` can rebuild the model's working context from any
  point. So compaction is *only ever a presentation/projection concern, never data loss.*
  A normal chat harness deletes; Truss elides a copy and keeps the original on disk, signed.
- **Already half-built.** M4 shipped layered L0–L4 injection + `projection.rs`
  (Head + structural-summary-of-middle + verbatim tail) + `compaction_exempt`
  (`l0_core`, `l0_self`, `l1_principal`, `l3_goal`) + `goal_repin_every`. The goal ledger and
  identity are already protected from compaction. What's missing is the *active controller*
  that decides **when** to compact and **records** it.
- **Governed + auditable + recallable.** A compaction becomes a signed ledger entry plus a
  trust-tiered memory entry holding the summary, with provenance back to the elided journal
  range. You can audit what was summarized and recall the verbatim original. No frontier
  harness ships this.
- **Eval-able.** Reuse the existing `eval` engine to run one contract under different
  compaction policies (off / threshold sweeps / summarize-vs-evict) and compare verdict +
  cost + turns-to-done. Per the everything-configurable directive, every knob is a setting so
  we can measure paths on real work.

## Caveat — delegated tier

Full control only exists on the **in-loop / API path**, where Truss owns the context. On the
**delegated** path (`claude -p`, `codex exec`), the vendor CLI owns its own compaction; Truss
can observe and bound (budget/turns) but not govern it. This reinforces the existing thesis:
in-loop is the real product; delegated is the convenience tier.

## Design

### Components

1. **ContextGauge** (`truss-core`): computes `pressure = projected_tokens / context_window`
   at each step boundary. Requires a provider capability `context_window()` (CC-1) and a
   model→window map seeded from `pricing.rs`/capability metadata. Emits `context_pressure`.
2. **CompactionPolicy** (configurable, layered global → agent → contract):
   - `mode`: `off` | `summarize` (default) | `evict`
   - `threshold`: pressure at which to act (default `0.70`)
   - `exempt`: reuse M4 `compaction_exempt` layers (never compact L0/L1/L3)
   - `min_tail`: verbatim recent turns always kept
   - `anticipate`: forward-estimate on/off (CC-3)
3. **Governed compaction action** (CC-2): at a *run-step boundary* (never mid-step — protects
   the KV-cache prefix and the determinism discipline, same rule as identity self-amend),
   when `pressure ≥ threshold`: summarize the elided middle via `projection.rs` into a memory
   entry (`EntryKind::ContextSummary`, trust per `VerificationPolicy`), stamp provenance
   `LedgerRef` to the elided journal range, keep exempt layers + `min_tail` verbatim, continue.
4. **Provenance** (CC-2): signed `compaction_performed` ledger entry recording
   before/after token counts, the summary memory id, and the elided journal range;
   `Store::verify_chain` covers it. Fully recallable + auditable.

### Anticipation (Matt's question, CC-3)

Beyond reading current pressure, estimate **steps-to-threshold** from the running average
per-step token growth (tool-result sizes dominate). That yields a true *early warning*
(`compaction_imminent`) and lets the controller compact one boundary *ahead* of the wall
instead of right at it — smoother, fewer cache-busting events. Optional CC-4 interop: when
talking to Anthropic, set provider-side `context_management` to defer to ours (or leave it
off) so there is exactly one compaction authority — Truss.

### Event / ledger vocabulary (grows the provenance spine, per the discipline rule)

- `context_pressure { tokens, window, pressure }` — observability (CC-1)
- `compaction_imminent { steps_to_threshold, pressure, window }` — early warning (CC-3)
- `compaction_performed { before, after, summary_memory_id, elided_range }` — signed ledger (CC-2)

## Milestones (1 milestone = 1 PR + 1 ADR section)

- **CC-1 — Gauge + observability (FOUNDATION, no behavior change).**
  `Provider::context_window()` capability + model→window map; `ContextGauge::pressure()`;
  `context_pressure` event each step. Pure measurement, nothing compacts yet. Tests:
  pressure math, window map, event emitted. → ADR 0020. *Start here.*
- **CC-2 — Governed compaction.** `CompactionPolicy` (layered config) + boundary-safe
  summarize-elided-middle → `ContextSummary` memory entry + signed `compaction_performed`
  ledger; exempt layers honored; recallable. Hermetic tests incl. "exempt layers survive" and
  "elided range recoverable from journal."
- **CC-3 — Anticipation.** steps-to-threshold estimator + `compaction_imminent` + compact-ahead. → ADR 0022.
- **CC-4 — Eval.** hermetic paired eval (off / governed threshold / anticipate) via a
  scripted provider workload, with a reproducible comparison table and ADR 0023. Live
  SWE-bench and Anthropic `context_management` interop stay out of scope for this milestone.

## Invariants (any implementation must hold)

1. The journal stays authoritative; compaction never deletes journal entries, only changes
   what is *projected* into the model's context.
2. Exempt layers (L0 core/self, L1 principal, L3 goal) are never compacted.
3. Every compaction is a signed ledger entry and is recallable.
4. Compaction happens only at a step boundary, never mid-step.
5. Every knob is configurable (global → agent → contract) with a safe default.
