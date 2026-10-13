# ADR 0031: Diff-Aware Review Evidence

## Status

Accepted.

## Context

Verified delegation already runs a no-stake N-reviewer panel for `review`
clauses, but the panel judged the executor's final text. That text is the
executor's own narration, not evidence. A confused or dishonest executor can
claim one change while the working copy contains another.

## Decision

Capture a bounded git working-tree diff immediately after executor completion
and before checks/reviews. Review prompts now contain two explicit sections:

- `EXECUTOR'S CLAIMED SUMMARY`
- `ACTUAL CHANGES (diff)`

Reviewers are instructed to compare the claim against the diff and refute
unsupported or contradictory claims. The signed ledger records a `work_diff`
event plus the verdict payload includes the diff source, full diff hash, reviewed
diff hash, byte counts, and truncation flag.

## Diff Source

Primary source is a git working-tree baseline comparison rooted at the contract
write scope, then read scope, then current directory. Truss snapshots the
working-tree state into a temporary git tree before executor work, snapshots it
again immediately after executor completion, then diffs those two tree objects.
This captures what changed during the run, including untracked files, without
mutating the real index or requiring the checkout to be clean before the run.

If no git working copy is found but the contract has a write scope, Truss falls
back to a bounded filesystem snapshot of that scope before and after executor
work and synthesizes a simple file-level diff. If neither git nor a write scope
is available, Truss records an empty diff with an `unavailable` source marker.

## Bounds

The diff shown to reviewers is capped at 20,000 bytes. Truncated diffs include a
clear marker with the full diff length and full SHA-256. The ledger records both
the full diff hash and the exact reviewed-content hash so provenance identifies
what was actually reviewed.

## Consequences

- Review panels can catch claim-vs-reality mismatches in review-only contracts.
- Verdict bundles can surface reviewed diff provenance without embedding the
  entire diff in every human summary.
- A future non-git substrate should add pre-write snapshots or a first-class
  changed-file tracker before claiming synthesized diffs outside git.
