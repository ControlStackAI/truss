# Claude Subscription Contract

`examples/contract-claude-subscription.toml` uses a delegated CLI executor. The
Claude CLI owns the inner loop on its subscription login, while Truss still owns
the contract boundary, independent checks, reviewer panel, and signed verdict.

```bash
truss --contract examples/contract-claude-subscription.toml
```

```toml
{{#include ../../examples/contract-claude-subscription.toml}}
```
