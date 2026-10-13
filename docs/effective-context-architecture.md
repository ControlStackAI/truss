# Effective Context & Verified Offload Architecture

> Status: design spec (2026-06-18). Builds directly on already-merged primitives:
> Compaction Controller (CC-1..CC-4, v0.31–0.34), `child_run` (v0.17), `fan_out`
> (v0.18), `spawn` tool (v0.19), M4 layered injection + `compaction_exempt` +
> `goal_repin_every` (v0.15), and ledger `recall` (v0.39). This spec mostly *wires
> and governs* existing substrate — it introduces one new measurement (the effective
> budget) and one new discipline (context-as-verified-reference), not new primitives.

## 1. The problem (Matt, 2026-06-18)

A model's **nominal** context window is not its **useful** window. Opus-class models
are strong out to ~250k tokens of *attended* content and then degrade — recall gets
spotty, instructions in the middle get dropped, and the harness gets a *false sense of
security*: "we still have room" when quality has already rotted. The degraded band is:

- **Real** — "lost in the middle" / effective-context decay is measurable, not folklore.
- **Model-dependent** — the useful fraction differs per model and per model family;
  it is NOT a fixed number we can hardcode.
- **Load-bearing for us** — it directly threatens the three things we inject every
  turn: **memory**, **persona/personality**, and **rules**. If those fall into the
  degraded band, the agent silently stops obeying them.

Today our Compaction Controller measures pressure against the **nominal** window
(`Provider::context_window()`), so we would happily ride context up toward the hard
ceiling — straight through the zone where the model already stopped paying attention.
**Substrate: yes. This specific mitigation: not yet.**

## 2. What already protects us (do not rebuild)

- **Identity/persona/rules/goal never get elided.** M4 sets
  `compaction_exempt = [l0_core, l0_self, l1_principal, l3_goal]` and re-pins the goal
  every `goal_repin_every` turns. Whatever else happens to context, these survive.
- **The journal is authoritative.** `replay.rs` reconstructs state from the signed
  hash-chained journal, so anything dropped from the *window* is never lost from the
  *record*. Compaction is presentation-only.
- **We assemble every request**, so we already know exact token counts and the model
  window — the CC gauge fires `context_pressure` and can compact-ahead.
- **Verified child runs exist.** `child_run` returns a signed `Verdict` + artifacts +
  an anchored child-ledger head — a *verified result*, not a transcript.

## 3. The strategy — three layers, each on-thesis

The goal is to keep the *attended* context inside each model's **useful** band at all
times, and to do so in a way that *raises* trust instead of eroding it. Three layers:

### Layer A — Effective-budget gauge (measure the right ceiling)

Stop measuring against the hard window. Add a **per-model effective context budget**:
the token count past which *this* model degrades. Drive `context_pressure`,
compact-ahead, and offload decisions against the **effective** budget, not the nominal
window.

- `Provider::effective_context_budget()` with a conservative default derived from the
  nominal window (e.g. `0.6 × nominal`) and **per-model-id overrides** in a small,
  configurable table (Opus ≈ 250k, others tuned as we measure).
- Fully configurable per Directive 1 (global → agent → contract): a knob, so we can
  **A/B different budgets on real contracts** and *measure* where each model actually
  rots instead of guessing. The eval harness (CC-4 / `truss eval`) already supports
  paired runs — this becomes a budget sweep.
- Net behavior: we start summarizing the middle / offloading / recalling **before**
  we enter the degraded band, eliminating the false-security ride to the ceiling.

### Layer B — Verified offload (context isolation done right)

The industry-standard mitigation for context bloat is **fan out to sub-agents**: each
worker gets a fresh, small, high-quality window; the orchestrator aggregates. The
*hidden flaw* in everyone else's version: the orchestrator ingests each worker's
**self-reported prose summary** and **trusts it**. The handoff is a lossy,
**unverified** trust boundary — you shrank the context by smuggling in an unaudited
claim. The smarter the worker, the more confident-but-wrong that summary can be.

**Truss fixes the handoff, because we already built the verified return path.** When
the gauge (Layer A) says the parent is approaching its effective budget, the harness
offloads a subtask via `child_run`. The child works in its *own* fresh window and
returns a **signed `Verdict` + artifacts + an anchored child-ledger head hash** — not a
transcript, not a prose summary. The parent ingests:

- a **small** payload (verdict + a hash), so the parent's window stays in its useful
  band, and
- a **verified** payload (panel-reviewed, signed, chain-anchored), so trust goes *up*,
  not down.

Normally context-size and trust trade off against each other. With verified offload
they move the same direction: the parent holds *less* context that is *more*
trustworthy. This is the thing no vanilla multi-agent setup can claim, and it falls
straight out of `child_run` + `fan_out` + the N-reviewer panel we already ship.

### Layer C — References over payloads (content-addressed context)

Because the journal is authoritative and every entry is hash-chained + signed, **any
in-window content can be replaced by a reference to it.** Instead of holding 30 KB of a
finished sub-result, the model holds `result verified ✓ — head_hash abc123…`. If it
later needs the detail, the harness **rehydrates exactly that slice on demand** via
`recall` / `ledger recall`, and the rehydrated bytes are **hash-checked against the
chain** — so the reference is provably *the same content*, not a drifting paraphrase.

This is the deep answer to "is there a better way than multiple sub-agent calls?":
- Everyone else's compression is **summary** — lossy, unverifiable, can silently drift
  from the truth.
- Ours is a **reference into a signed authoritative log** — losslessly rehydratable and
  cryptographically *equal to* the original. Their compressed context can lie; ours
  can't, because the pointer *is* the evidence.

Context stops being a fixed budget you burn down and becomes a **working set over an
authoritative store** — hold pointers, page in verified detail only when needed, page
it back out to a hash when done. The chain of evidence is what makes paging *safe*.

## 4. Why this is uniquely ours

| Concern | Typical harness | Truss |
|---|---|---|
| When to compact/offload | % of nominal window | % of **per-model effective budget** (measured) |
| Sub-agent handoff | prose summary, trusted | **signed verdict + anchored hash**, verified |
| Compressed context fidelity | lossy summary, can drift | **reference into signed journal**, hash-checked rehydrate |
| Memory/persona/rules under pressure | may fall into degraded band | **exempt + re-pinned**, never elided |
| Tuning the budget | guesswork | **policy knob + paired eval** on real contracts |

The differentiator is the same one as the whole product: **we have independent
verification and a signed chain of evidence**, so we can do context management as a
*trust-preserving* operation instead of a *trust-eroding* one.

## 5. Milestones

- **EC-1 — Effective-budget gauge** (first build; ≈ CC-5). Add
  `Provider::effective_context_budget()` (default `ratio × nominal`, per-model-id
  override table), thread it through the CC gauge so `context_pressure` /
  compact-ahead fire against the effective budget. New `EffectiveBudgetPolicy`
  (`ratio`, `overrides`, layered global→agent→contract), default conservative. No
  offload yet, no behavior change unless enabled. ADR 0030. Hermetic tests:
  pressure crosses at the effective budget, not the nominal window; per-model override
  beats the ratio default; policy layering resolves correctly.
- **EC-2 — Pressure-triggered verified offload.** When effective pressure crosses a
  threshold, route an eligible subtask through `child_run`; parent ingests the signed
  verdict + anchored head instead of the child transcript. Signed `context_offload`
  ledger entry. Reuses `child_run`; the new bit is the *trigger + ingestion shape*.
- **EC-3 — Reference-mode context (content-addressed working set).** Replace elided
  in-window payloads with chain references; rehydrate on demand via `recall` with a
  hash-equality check; signed `context_reference` / `context_rehydrate` ledger entries.
  Generalizes CC-2's compaction-summary into a verified reference.
- **EC-4 — Budget eval + interop.** Paired `truss eval` budget sweep (find each
  model's real degradation point on real contracts); document the measured per-model
  table; interop with provider-side context management (Anthropic
  `context_management`) so we observe/bound rather than fight it on the API path.

## 6. Caveats / boundaries

- **In-loop / API path only for active control.** On the **delegated-CLI** path the
  vendor CLI owns its own compaction; there we *observe and bound* (set the contract's
  effective budget low, offload whole subtasks) but cannot reach inside its window.
- **Default Off / conservative.** EC-1 ships with defaults that change nothing until a
  policy enables it; the effective ratio defaults conservative so we never *expand*
  perceived headroom, only contract it toward the useful band.
- **Don't overfit.** The budget table is per *model family*, not per benchmark; the
  goal is generality (memory/persona/rules integrity on real work), not a SWE-bench
  number.

## 7. Code mapping

- `crates/truss-core/src/provider.rs` — add `effective_context_budget()` (default impl
  over `context_window()`).
- `crates/truss-core/src/memory/context.rs` + the CC gauge — measure against effective
  budget.
- `crates/truss-core/src/memory/policy.rs` — `EffectiveBudgetPolicy` + layering
  (mirror `CompactionPolicyLayer`).
- `crates/truss-core/src/child_run.rs` / `fan_out.rs` — EC-2 reuses; new trigger +
  ingestion in the durable loop.
- ledger `recall` (`truss-cli` + store) — EC-3 rehydrate-with-hash-check.
- `truss eval` — EC-4 budget sweep.
