# Codex Exec Contract

`examples/contract-codex-exec.toml` delegates the inner solve to `codex exec`.
Truss clears API-key environment variables from the child and verifies the
result at the same contract boundary as API-backed runs.

```bash
truss --contract examples/contract-codex-exec.toml
```

```toml
{{#include ../../examples/contract-codex-exec.toml}}
```
