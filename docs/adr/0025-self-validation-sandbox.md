# ADR 0025: SB-1 Self-Validation Sandbox

Status: Accepted

Date: 2026-06-18

## Context

The SWE in-loop run showed a concrete weakness: the executor could edit code and
use `bash`, but it did not have a reproducible, first-class path for running the
project's own tests during the solve. The external verify boundary remained the
truth, but the agent lacked useful local evidence before reaching that boundary.

At the same time, today's policy layer is authorization, not isolation. `Policy`
can deny tools, write scopes, network, and forbidden command substrings, while
the process still runs as the host user. SB-1 does not try to solve kernel-level
isolation. It creates the stable execution seam and ships a working-copy
implementation that contains common test pollution and records evidence.

## Decision

Add `truss_core::sandbox::Sandbox` as the command-execution seam:

```rust,ignore
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, cmd: SandboxCommand, ctx: &Ctx) -> Result<SandboxOutput, ToolError>;
    fn materialize(&self, checkout: &Path) -> Result<WorkDir, ToolError>;
    fn capabilities(&self) -> SandboxCaps;
}
```

`Ctx` carries the active sandbox alongside policy, budget, cancellation, and the
event sink. Tool execution composes both layers: `ToolRegistry` and individual
tools still consult `Policy`; shell execution then runs through the active
`Sandbox`.

SB-1 provides two implementations:

- `NoSandbox`: the default. It preserves the existing host `sh -c` execution path
  and process reaping discipline.
- `WorkingCopySandbox`: materializes an ephemeral copy of the checkout, runs the
  command there with `kill_on_drop(true)`, captures stdout/stderr/exit, hashes the
  resulting workdir, and deletes the copy when finished.

Add model-facing self-validation tools, `self_check` and `run_tests`, for
store-backed runs when a working-copy sandbox is selected. They run either the
contract's first declared `check` command or an explicit command supplied by the
model inside the active sandbox. Each run appends a signed journal entry:

```json
{
  "kind": "self_validation",
  "payload": {
    "cmd": "...",
    "exit": 0,
    "summary": "...",
    "workdir_hash": "...",
    "evidence_only": true,
    "verdict_unchanged": true
  }
}
```

A matching `AgentEvent::SelfValidation` is emitted. The verify boundary is
unchanged: harness-owned `check` clauses and the reviewer panel still determine
the verdict. Self-validation is evidence only.

Sandbox selection uses the same layered pattern as governed memory:

```toml
[sandbox]
mode = "no_sandbox"      # no_sandbox | working_copy
checkout = "."

[agent.sandbox]
mode = "working_copy"

[sandbox]               # in a contract file
mode = "working_copy"
```

Resolution is global → agent → contract, with `NoSandbox` as the default.
`truss-cli` performs TOML wiring and checkout selection; `truss-core` owns only
the typed config and runtime seam. Clap stays out of core.

## Consequences

Existing runs keep the same behavior unless sandbox mode is explicitly changed.
`NoSandbox` remains the default and the host execution output formatting stays
unchanged.

`WorkingCopySandbox` improves reproducibility and blast-radius containment for
test runs that use the checkout-relative working directory. It is not a security
boundary: absolute paths, host permissions, network, namespaces, seccomp, and
container images are intentionally out of scope for SB-1.

The stable seam allows SB-2 and SB-3 to swap in namespace or container-backed
implementations without changing the agent loop, self-validation journal shape,
or verify-boundary verdict semantics.
