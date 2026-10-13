# Verified Contract

`examples/contract.toml` is the baseline verified-delegation contract. The
executor does the work, then Truss independently runs `check` clauses and asks
the reviewer panel to judge `review` clauses before signing a verdict.

```bash
truss --contract examples/contract.toml --tools
```

```toml
{{#include ../../examples/contract.toml}}
```
