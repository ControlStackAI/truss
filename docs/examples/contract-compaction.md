# Contract with Compaction

`examples/contract-compaction.toml` enables governed compaction at the contract
layer. This is default-off everywhere else; selecting `summarize` here affects
only the projected context for this contract run.

```bash
truss --contract examples/contract-compaction.toml --tools
```

```toml
{{#include ../../examples/contract-compaction.toml}}
```
