# ADR 0032: Verified Playbook Promotion and Recall

Status: Accepted

## Context

Verified Playbooks M1 added `EntryKind::Playbook`, markdown/SKILL.md import, and
signed playbook import/revoke events. Imported playbooks deliberately landed as
`Unverified` drafts. The next tranche needs playbooks to become trusted only
through the same trust path lessons already use, and then become available to
future runs through the existing M4 L2 context injector.

## Decision

Truss adds `truss playbook promote <id-or-key>`.

Promotion reuses the existing memory-claim review lane:

- load the playbook draft from governed memory;
- run `delegation::run_memory_claim_with_evidence`, which routes to the same
  N-reviewer panel used by contracts and lessons;
- require a strict majority of reviewers to pass;
- treat failed and unparsable panels as not promoted.

A passing panel records a signed `playbook_promoted` journal event and calls
`MemoryStore::promote_with_verdict_ref`. The resulting memory entry becomes
`Trust::Verified`, carries the reviewer `Verdict`, and stores
`Provenance.verified_by_ref` pointing at the signed event.

A failing or unverifiable panel records a signed
`playbook_promotion_rejected` event and leaves the playbook `Unverified`. That
differs from lesson extraction's eager reject path because `playbook promote` is
an operator-invoked draft promotion attempt; failed attempts should be auditable
without destroying the draft.

## Trusted Recall

Truss adds `recall_verified_playbooks`, mirroring `recall_verified_lessons`:

- query `EntryKind::Playbook`;
- require `min_trust = Verified`;
- rank by lexical overlap with the task context, then salience and recency;
- render an L2 block labeled `[L2 verified playbooks]` with provenance.

Playbook recall is opt-in through playbook-specific recall limits. Defaults keep
the request shape unchanged:

- `playbook_recall_max_playbooks = 0`;
- `playbook_recall_token_budget = 512`.

Setting either limit to `0` disables playbook recall. Unverified, rejected, and
stale playbooks never enter the trusted L2 playbook block.

## Consequences

Verified Playbooks now share the same reviewer-panel trust semantics and
ledger-backed provenance as verified lessons. The only new path is the
playbook-specific CLI/audit event surface; trust mutation still uses the
kind-agnostic memory store.

The no-fake-trust boundary remains explicit: import creates a draft, promotion
must pass the panel, and recall is verified-only.
