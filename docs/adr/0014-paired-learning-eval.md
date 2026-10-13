# ADR 0014: Paired Learning-On/Off Eval Harness

Status: Accepted

## Context

ADR 0011 added SI-1 lesson extraction, ADR 0012 added panel-gated lesson
promotion, and ADR 0013 added verified lesson recall into L2 context. Those
steps make cross-run learning possible, but existence is not evidence that L2
helps.

`docs/self-improvement-impl-plan.md` Step 6 requires SI-4: run the same contract
with learning enabled and disabled, compare outcomes and cost, and produce the
measurement signal that a future L3 optimizer can optimize against.

## Decision

Truss adds `truss_core::eval`, a paired evaluation harness that runs the same
contract in two arms:

- `learning_on`: lesson extraction on, lesson promotion eager, verified lesson
  recall enabled.
- `learning_off`: lesson extraction off, promotion off, verified lesson recall
  disabled.

Every repeat in each arm runs through `child_run::run_child`; the eval harness
does not reimplement execution. The child ledger is the durable contract
execution ledger for that repeat, and the parent eval ledger records each
`child_anchor` as usual. The contract verifier still uses the existing
delegation path, reviewer panel, budget meter, and provider path.

The CLI exposes the measurement as:

```bash
truss eval --contract path/to/contract.toml --repeats 3
```

The command prints a concise human summary to stderr and the structured report
as pretty JSON on stdout. It also records the report as a signed `eval_report`
journal entry in the eval parent session, so `truss --db <db> --verify
<eval_run_id>` verifies the report session and recursively verifies the anchored
arm child-runs.

## EvalReport Schema

`EvalReport` is serializable with `serde`:

- `eval_run_id`: parent eval session id.
- `contract_id`: source contract id.
- `repeats`: repeats per arm.
- `verdict`: `pass` or `fail` for whether the learning-on arm improved or tied
  with no extra measured cost/tokens.
- `arms`: one `EvalArmReport` per arm, including:
  - `arm`: `learning_on` or `learning_off`;
  - resolved `memory` config for that arm;
  - pass/fail/unverifiable counts and pass-rate;
  - total budget usage (cost, steps, input/output tokens);
  - child-run ids plus verdict refs, parent anchor refs, and child root/head
    hashes for traceability.
- `delta`: learning-on minus learning-off pass-rate, cost, steps, and tokens.
- `summary`: concise human-readable comparison.
- `report_ref`: ledger ref of the signed `eval_report` event after recording.

## Measurement Only

SI-4 does not tune policy, change routing, promote configuration deltas, or
auto-apply anything based on the result. The report is a signal source only.

L3 remains deferred: a future optimizer may propose policy/config deltas from
many `EvalReport`s, but those proposals must use the same governed
store/provenance/reviewer gate described in the L2/L3 architecture docs.

## Consequences

L2 can now be judged by paired evidence instead of assertion. The cost is that
evals may spend roughly twice the budget of a single contract times repeat
count, but the report makes that spend visible and auditable.
