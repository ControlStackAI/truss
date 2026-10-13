# ADR 0002 — Provider registry boundary (auth lives in the provider)

- **Status:** Accepted
- **Date:** 2026-06-15
- **Deciders:** Matt (owner), ALINA
- **Builds on:** [ADR 0001](0001-plugin-architecture.md)

## Context

ADR 0001 set the strategy: a small, rock-solid core, with providers as plugins,
realized in Rust as a **compile-time tier** (Cargo features + workspace crates)
now and an optional **runtime tier** (external process / subprocess protocol)
later. The workspace split shipped — `truss-core` is vendor-free and only knows
the `Provider` trait; `truss-providers` holds the vendor code.

But the *boundary wasn't real yet*. The CLI still reached across it: `truss-cli`
contained two near-identical functions that hardcoded `match args.provider { ... }`
arms, read `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` directly, and knew each
provider's default model. Adding a provider meant editing the CLI. Worse, the CLI
was the place auth was resolved — which is exactly the line that would have to
branch for subscription/OAuth vs. API key (see `docs/auth-strategy.md`). Auth was
in the wrong layer.

## Decision

Introduce a **provider registry** in `truss-providers` (`registry.rs`) as the one
seam callers use. Two changes:

1. **Dispatch by id, in `truss-providers`.** `registry::build(name, &ProviderConfig)
   -> Result<Arc<dyn Provider>, BuildError>` owns the `match` over provider ids,
   with each arm `#[cfg]`-gated by its feature. `registry::available() ->
   Vec<&'static str>` lists what's compiled in (drives help + error messages).
   The CLI now just forwards `args.provider` + a `ProviderConfig`; it no longer
   names a concrete provider or knows which features exist.

2. **Auth resolution moves into the provider.** Each provider gets a
   `pub(crate) fn build(&ProviderConfig)` that resolves *its own* credentials and
   default model. `ProviderConfig` carries only *intent* (model, base_url) — never
   credentials. Today every provider resolves an API key from the environment; a
   future subscription/OAuth provider resolves its token behind the identical
   signature, so it slots in without touching `truss-core` or `truss-cli`.

Adding a provider is now: new module + a cargo feature + two lines in `registry.rs`
(one in `available()`, one match arm in `build()`). The core and CLI are untouched.

### `BuildError`

Construction failures get their own typed error, surfaced *before* any model call:

- `Unknown { name, available }` — no provider with this id is compiled in; the
  message lists the alternatives.
- `Auth(String)` — credentials missing/unusable (e.g. an env var isn't set).
- `Config(String)` — structurally invalid request (e.g. OpenAI needs a model).

This keeps "couldn't even start" distinct from transport/runtime `ProviderError`.

## Why not a global `inventory`/`linkme`-style auto-registry?

Crates like `inventory` let providers self-register via life-before-main static
constructors, removing even the two-line edit. Rejected for now: it adds a
dependency and link-time magic for a registry with a handful of entries, and it
hides the provider list (harder to reason about what a given build contains). An
explicit `match` is trivially auditable and costs two lines. Revisit only if the
provider count grows large enough that the central list becomes a real merge-point.

## Consequences

**Good**
- The boundary is now compiler-enforced *and* used: the CLI cannot reach into a
  vendor. Verified by building `truss-cli --no-default-features --features
  provider-openai` (Anthropic fully excluded) and by a dynamic "unknown provider"
  error that lists exactly what's compiled in.
- Auth lives where the provider-specific knowledge is. The subscription-auth work
  (ADR 0001's messy tier) now has a clean home and a stable signature to target —
  **without building it yet.**
- Default model + env-var names left the CLI; one source of truth per provider.

**Costs / trade-offs**
- Still compile-time: a third party can't add a provider without recompiling. That
  is the explicit ADR 0001 boundary; the runtime/subprocess tier remains the answer
  for external providers and is unaffected by this change.
- `ProviderConfig` will grow (e.g. an `auth` field) when subscription auth lands.
  That's expected and additive — the seam is in the right place.

## Follow-ups

- Subscription/OAuth provider(s) as new registry entries, per `docs/auth-strategy.md`.
- Optionally surface `available()` in `--provider`'s help text.
- The runtime (subprocess-protocol) plugin tier, if/when an external provider needs it.
