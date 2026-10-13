# Enterprise readiness — status

This maps the "what does it take to go from the simple loop to enterprise-ready"
checklist to what v0.2 actually implements. Legend: ✅ done · 🟡 seam in place
(stable signature, fuller impl later) · ⬜ not started.

The governing idea: **one architectural move makes the rest bolt-on** — thread a
single `Ctx` handle (cancellation, deadline, budget, principal, policy, events)
through `Provider::complete` and `Tool::call`, and use typed errors. Done once, in
`src/ctx.rs` and `src/error.rs`, so no later feature changes those signatures.

## Lifecycle & control
- ✅ Cancellation token on `Ctx`; aborts in-flight HTTP and child processes
- ✅ Per-run wall-clock deadline (`--timeout`); auto-cancels
- ✅ Ctrl-C wired to cancellation
- 🟡 Graceful drain on SIGTERM (cancellation primitive exists; no signal handler yet)
- 🟡 Pause/resume (audit log is the resume seam; loader not built)

## Reliability
- ✅ Retries with exponential backoff + jitter (`src/retry.rs`)
- ✅ 429 handling honoring `Retry-After`
- ✅ Typed error taxonomy: retryable / fatal / policy (`src/error.rs`)
- ⬜ Idempotency keys for retried tool side effects

## Streaming & output
- ✅ Provider-neutral `complete_stream()` contract: default fallback for every
  provider, real SSE for Anthropic and OpenAI-compatible/local backends
- ✅ Incremental `AssistantTextDelta` events for live UIs + final assembled
  `CompletionResponse` for tool loop / usage / stop reason
- ✅ Tool-result truncation in events (full result still goes to the model)
- ⬜ Multimodal blocks (image/audio) — `Block` is the extension point

## Cost & limits
- ✅ Budget meter: tokens / cost / steps, enforced before & after each call
- ✅ Escalation warning at a configurable fraction (default 80%)
- ✅ Shared budget across sub-agents (clone of `BudgetMeter`)
- 🟡 Context-window manager (capabilities expose window size; no trimming yet)

## Observability
- ✅ `AgentEvent` stream for everything (UI + audit)
- ✅ Durable JSONL audit log with full provenance (`src/state.rs`)
- ✅ `tracing` diagnostics (`RUST_LOG`)
- ⬜ Metrics export (counters/histograms)

## Security & policy
- ✅ Enforced policy: write scope, read scope, forbidden shell ops, tool allowlist
- ✅ Network policy model (none/allowlist/open) — enforced by net-using tools
- ✅ Safe-by-default policy (deny writes, no network)
- 🟡 Pluggable secret provider (keys via env today; trait seam later)
- 🟡 Tool sandboxing (process isolation beyond kill-on-drop)

## Multi-tenancy & scale
- 🟡 `Principal` (tenant/user) on `Ctx` — carried, not yet isolating
- ✅ Parallel tool execution within a step
- 🟡 Sub-agent fan-out (`Ctx::child` exists; no orchestrator yet)
- ⬜ Concurrency caps / backpressure across many runs

## State & provider robustness
- ✅ Model capability metadata (`ModelCapabilities`) per provider
- ✅ Model override per request (`CompletionRequest::model`)
- 🟡 Durable run store / resume (JSONL log written; loader pending)
- ⬜ Record/replay of provider calls for CI

## Config & DX
- ✅ Declarative TOML config (provider/budget/policy), flags override
- ✅ Single-binary CLI
- ✅ Unit tests (budget, policy) + live end-to-end verified
