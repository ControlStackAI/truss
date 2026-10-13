# Truss engineering tour

This is a source-oriented map of workspace **0.58.0**, reviewed against commit
`5e26aacb9123b9e25b06a111ad781f413e846dec` on **2026-10-12**. Unlike the historical
roadmap, the links below point to implemented code and its test coverage.

## 1. Contract acceptance is separate from execution

[`contract.rs`](../crates/truss-core/src/contract.rs) models intent, acceptance
clauses, budget, and constraints. [`delegation.rs`](../crates/truss-core/src/delegation.rs)
coordinates execution, harness-run shell checks, and the model-review panel.

Read `contract_fails_when_a_check_fails` in
[`durable_test.rs`](../crates/truss-core/tests/durable_test.rs). A fake executor
writes the wrong content and completes; the check still yields a failed verdict.
The majority-review tests separately exercise panel aggregation. These are useful
regressions, not a demonstration that an LLM reviewer reliably catches every bug.

## 2. The journal is a recovery mechanism and an audit record

Explore [`durable.rs`](../crates/truss-core/src/durable.rs) and the suspend/resume,
recorded-step replay, tamper, and signature tests in
[`durable_test.rs`](../crates/truss-core/tests/durable_test.rs).

The architectural choice is to reconstruct from persisted outcomes, rather than
ask the model to infer what happened before a restart. Reusing a recorded outcome
is distinct from guaranteeing exactly-once effects in an external service. A crash
between an effect and its journal commit still requires idempotency or reconciliation.

## 3. Cross-cutting controls use one context

[`ctx.rs`](../crates/truss-core/src/ctx.rs) carries controls through model and tool
calls. [`agent.rs`](../crates/truss-core/src/agent.rs),
[`policy.rs`](../crates/truss-core/src/policy.rs), and
[`budget.rs`](../crates/truss-core/src/budget.rs) show how the loop consumes them.
This keeps new controls from requiring an unrelated change to every integration.

## 4. The isolation boundary is explicit

[`sandbox.rs`](../crates/truss-core/src/sandbox.rs) defaults to `NoSandbox` and also
provides a working-copy mode. Neither is a claim of kernel or network isolation.
[`sandbox_test.rs`](../crates/truss-core/tests/sandbox_test.rs) tests the implemented
boundary. A fresh working copy can make validation more repeatable without making
arbitrary shell commands safe to run on a sensitive host.

## 5. Integrations stay inspectable

- [Provider registry](../crates/truss-providers/src/registry.rs): feature-gated model backends.
- [MCP tests](../crates/truss-core/tests/mcp_test.rs) and [server tests](../crates/truss-core/tests/mcp_server_test.rs): protocol-facing integration behavior.
- [Child-run tests](../crates/truss-core/tests/child_run_test.rs) and [fan-out tests](../crates/truss-core/tests/fan_out_test.rs): delegation state and aggregation.
- [Compaction tests](../crates/truss-core/tests/compaction_cc2_test.rs): projected context remains separate from the durable record.

## Reproduce the short tour

```sh
cargo test --locked -p truss-core --test durable_test
```

This uses fake providers and temporary state, not a model subscription or API key.
It checks runtime behavior, not live-provider quality. See the root
[README](../README.md) for build steps and the broader CI commands.

## Verification recorded on 2026-10-12

The command above completed locally on Linux: **18 passed, 0 failed, 0 ignored**.
It used controlled providers and temporary state. No paid model call was made.
This is a dated result for the referenced source revision, not a continuously
updated CI badge or a statement that the entire workspace was requalified.
