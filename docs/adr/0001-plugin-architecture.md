# ADR 0001 — Tiered plugin architecture (small, rock-solid core)

- **Status:** Proposed
- **Date:** 2026-06-15
- **Deciders:** Matt (owner), ALINA
- **Supersedes:** —

> **Naming note:** crate names below use a `<prefix>-` placeholder. The product
> name (working name "Truss") is *not yet decided* — see the "Open questions"
> section. Nothing in this ADR depends on the final name.

## Context

The harness today is a single Rust crate. The runtime, durable execution, signed
ledger, and verified delegation are vendor-agnostic and depend only on the
`Provider` / `Tool` traits — that boundary is already clean. But vendor-specific
code (HTTP backends, and soon several *subscription-auth* strategies that are
messy, frequently-broken, and ToS-sensitive — see `docs/auth-strategy.md`) lives
*inside* the crate. As we add providers, the core's dependency surface and
compile size grow, and vendor churn risks destabilizing the part we most want to
keep rock-solid.

OpenClaw hit the same wall and responded with a major revision to a plugin scheme
to keep its core small. We want the same property — **the core gets smaller and
more stable as providers are added, not bigger** — adapted to Rust.

**Key constraint that shapes the design:** Rust has no stable dynamic-plugin ABI,
so "plugin" cannot mean "drop a `.so` in at runtime" the way it does for
OpenClaw's Node/JS. Rust gives us two real extension mechanisms — *compile-time*
(Cargo features + workspace crates) and *runtime* (an external process speaking a
protocol). The right design uses both, tiered.

## Decision

Adopt a **three-tier architecture**.

### Tier 0 — the core (small, frozen, rock-solid)
A minimal core that contains only:
- the **contracts**: `Provider`, `Auth`, `Tool` traits, message/usage/error types,
  the run `Ctx`;
- the **runtime value**: the agent loop, durable execution (journal + replay),
  the signed ledger, verified delegation.

Invariants (enforced, not aspirational):
- **No vendor dependencies.** The core's `Cargo.toml` cannot reach a vendor SDK or
  even an HTTP client — HTTP providers move *out* of the core. Core deps shrink to
  essentials (tokio, serde, rusqlite, sha2, ed25519, …).
- **Stable, semver-versioned plugin API.** The trait contracts are the public
  surface; breaking them is a major-version event.
- Quality is enforced at the boundary by a **provider compliance kit** (below), so
  a small core does not mean a loose one.

### Tier 1 — first-party plugins = feature-gated workspace crates
Each provider / auth strategy is its own crate, behind a Cargo feature. A build
compiles **only** the ones enabled; an unused provider is *not in the binary at
all* (this is the bloat fix — opt-in compilation, not runtime toggles).

```text
crates/
  <prefix>-core/         # contracts + runtime (Tier 0)
  <prefix>-provider-openai/      # feature: provider-openai
  <prefix>-provider-anthropic/   # feature: provider-anthropic
  <prefix>-auth-apikey/          # feature: auth-apikey  (default)
  <prefix>-auth-oauth/           # feature: auth-oauth   (generic OAuth login/refresh)
  <prefix>-runtime-cli/          # feature: runtime-cli  (drive a vendor CLI as a backend)
  <prefix>-plugin-host/          # Tier 2 external-process host
  <prefix>-cli/                  # the binary; selects features
```

Adding a first-party provider = a new crate + one feature line. **The core never
changes.** Default features stay lean (proposed: `auth-apikey`, `provider-openai`,
`provider-anthropic`); everything else opt-in.

### Tier 2 — external plugins = a minimal process protocol
For third-party, experimental, out-of-tree, or ToS-sensitive providers, the core
ships one generic `ExternalProvider` that speaks a tiny JSON-over-stdio contract
(the model-provider analog of MCP-for-tools). Any executable implementing the
protocol is a provider — **no recompile, no core change, any language**.

Sketch (line-delimited JSON-RPC over stdio):
```text
→ {"method":"describe"}                      ← {"name","models","capabilities"}
→ {"method":"complete","params":{request}}   ← {"content","stop_reason","usage"}
→ {"method":"complete_stream","params":…}     ← stream of {delta} then {done}
→ {"method":"shutdown"}
```
This is where future **subscription providers** live. It isolates the per-vendor
token-replay logic (which breaks often and carries ToS risk) *entirely outside the
core*.

## How the auth/subscription problem maps onto this

The core knows only `Auth` (produce credentials + headers for a request) and
`Provider`. Every subscription strategy is a plugin:
- **API key** — `auth-apikey` (Tier 1, default).
- **OAuth subscription** — a generic `auth-oauth` login/refresh engine (Tier 1) +
  a small per-vendor descriptor (endpoint, headers, beta flags). Viable for OpenAI
  (tolerated); **not** for Anthropic (ToS-banned Feb 2026 — see
  `docs/auth-strategy.md`).
- **Vendor CLI runtime** — `runtime-cli` (Tier 1) or a Tier-2 plugin; the compliant
  path for Anthropic subscriptions. *This is where the prototype `claude-cli`
  provider (PR #10) belongs — as one feature-gated plugin, off by default, not a
  core feature and not "the" approach.*

## Provider compliance kit

A shared test suite (in `<prefix>-core` under a `testkit` feature) that every
provider — Tier 1 or Tier 2 — must pass: cancellation honored, deadline honored,
typed-error classification, usage accounting, tool-call translation round-trip,
streaming contract. Keeps the core small while guaranteeing plugins behave.

## Phasing (protects the June 28 demo)

A big-bang workspace split right before a demo is how things break. So:

- **Phase 0 — now, non-breaking:** feature-gate the existing providers *in the
  current single crate*. Instant "lean default build" win, near-zero risk, no file
  moves, demo unaffected.
- **Phase 1 — after the demo:** mechanically extract Tier 0 + Tier 1 into the
  workspace. Pure refactor, covered by the existing test suite.
- **Phase 2:** define + ship the Tier-2 external provider protocol. From then on,
  new subscription providers never touch the core.

## Consequences

**Positive**
- Core shrinks and stabilizes as providers grow — the opposite of bloat.
- Vendor churn / ToS risk is isolated outside the core.
- Lean default binary; users compile in only what they need.
- Tier 2 gives the OpenClaw-style "drop in a plugin" without Rust dynamic-loading.

**Negative / costs**
- A workspace is more moving parts than one crate (Phase 1 cost).
- Tier-2 plugins pay IPC/serialization overhead vs. in-process (acceptable; they're
  for the long tail, not the hot default).
- The compliance kit is upfront work — but it's how we keep the core honest.

## Alternatives considered

- **Stay a monolith, gate with features only.** Simpler, but vendor deps still live
  in the core crate and the dependency surface keeps growing. (Phase 0 *is* this —
  as a stepping stone, not the destination.)
- **Dynamic `dylib`/`abi_stable` plugins.** True runtime loading, but Rust's
  unstable ABI makes this fragile and a maintenance trap. Rejected.
- **WASM plugins.** Sandboxed, dynamic, language-agnostic — attractive long term,
  but heavier to build now and overkill for providers. Deferred; Tier-2 process
  plugins get 90% of the benefit sooner.

## Open questions (need decisions)

1. **Product name / crate prefix.** Blocks crate naming. "Truss" is the working
   name but unsettled — to be decided before Phase 1.
2. **Default-feature matrix.** Proposed: `auth-apikey`, `provider-openai`,
   `provider-anthropic` on by default; all else opt-in. Confirm.
3. **Disposition of PR #10 (`claude-cli`).** Keep as an off-by-default plugin
   example, or hold until Phase 1?
