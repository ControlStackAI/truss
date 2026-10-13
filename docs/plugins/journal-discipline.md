# Journal Discipline

The signed ledger is for facts Truss may need to replay, prove, or trust later.
It is not a dumping ground for every UI notification.

## Journal it

Write or reuse a journaled path when the extension:

- calls a model or delegated executor;
- invokes a tool whose output feeds the model or verifier;
- creates or modifies memory, identity, policy, or durable context;
- performs a side effect that should not run twice after resume;
- records contract evidence, self-validation, reviewer output, verdicts, child
  anchors, fan-out reductions, compactions, or MCP provenance;
- publishes an anchor receipt.

## Keep it off-journal

Do not journal work that only:

- renders a UI;
- streams token deltas for presentation;
- emits desktop/chat/web notifications;
- indexes an already-signed export for search;
- draws charts or dashboards from ledger data;
- records local debug traces that are not evidence.

These consumers should keep a pointer to the source run id and head hash if they
persist derived state.

## Secret handling

- Providers resolve credentials internally.
- MCP HTTP secrets belong in `env_headers`, not static config.
- Anchor backends publish hashes only.
- Errors and event previews should be sanitized.
- Ledger payloads should contain evidence and hashes, not raw secrets.

## Replay rule

If resume would be wrong after re-executing it, the operation needs a recorded
result or memo. If resume can recompute it safely from signed data, keep it as a
consumer.
