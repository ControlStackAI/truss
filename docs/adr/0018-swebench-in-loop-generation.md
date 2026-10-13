# ADR 0018: SWE-bench In-Loop Generation

Status: Accepted

Date: 2026-06-18

## Context

The first SWE-bench Verified adapter graded existing prediction JSONL files. That
was enough to validate the official fail-closed Docker grading path, but it did
not exercise Truss as an agent harness. In particular, `truss swebench` could
prove whether a patch resolved an instance, but the patch itself had to come from
outside Truss, often from a delegated vendor CLI.

That left a product gap: SWE-bench should also be able to measure Truss's own
loop, where `Executor::InLoop(provider)` drives the provider through Truss's
filesystem and bash tools inside the task checkout.

## Decision

Add an in-loop SWE-bench solve mode to the live CLI:

```sh
truss --provider openai-codex swebench \
  --solve in-loop \
  --dataset /path/to/verified.jsonl \
  --checkout-root /path/to/checkouts \
  --predictions /path/to/generated.jsonl \
  --run-id unique-run-id
```

`--solve predictions` remains the default and requires `--predictions`. It keeps
the existing product tier for externally generated patches.

`--solve in-loop` does the following for each selected task:

1. Prepare a per-instance checkout under `--checkout-root`, checking out the
   task `base_commit`.
2. Build a Truss `Contract` whose intent is the SWE-bench `problem_statement`
   and whose constraints scope reads and writes to that checkout.
3. Run `Executor::InLoop(provider)` with the default filesystem and bash tool
   registry.
4. Capture `git diff --binary HEAD` as `model_patch` in an official predictions
   JSONL row.
5. Hand that predictions file to the existing official SWE-bench Docker grading
   path.

The grading wrapper uses a no-op in-process provider in generated mode so the
official Docker harness is the sole pass/fail authority after generation. Empty
or whitespace-only diffs are intentionally written as empty `model_patch` values,
which the existing preflight rejects, preserving the fail-closed invariant.

Provider selection is the normal provider registry path. `openai-codex` is the
subscription-backed in-process provider used for development validation.
`anthropic` remains selectable for API-key-backed runs, but is metered and should
only be run when explicitly authorized.

## Consequences

SWE-bench now has three honest tiers:

- predictions JSONL: grade patches generated elsewhere.
- delegated CLI: a vendor agent owns the inner loop; Truss verifies at the
  contract boundary.
- in-loop: Truss owns the loop, tools, policy, durable ledger, and diff capture.

The in-loop path gives direct debugging signal about the harness itself: whether
models can discover files through Truss tools, edit correctly, run tests, and
stop with a useful diff. Its score may initially be low; that is useful product
evidence rather than a grading failure.
