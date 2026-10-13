# Provider Plugins

A provider plugin turns a model backend into the one abstraction the agent loop
understands:

```rust
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;
    fn capabilities(&self, model: Option<&str>) -> ModelCapabilities;
    async fn complete(
        &self,
        ctx: &Ctx,
        req: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;
}
```

The CLI does not construct concrete providers. It passes a provider id and
`ProviderConfig` to `truss_providers::registry::build`. Auth resolution belongs
inside the provider's `build()` function, not in `truss-cli` or `truss-core`.

## Add a compile-time provider

1. Add a module in `crates/truss-providers/src/`.
2. Implement `Provider`.
3. Add `pub(crate) fn build(cfg: &ProviderConfig) -> Result<Arc<dyn Provider>, BuildError>`.
4. Add a cargo feature in `crates/truss-providers/Cargo.toml`.
5. Add one `available()` entry and one `build()` match arm in `registry.rs`.

The provider should convert Truss-neutral `CompletionRequest`, `ToolDef`,
`SystemBlock`, `ReasoningEffort`, and messages into the backend's wire format.
Keep provider-specific HTTP/auth/client code in `truss-providers`; `truss-core`
should stay vendor-free.

## Durability and provenance

Providers do not write durable journal entries themselves. The agent/durable loop
records model results after the provider returns. That keeps replay centralized:
on resume, Truss reuses the recorded result instead of asking the provider again.

Provider authors must:

- honor `ctx` cancellation/deadlines;
- return typed `ProviderError` values so retry policy can distinguish retryable
  transport errors from fatal config/model errors;
- expose conservative `ModelCapabilities`;
- keep secret material out of `Debug`, events, and errors;
- implement `complete_stream` only when the backend has true streaming. The
  default wrapper is acceptable and still produces correct `AgentEvent`s.

## Runnable checks

These checks exercise the provider registry boundary without requiring live
credentials:

```bash
cargo test -p truss-providers registry
cargo build -p truss-cli --no-default-features --features provider-openai
cargo build -p truss-core
```

For a provider with live credentials, add a wiremock/unit test first, then smoke
the CLI:

```bash
truss --provider <id> --model <model> "say hello in one sentence"
```
