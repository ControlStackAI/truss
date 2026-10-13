# Event And Ledger Consumers

Most enterprise plugins should be consumers, not runtime extensions. If a plugin
only renders, exports, alerts, indexes, or visualizes a run, it should consume
the event stream or ledger without adding new journal entries.

## Live event consumers

`AgentEvent` is the live stream:

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
let ctx = Ctx::new().with_events(tx);

while let Some(event) = rx.recv().await {
    println!("{}", serde_json::to_string(&event)?);
}
```

Events are for observation. A UI can show `AssistantTextDelta`, tool requests,
tool results, context pressure, child runs, fan-out, memory writes, and run
completion without becoming part of the signed execution source of truth.

If an event consumer decides something that affects the run, that decision must
come back through a governed runtime seam and be journaled there.

## Ledger consumers

`LedgerReader` and the `truss ledger` commands are for durable, read-only
evidence:

```bash
truss --db harness.db ledger list
truss --db harness.db ledger show <run-id>
truss --db harness.db ledger verdict <run-id> --json
truss --db harness.db ledger export <run-id> \
  | python3 examples/plugin-ledger-consumer.py --fail-on-unverified
```

The export bundle is self-contained: contract summary, evidence, signed entries,
anchor refs, child sessions, and offline verification status.

## Consumer checklist

- Treat ledger data as immutable.
- Verify before trusting: use `ledger verify`, `ledger verdict`, or export
  `offline_verification`.
- Keep consumer-derived presentation state off-journal.
- If you cache derived views, store the source run id and head hash with the
  cache entry.
- Do not re-run tools or model calls from a consumer.
