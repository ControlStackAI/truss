# ADR 0017: SWE-bench Verified Adapter

Status: Accepted

Date: 2026-06-17

## Context

RECON's benchmark landscape ranked SWE-bench Verified as the first real-world
corpus to integrate: 500 human-verified GitHub issue tasks, MIT license, and a
grader that is exactly Truss-shaped: apply a candidate patch, run the repository
tests, and resolve the task only when the selected tests pass.

The dataset itself is not part of this repository. Real runs require a cached
HuggingFace export of `princeton-nlp/SWE-bench_Verified`, candidate predictions,
the SWE-bench harness, Docker, and task images. Those dependencies are useful for
nightly/live evaluation, but they do not belong in default CI.

GAIA remains excluded from CI and this adapter because it is gated and cannot be
redistributed into public/open test fixtures.

## Decision

Add a real SWE-bench Verified adapter to `truss-bench`:

- `SweBenchTask` is a typed record for the HuggingFace fields: `instance_id`,
  `repo`, `base_commit`, `problem_statement`, `patch`, `test_patch`,
  `FAIL_TO_PASS`, `PASS_TO_PASS`, and optional metadata.
- `load_swebench_tasks(path)` reads local JSON arrays, single JSON objects, or
  JSONL records. It never fetches the dataset.
- `SweBenchAdapter` maps a task into a Truss `Contract` whose intent is the
  `problem_statement` and whose `check` clause applies the candidate patch,
  applies the task `test_patch`, and runs `FAIL_TO_PASS + PASS_TO_PASS`.
- The benchmark runner records a signed `swebench_result` ledger event:
  `{ instance_id, patch_hash, resolved }`.

Default tests use a tiny in-repo fake SWE-bench record and local repo fixture.
They exercise parsing, contract mapping, paired learning-on/off reporting, and
the signed ledger record without Docker, network, or external data.

## Live Execution Boundary

The official SWE-bench harness path is behind the disabled-by-default
`swebench-live` Cargo feature. With that feature enabled, the CLI exposes:

```sh
cargo run -p truss-cli --features swebench-live -- \
  swebench \
  --dataset /path/to/SWE-bench_Verified.jsonl \
  --checkout-root /path/to/checkouts \
  --predictions /path/to/predictions.jsonl \
  --limit 5 \
  --repeats 1
```

The command assumes the operator has already cached the dataset and installed the
official SWE-bench harness with Docker access. The code passes local paths to the
harness; it does not fetch HuggingFace data and it does not make Docker a
default build dependency.

## Consequences

SWE-bench becomes a first-class benchmark adapter while preserving the product
boundary: `truss-core` still owns contracts, evals, and the signed ledger;
`truss-bench` owns dataset shape and benchmark mapping.

Default CI stays hermetic. Live SWE-bench runs are intentionally opt-in because
they are heavyweight, stateful, and environment-dependent.
