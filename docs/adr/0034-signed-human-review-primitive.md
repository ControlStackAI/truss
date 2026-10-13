# ADR 0034: Signed Human Review Primitive

Status: Accepted

## Context

Phase 4 adds a human-facing verdict-review surface. The surface is only
trustworthy if a human approval, rejection, or needs-changes decision becomes
ledger provenance instead of presentation state. The decision must be
tamper-evident, attributable to the reviewer key, and pinned to the exact ledger
state the human reviewed.

## Decision

Truss adds a signed `human_review` journal entry in core. Its payload contains:

- `decision`: `approved`, `rejected`, or `needs_changes`;
- `note`: an optional reviewer note;
- `attested_head_hash`: the session head hash being reviewed at decision time;
- `reviewer_key_id`: the ed25519 public-key id of the signer that recorded the
  review.

`Store::record_human_review` is the single write API. It reads the session head
hash, builds the typed payload, and appends it through the existing journal
append path so the entry is hash-chained and signed. `Store::verify_chain`
continues to verify normal run entries against the session key and verifies
`human_review` entries against their payload `reviewer_key_id`.

`truss review approve|reject|needs-changes <session>` records the entry without
requiring a terminal UI. `truss ledger verdict` surfaces the latest human
sign-off alongside the machine verdict and reports whether the attested reviewed
head still matches the current reviewable head. The machine `Verdict` remains
unchanged; human sign-off is a separate trust signal.

## Consequences

Human review decisions are durable provenance and survive export/verification.
An approval does not mutate, overwrite, or launder the machine verdict. Absence
of a human approval remains non-approval.

Because the review entry itself advances the hash chain, stale-signoff detection
compares the attested hash with the latest non-`human_review` entry hash while
the full chain head remains visible separately. If later work appends new
non-review evidence after approval, the verdict bundle reports the sign-off as
not matching the current reviewable head.
