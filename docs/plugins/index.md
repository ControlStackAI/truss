# Plugin Authoring Overview

Truss extension work should start from the smallest seam that preserves the
runtime's trust model. The safe path is:

1. Use MCP when the extension is an external tool or already exists as a service.
2. Use an in-process Rust `Tool` when the tool must share process memory or run
   hot/trusted logic.
3. Use a provider plugin when adding a model backend or auth strategy.
4. Use an event or ledger consumer when the extension only observes runs.
5. Use a sandbox or anchor backend when swapping infrastructure behind an
   existing trust boundary.

The key rule is simple: outcome-affecting work must pass through `Ctx`, policy,
events, and the signed ledger. Presentation-only work should stay off-journal.

## Current seams

| Seam | Current API | Best for | Journal rule |
|---|---|---|---|
| Provider | `truss_core::provider::Provider`, built through `truss_providers::registry` | Model backends, auth strategies, local OpenAI-compatible servers | Model results are journaled by the agent/durable loop, not by the provider |
| In-process tool | `truss_core::tools::Tool` | Trusted Rust tools that touch local state or need low latency | Override `call_with_ledger` when the tool creates durable facts or side effects |
| MCP server tool | `[[mcp.servers]]` + `mcp:<server>:<tool>` | Out-of-process tools and enterprise integrations | Truss records `mcp_tool_call` provenance plus replayable `tool_result` |
| Event stream | `AgentEvent` via `Ctx::with_events` | Live UIs, logs, dashboards, telemetry | Usually off-journal; observe without changing run outcome |
| Ledger reader | `LedgerReader`, `truss ledger ...` | Evidence exports, audits, buyer reports, recall | Read-only; never rewrites signed entries |
| Sandbox | `truss_core::sandbox::Sandbox` | Stronger command isolation behind existing self-check/tool flows | Keep the same `self_validation` journal shape |
| Anchor | `truss_core::anchor::Anchor` | Rekor/transparency log/on-chain hash anchoring | Anchor hashes only, never evidence or secrets |
| ContextProvider | Roadmap seam | Enterprise docs/vector/policy injection | Not a public trait yet; do not invent a parallel path |

## Pick the seam

- Add a model backend: [Provider Plugins](provider.md).
- Add a local Rust tool: [In-Process Tools](in-process-tool.md).
- Add an external tool: [MCP Server Tools](mcp-server.md).
- Add context injection: [Context Providers](context-provider.md).
- Add isolation or notarization infrastructure: [Sandbox and Anchor Backends](sandbox-anchor.md).
- Build a UI, exporter, dashboard, or audit bot: [Event and Ledger Consumers](event-ledger-consumer.md).

## Authoring checklist

- Does this extension affect the model's answer, contract verdict, filesystem,
  network, spend, memory, or identity? If yes, route it through a core seam.
- Does it only render, summarize, notify, or visualize already-recorded data? If
  yes, keep it as a consumer and do not add journal entries.
- Can it run out-of-process? Prefer MCP. In-process tools are for trusted code.
- Does it need credentials? Resolve them inside the provider/MCP server/backend
  and avoid recording secret values.
- Does it create a fact future runs may rely on? Stamp it with `LedgerRef`
  provenance.
- Does it perform a side effect that should execute once? Make it replayable via
  the durable ledger/memo path instead of relying on best-effort idempotency.

## Verification checklist

- `cargo test` or a focused crate test for Rust seam changes.
- `truss mcp probe <server>` for MCP tools.
- `truss ledger verify <run-id>` or `truss ledger export <run-id>` for durable
  evidence.
- `mdbook build docs` for docs and examples.
- Confirm no secret values appear in logs, events, or ledger payloads.
