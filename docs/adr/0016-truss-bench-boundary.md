# ADR 0016: Truss Bench Crate Boundary

Status: Accepted

Date: 2026-06-17

## Context

`truss_core::eval` is now a product primitive: it records paired learning-on/off
contract runs through `child_run`, emits signed `EvalReport`s, and gives Truss a
way to prove governed learning helps. That primitive belongs in core because it
is part of the shipped runtime and CLI surface.

Real-world benchmark and simulated-user tooling have a different lifecycle.
They will accrete external dataset adapters, local fixtures, test personas,
research-only glue, and eventually heavier optional dependencies. Keeping that
surface inside core would blur the runtime boundary and make shipped crates carry
testing-only concerns.

## Decision

Add `crates/truss-bench` as a separate workspace crate for benchmark and
interaction-UX tooling. It depends on `truss-core`; `truss-core` does not depend
on it.

The crate introduces:

- `BenchmarkTask`: the shared identity surface for external benchmark items.
- `BenchmarkAdapter`: a trait that maps a benchmark task into a Truss
  `Contract`.
- `SweBenchStyleAdapter`: a local SWE-bench-style adapter where a repo fixture
  plus shell/test command becomes the contract's harness-owned `check` clause.
- `BenchmarkRunner`: a thin runner that executes adapted contracts via
  `truss_core::eval::run_paired_eval` and aggregates a serializable
  `BenchmarkReport`.
- `SimulatedUser`: a provider-backed persona/goal harness that drives
  multi-turn interaction with an agent provider and records turn metrics plus a
  score/legibility rating.

The key model is that benchmark graders become contract `done` checks. For the
SWE-bench-style local adapter, this means `{ workdir, intent, test_cmd }` maps to
a `Contract` whose `done` clause runs `cd <workdir> && <test_cmd>` and expects
exit 0. No real SWE-bench data or network access is pulled into this scaffold.

## Consequences

The core eval primitive stays in `truss-core`. `truss-bench` builds on it instead
of moving it because the primitive is useful to shipped Truss workflows while
dataset adapters and simulated-user personas are explicitly testing tooling.

The first scaffold remains hermetic: in-repo fixture, fake providers, no
external datasets, and no network. Heavy adapters should be feature-gated when
added.

Follow-up dataset adapters are pending RECON's landscape research, especially:

- SWE-bench Verified
- GAIA
- τ-bench
