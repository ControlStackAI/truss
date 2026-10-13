# Harness Config

`examples/harness.toml` is the general-purpose configuration file. It shows the
provider block, agent defaults, optional memory/projection/compaction knobs,
budget ceilings, and enforced policy.

```bash
truss --config examples/harness.toml --tools "summarize this repository"
```

```toml
{{#include ../../examples/harness.toml}}
```
