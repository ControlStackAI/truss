# In-Process Tools

An in-process tool is trusted Rust code registered in `ToolRegistry`. It receives
the active `Ctx`, so it inherits cancellation, deadlines, budget, principal,
policy, and event emission.

```rust
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn def(&self) -> ToolDef;

    async fn call(&self, ctx: &Ctx, input: serde_json::Value)
        -> Result<String, ToolError>;

    async fn call_with_ledger(
        &self,
        ctx: &Ctx,
        input: serde_json::Value,
        produced_by: Option<LedgerRef>,
    ) -> Result<String, ToolError> {
        self.call(ctx, input).await
    }
}
```

Use this seam only when MCP is the wrong fit: hot trusted logic, direct access to
in-process stores, or runtime primitives that must share `Ctx` internals.

## Minimal shape

```rust
use async_trait::async_trait;
use serde_json::json;
use truss_core::ctx::Ctx;
use truss_core::error::ToolError;
use truss_core::provider::ToolDef;
use truss_core::tools::{Tool, ToolRegistry};

struct BuildInfoTool;

#[async_trait]
impl Tool for BuildInfoTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "build_info".into(),
            description: "Return static build metadata".into(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn call(&self, ctx: &Ctx, _input: serde_json::Value)
        -> Result<String, ToolError>
    {
        ctx.check().map_err(|_| ToolError::Cancelled)?;
        Ok("build=local".into())
    }
}

fn register(registry: &mut ToolRegistry) {
    registry.register(std::sync::Arc::new(BuildInfoTool));
}
```

The tool name is the policy name. It is visible to the model only when
`Policy::can_use_tool(name)` allows it.

## When to override `call_with_ledger`

Override `call_with_ledger` when the tool creates state that future decisions may
trust:

- memory writes,
- identity/persona amendments,
- child-run or fan-out decisions,
- external side effects that must be tied to a model request,
- evidence artifacts used by a contract verdict.

The `produced_by` value points to the signed journal entry that caused the tool
call. Store that reference with the created fact, emit an `AgentEvent` if a UI
should observe it, and keep secret values out of the payload.

If the tool only renders a view, lists files, or returns deterministic local
metadata, the default `call_with_ledger` is fine.

## Verification checklist

- Add a unit test for schema/name stability.
- Add a policy denial test when the tool touches outside state.
- Confirm `ctx.check()` is called before expensive or blocking work.
- If `call_with_ledger` is overridden, test that missing provenance fails when
  provenance is required.
