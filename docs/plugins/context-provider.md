# Context Providers

`ContextProvider` is a planned seam, not a public Rust trait in this release.
That distinction matters: context injection changes what the model sees, so it
must be governed by trust tier and provenance rather than bolted on as a prompt
string.

The current runtime already has the substrate a future `ContextProvider` will use:

- `GoalLedger` for contract intent and definition-of-done context;
- layered memory/persona context;
- governed compaction and signed `compaction_performed` journal entries;
- `LedgerRef` provenance on memory entries;
- `AgentEvent` for recall/write observations.

## Desired shape

A provider should eventually answer: "for this run, principal, policy, goal, and
trust tier, what context fragments may enter which layer?"

```rust
// Roadmap sketch, not current API.
#[async_trait::async_trait]
pub trait ContextProvider: Send + Sync {
    async fn project(
        &self,
        ctx: &Ctx,
        request: ContextRequest,
    ) -> Result<Vec<ContextFragment>, ContextError>;
}
```

Each fragment needs provenance:

- source system and stable id,
- trust tier,
- content hash,
- redaction/secret handling,
- optional `LedgerRef` when produced by a prior Truss run.

## What to do today

- For durable facts produced by Truss, use the governed memory tool and
  `LedgerReader`.
- For passive presentation, consume `AgentEvent` or ledger exports.
- For enterprise docs/vector DB injection, keep the adapter outside the runtime
  until the public `ContextProvider` trait lands. Do not fork prompt assembly or
  write directly into system prompts if the data affects outcomes.

## Done criteria for the future seam

- Context fragments are selected by policy and trust tier.
- Injected fragments can be traced back to source ids/hashes.
- Summaries and compactions point back to signed ledger ranges.
- Presentation-only context stays off-journal; outcome-affecting context is
  provenance-bearing.
