# Examples

The examples in this section render the live files from `examples/`. The Markdown
adds context, but the TOML and prompt examples are included from the repository
source so they stay in sync with code and tests.

| Example | Use |
|---|---|
| [Harness Config](harness.md) | A full `--config` file for provider, agent, memory, budget, and policy defaults. |
| [Verified Contract](contract.md) | The minimal contract-to-verdict flow with checks and reviewer clauses. |
| [Contract with MCP Tool](contract-mcp.md) | Contract-scoped stdio MCP tool mounting with signed provenance. |
| [Contract with Compaction](contract-compaction.md) | A contract-level `[memory.compaction]` example; compaction remains opt-in. |
| [Claude Subscription Contract](contract-claude-subscription.md) | Delegated execution through Claude Code while Truss owns verification. |
| [Codex Exec Contract](contract-codex-exec.md) | Delegated execution through `codex exec` while Truss owns verification. |
| [Ledger Export Consumer](plugin-ledger-consumer.md) | A read-only plugin that consumes exported signed evidence without touching the runtime. |
| [Layered Long Run](m4-layered-long-run.md) | Intended long-run prompt shape for layered memory and projection. |

Run examples from the repository root unless the example says otherwise.
