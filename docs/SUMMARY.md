# Summary

[Introduction](introduction.md)

# Architecture

- [Engineering Tour](engineering-tour.md)

- [System Architecture](architecture.md)
- [Build Plan](BUILD-PLAN.md)
- [Durable Execution](durable-execution.md)
- [Extensibility Architecture](extensibility-architecture.md)
- [MCP Architecture](mcp-architecture.md)
- [Native Git and GitHub Tools](native-git-github-tools.md)
- [Provider Tool Architecture](tool-architecture-provider-tools.md)
- [Auth Strategy](auth-strategy.md)
- [Enterprise Readiness](enterprise-readiness.md)
- [Testing Strategy](testing-strategy.md)
- [UX Presentation Principles](ux-presentation-principles.md)
- [Verdict Review Surface Architecture](verdict-review-surface-architecture.md)
- [Session TUI](session-tui-architecture.md)

# Memory, Compaction, and Self-Improvement

- [Memory and Identity Architecture](memory-identity-architecture.md)
- [Memory and Identity Implementation Plan](memory-identity-impl-plan.md)
- [Memory Evolution Path](memory-evolution-path.md)
- [Compaction Controller Architecture](compaction-controller-architecture.md)
- [Self-Improvement Architecture](self-improvement-architecture.md)
- [Self-Improvement Implementation Plan](self-improvement-impl-plan.md)
- [Self-Validation Sandbox Architecture](self-validation-sandbox-architecture.md)

# Configuration

- [Configuration Surface](configuration.md)
- [Native Memory Tools](native-memory-tools.md)
- [ai-memory Backend](ai-memory-backend.md)
- [Native Web Tools](native-web-tools.md)

# Plugin Authoring

- [Plugin Authoring Overview](plugins/index.md)
- [Provider Plugins](plugins/provider.md)
- [In-Process Tools](plugins/in-process-tool.md)
- [MCP Server Tools](plugins/mcp-server.md)
- [Context Providers](plugins/context-provider.md)
- [Sandbox and Anchor Backends](plugins/sandbox-anchor.md)
- [Event and Ledger Consumers](plugins/event-ledger-consumer.md)
- [Journal Discipline](plugins/journal-discipline.md)

# Governed Competence

- [Verified Playbooks](playbooks.md)

# Examples

- [Example Overview](examples/index.md)
- [Harness Config](examples/harness.md)
- [Verified Contract](examples/contract.md)
- [Contract with MCP Tool](examples/contract-mcp.md)
- [Serve Truss over MCP](examples/serve-mcp.md)
- [Contract with Compaction](examples/contract-compaction.md)
- [Claude Subscription Contract](examples/contract-claude-subscription.md)
- [Codex Exec Contract](examples/contract-codex-exec.md)
- [Ledger Export Consumer](examples/plugin-ledger-consumer.md)
- [Layered Long Run](examples/m4-layered-long-run.md)

# Decision Records

- [ADR 0001: Tiered Plugin Architecture](adr/0001-plugin-architecture.md)
- [ADR 0002: Provider Registry Boundary](adr/0002-provider-registry-boundary.md)
- [ADR 0003: Subscription Execution Tiers](adr/0003-subscription-execution-tiers.md)
- [ADR 0004: Governed Memory Substrate](adr/0004-governed-memory-substrate.md)
- [ADR 0005: Memory Tool Binding](adr/0005-memory-tool-binding.md)
- [ADR 0006: Trust Tiers and Promotion](adr/0006-trust-tiers-promotion.md)
- [ADR 0007: Layered Injection and Identity Persona](adr/0007-layered-injection-identity.md)
- [ADR 0008: Child-Run Substrate](adr/0008-child-run-substrate.md)
- [ADR 0009: Best-of-N Fan-Out on Child-Run](adr/0009-fan-out-child-run.md)
- [ADR 0010: Spawn Tool and Reviewer Panel/Fan-Out Unification](adr/0010-spawn-tool-and-panel-fan-out.md)
- [ADR 0011: Lesson Extraction and Governed Policy Seam](adr/0011-lesson-extraction-policy-seam.md)
- [ADR 0012: Lesson Promotion Through the Reviewer Panel](adr/0012-lesson-promotion.md)
- [ADR 0013: Verified Lesson Recall Into L2 Context](adr/0013-lesson-recall.md)
- [ADR 0014: Paired Learning-On/Off Eval Harness](adr/0014-paired-learning-eval.md)
- [ADR 0015: Adversarial and Property Testing](adr/0015-adversarial-property-testing.md)
- [ADR 0016: Truss Bench Crate Boundary](adr/0016-truss-bench-boundary.md)
- [ADR 0017: SWE-bench Verified Adapter](adr/0017-swebench-verified-adapter.md)
- [ADR 0018: SWE-bench In-Loop Generation](adr/0018-swebench-in-loop-generation.md)
- [ADR 0019: Provider-Aware Typed Tool Schemas](adr/0019-provider-aware-typed-tools.md)
- [ADR 0020: Context Pressure Gauge](adr/0020-context-pressure-gauge.md)
- [ADR 0021: Governed Compaction](adr/0021-governed-compaction.md)
- [ADR 0022: Compaction Anticipation](adr/0022-compaction-anticipation.md)
- [ADR 0023: Compaction Eval Harness](adr/0023-compaction-eval-harness.md)
- [ADR 0024: Compaction CLI and Config Surface](adr/0024-compaction-cli-config-surface.md)
- [ADR 0025: Self-Validation Sandbox](adr/0025-self-validation-sandbox.md)
- [ADR 0026: mdBook Documentation Site](adr/0026-mdbook-documentation-site.md)
- [ADR 0027: Ledger Read Tools LR-1](adr/0027-ledger-read-tools-lr1.md)
- [ADR 0028: Ledger Read Tools LR-2](adr/0028-ledger-read-tools-lr2.md)
- [ADR 0032: Verified Playbook Promotion and Recall](adr/0032-verified-playbooks-m2.md)
- [ADR 0033: Eval-Driven Playbook Promotion](adr/0033-eval-driven-playbook-promotion.md)
- [ADR 0034: Signed Human Review Primitive](adr/0034-signed-human-review-primitive.md)
- [ADR 0035: Read-Only Review TUI](adr/0035-read-only-review-tui.md)
- [ADR 0036: TUI Review Actions](adr/0036-tui-review-actions.md)
- [ADR 0037: Approval Gates](adr/0037-approval-gates.md)
- [ADR 0038: Native Git and GitHub Tools](adr/0038-native-git-github-tools.md)
- [ADR 0039: MCP Server Tier](adr/0039-mcp-server-tier.md)
- [ADR 0039: Native Web Search And Fetch Tools](adr/0039-native-web-tools.md)
- [ADR 0040: Semantic Memory Recall Backend](adr/0040-semantic-memory-recall.md)
- [ADR 0041: Native Model-Pulled Memory Tools](adr/0041-native-memory-tools.md)
