# ADR 0003 — Subscription execution tiers (delegated CLI + OAuth-replay provider)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Matt (owner), ALINA
- **Builds on:** [ADR 0001](0001-plugin-architecture.md), [ADR 0002](0002-provider-registry-boundary.md)
- **Research:** [`docs/auth-strategy.md`](../auth-strategy.md)

## Context

`docs/auth-strategy.md` (incl. the 2026-06-15 deep dive) established: only OAuth
login/refresh generalizes across vendors; the inference call is irreducibly
per-vendor; and **vendor CLIs (`claude`, `codex`) are full agents that own their
own loop/tools/compaction — not raw model endpoints.** ADR 0002 gave us the
registry seam where each provider resolves its own auth.

Matt's direction (2026-06-15): proceed assuming 3rd-party subscription usage stays
supported near-term; **build Claude first**; for OpenAI, build **both** a delegated
path *and* in-process subscription replay — because a replay path that "works just
like the API path" is ideal for development/testing (no API credits burned, full
Truss loop).

That last point is the crux: there are **two structurally different ways** a
subscription can drive work, and they live at **different layers**.

## Decision

Support subscription auth at **two tiers**, chosen per vendor by what that vendor's
subscription surface actually allows:

### Tier 1 — In-loop OAuth-replay **Provider** (we own the loop)
A normal `Provider` (ADR 0002 registry entry) whose `build()` resolves an OAuth
**subscription** token instead of an API key, then calls the vendor's HTTP endpoint
with the vendor's mandatory client headers. Because it's one-request-in/out, it
behaves **exactly like the API path** — it drives Truss's full durable, signed,
policy-enforced, N-reviewer-verified loop. Best for **development/testing** and for
any vendor whose subscription endpoint accepts a replayed token.
- **OpenAI:** viable now (`~/.codex/auth.json` → Bearer access-token +
  `ChatGPT-Account-ID` against the Responses endpoint; refresh via the OAuth token
  endpoint). Build it — it's our cheap dev driver for the whole stack.
- **Anthropic:** **not** this tier — replay is ToS-banned (2026-02-19). Anthropic
  subscription only via Tier 2.

### Tier 2 — Delegated-CLI **Executor** (the vendor owns the inner loop)
Not a `Provider`. A new `Executor` abstraction at the **delegation layer**: shell
out to the vendor's own agent CLI (`claude -p --model claude-opus-4-8`, `codex exec`) with subscription
credentials **cleared from the child env** so the CLI uses *its own* stored login
(OpenClaw's model — zero token replay by us, ToS-robust). The CLI runs its own
loop/tools; we **do not** journal inner steps. We wrap at the **contract boundary**:
record the contract, capture the delegated result, then run our independent `check`
clauses + N-reviewer panel on it and sign the verdict.
- **Anthropic:** this is the path. `claude -p --model claude-opus-4-8` with
  `ANTHROPIC_API_KEY`/OAuth vars cleared.
- **OpenAI:** also available here (`codex exec`) for parity / when you want the
  vendor's full agent rather than just its model.

### The `Executor` seam (new)

`run_contract` stops taking `executor: Arc<dyn Provider>` and takes:

```rust,ignore
pub enum Executor {
    /// Truss owns the loop: durable agent drives a Provider. Full per-step journal,
    /// policy, tool sandbox. (API key OR Tier-1 OAuth-replay provider.)
    InLoop(Arc<dyn Provider>),
    /// A vendor agent owns the inner loop (Tier 2). Shell out, creds cleared,
    /// capture result, verify at the contract boundary.
    DelegatedCli(CliExecutor),
}
```

Verification (checks + reviewer panel + signed verdict) is **identical** for both —
that's the invariant. Subscription auth never weakens the moat: every contract still
yields a signed, independently-verified verdict. The only thing that varies is
whether the inner loop was ours (journaled per-step) or the vendor's (verified at
the boundary).

`CliExecutor` mirrors the load-bearing bits of OpenClaw's `CliBackendConfig`:
`command`, `args`, `clear_env` (vars stripped so the CLI uses its own login),
`prompt_via` (stdin | arg). Honors `Ctx` cancellation + deadline. Configurable via a
contract `[executor]` block.

## Why this split (not one mold)

You cannot have "the vendor CLI authenticates and drives" **and** "Truss journals
every inner step" simultaneously — the CLI owns the loop. Forcing Tier 2 into the
`Provider` trait would be a lie (it's not one-shot). Forcing Tier 1 into an
`Executor` would throw away the per-step durability we get for free when we own the
loop. Two honest abstractions beat one leaky one.

## Consequences

**Good**
- Anthropic subscription works **ToS-cleanly** (vendor CLI owns login, no replay).
- OpenAI gives us a **subscription-backed Provider that drives the full stack** —
  cheap dev/testing without API credits, behaves just like the API path.
- Both tiers sit behind existing seams (registry for Tier 1, the new `Executor` enum
  for Tier 2). Core stays small; vendor/ToS-sensitive logic stays at the edges.
- If a vendor flips billing/ToS (as Anthropic nearly did 2026-06-15), we disable one
  tier for one vendor; the API-key baseline and the verified core are untouched.

**Costs / trade-offs**
- Tier 2 loses inner-step journaling (we verify at the boundary instead). Acceptable:
  verified delegation is the value, not inner-step replay.
- Tier 1 for OpenAI is the grey/revocable path (`docs/auth-strategy.md`): off by
  default, dev/personal-use framing, never multi-tenant. Tier 2 is the safer
  production answer for OpenAI subscriptions.
- Tier 2 couples to each CLI's argv/stdin contract → needs a live smoke test per CLI
  (static inspection isn't enough); hermetic tests use a fake CLI to prove the
  subprocess wiring (env-clearing, stdin piping, capture, cancellation).

## Build order

1. **(this ADR's PR) Tier-2 `Executor` seam + `CliExecutor` + Anthropic (`claude -p --model claude-opus-4-8`)**,
   hermetic tests with a fake CLI, contract `[executor]` block. "Claude first."
2. **Tier-1 OpenAI OAuth-replay provider** (registry entry; dev driver) **+** Tier-2
   `codex exec`. Next PR.

## Status (2026-06-16)

- **Tier 2 (delegated CLI):** shipped v0.10.0. Anthropic `claude -p --model claude-opus-4-8`
  (`examples/contract-claude-subscription.toml`) and OpenAI `codex exec`
  (`examples/contract-codex-exec.toml`) both work via the same `CliExecutor` — no
  provider code, just an `[executor]` block.
- **Tier 1 (OpenAI in-process OAuth-replay):** shipped v0.11.0 as the
  `openai-codex` registry provider, behind the **`provider-openai-codex` cargo
  feature (off by default** — the ToS-grey path). Reads `~/.codex/auth.json`
  (`codex_auth.rs`), talks the ChatGPT-backend **Responses** API
  (`openai_codex.rs`), refreshes the access token reactively on a 401. Request
  shaping + SSE parsing are unit-tested against the source-exact schema, but the
  **live request hasn't been smoke-tested against the real backend** — the client
  headers (`originator`, `version`, user-agent) may need tuning, and the backend can
  fingerprint non-official clients. Treat as dev/personal-use until a real-login
  smoke test confirms it. Anthropic in-process replay stays **banned** — Tier 2 only.
