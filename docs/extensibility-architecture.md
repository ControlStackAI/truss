# Truss Extensibility & Product Architecture

> Status: design / roadmap. Captured 2026-06-17 from the #truss product
> discussion. Decisions here drive the milestone sequence after v0.16.0.

## Product framing

Truss is **not an application — it is an embeddable verified-agent engine.** The
deliverable an enterprise integrates is a *library + a streaming service*, not a
UI. Almost everything a company wants to "plug in" reduces to two things:

1. **Stable extension traits** (backends, tools, context sources, routing).
2. **A typed event/provenance stream** they can observe and drive a UI from.

The `Ctx` keystone (cancellation, deadline, shared budget meter, `Principal`,
enforced `Policy`, `events` sink) is the spine that lets new cross-cutting
concerns attach without changing `complete`/`call` signatures. New capabilities
ride the `Ctx` and emit through the existing `events` + durable ledger.

### The three "customs" an enterprise asks for → which seam

- **Custom UI** → the `events` stream + ledger *is* the UI contract. Today
  CLI-only. Gap: expose the same typed events (step / tool-call / verdict /
  ledger-head) over a long-running service (WS/gRPC). Build the stream, not the
  screen. ClawTerm/Warp is just the first consumer.
- **Custom tools** → the tool registry exists but is **in-process Rust only**.
  Gap: an out-of-process tool tier. Adopt **MCP** as the standard for it so
  enterprises reuse existing MCP servers and add tools without forking/recompiling.
  In-process Rust stays for hot/trusted tools. Both register into the same loop,
  both captured in the ledger.
- **Custom content injection** → generalize the L0–L4 layered injection (M4).
  Today it pulls from our SQLite memory store + persona. Gap: a pluggable
  `ContextProvider` chain so a company injects from *their* vector DB / docs /
  policy engine into specific layers, routed through the same trust-tier +
  provenance machinery. The principled replacement for OpenClaw's `agent.md`
  blob-dump injection.

## Sequencing decision: primitives before the SDK

**Decision (Matt, 2026-06-17): build the core primitives first; cut the SDK last.**
Rationale: the SDK is a *surface*. Freezing a stable surface and then widening it
when spawn/fan-out land would churn the very thing we promised was stable. Draw
the boundary around the finished shape.

There is a hidden third layer that makes this work:

- **Primitives** — internal Rust (child-run, spawn, fan-out, ContextProvider,
  routing). Compose with the verified loop + ledger.
- **Event / provenance vocabulary** — the new event types these primitives emit,
  plus child-ledger anchoring into the parent hash chain.
- **SDK** — the thin external projection of that vocabulary + a handle to invoke
  the primitives.

**The discipline that makes the SDK cheap:** every primitive must route its
events/provenance through the existing `events` + ledger spine *as it is built* —
never bolt events on afterward. Then the SDK is a projection, not a redesign.

## Keystone primitive: child-run

**spawn and fan-out are the same underlying primitive.** Build one internal
**child-run** abstraction first:

> Run a sub-task with its own scoped `Ctx` (budget slice, deadline, scope),
> returning a **verified result** (verdict + artifacts) — not a transcript — with
> its child ledger **anchored** into the parent's hash chain.

That one primitive unlocks three features and de-duplicates existing code:

- **subagent spawn** = expose child-run as a model-facing tool.
- **best-of-N fan-out** = launch K child-runs of the same task + a reducer
  (vote / judge / merge).
- the existing **N-reviewer panel is already a fan-out instance** — refactor it
  onto the same substrate instead of keeping it as a one-off.

This composability with *verification* is what a simple harness (pi-class) can't
do — it's the differentiator, not a checkbox.

## Build order (post v0.16.0)

1. **child-run substrate** — scoped `Ctx` + budget slicing + anchored child
   ledger + its event/provenance vocabulary. The keystone; real design weight.
   Leave the spawn/fan-out *tool surface* as a follow-up so the substrate shape
   can be reviewed before building on it.
2. **spawn tool + best-of-N fan-out** on top of #1; refactor the N-reviewer panel
   onto the same substrate.
3. **ContextProvider trait + MCP tool tier** — independent seams; can run in
   parallel with #2.
4. **`truss-sdk`** — projects the now-coherent event stream and exposes the
   primitives. Cut last.

## Other primitives flagged (don't forget)

- **Tree-wide budget/concurrency governance** — one ceiling for a spawned fleet
  (the `Ctx` shared meter extends tree-wide).
- **Human-in-the-loop approval gates / pausable-resumable contracts** — "pause
  before this tool, or before spend > $X." Enterprise governance table stakes.
- **Smart routing as a Provider decorator** — the local-first pillar; route
  task → cheapest-capable model; the router itself a small local model later.
- **Out-of-process tool sandboxing + per-tenant isolation** — the multi-tenant,
  API-keys-only path.

## Discipline rule (so core doesn't bloat)

If provenance must capture it, it's **core** (child-run, spawn, fan-out, context
injection, routing, budget). If it only *consumes* the stream, it's a **plugin**
(the UI, concrete tools). Keeps faith with the "get out of the model's way" thesis.

For author-facing recipes, see the [Plugin Authoring Overview](plugins/index.md).
