# ADR 0028: Ledger Read Tools LR-2

Status: Accepted

Date: 2026-06-18

## Context

LR-1 made the durable ledger readable as a signed chain. The next buyer-facing
question is not "what happened in every transcript turn," but "was the contract
met, what evidence supports that verdict, and can I take the proof away and
verify it offline?"

The existing durable journal already contains the required material: the
recorded contract intent, harness-owned check results, optional
`self_validation` evidence, reviewer-panel evidence, the signed verdict entry,
hash-chain material, the session signing key id, and any anchor receipts.

## Decision

Extend the read-only `LedgerReader` with two LR-2 views:

- `verdict(<run-id>)`: renders the recorded verdict bundle for a run.
- `export(<run-id>)`: emits a portable JSON evidence bundle for offline
  verification.

Add CLI commands:

- `truss ledger verdict <run-id>` with human-readable default output and
  `--json`.
- `truss ledger export <run-id>` as JSON-native output.

Both commands keep LR-1's stable schema version, `truss.ledger.v1`. LR-2 adds
new command shapes instead of changing the LR-1 `list`, `show`, or `get`
schemas.

## JSON Schema

`verdict --json`:

```json
{
  "schema_version": "truss.ledger.v1",
  "command": "verdict",
  "verdict": {
    "session_id": "...",
    "contract": {
      "id": "...",
      "title": "...",
      "intent": "...",
      "definition_of_done": []
    },
    "evidence": {
      "checks": [],
      "self_validation": [],
      "panel": []
    },
    "recorded_overall": "PASS",
    "overall": "PASS",
    "verdict_entry": {},
    "head_hash": "...",
    "anchor_refs": [],
    "verification": {}
  }
}
```

`export`:

```json
{
  "schema_version": "truss.ledger.v1",
  "command": "export",
  "contract": {},
  "evidence": {
    "checks": [],
    "self_validation": [],
    "panel": []
  },
  "verdict": {},
  "ledger": {
    "session_id": "...",
    "pubkey_id": "...",
    "head_hash": "...",
    "verification": {},
    "entries": [
      {
        "seq": 0,
        "kind": "run_started",
        "payload": {},
        "prev_hash": "...",
        "entry_hash": "...",
        "signature": "...",
        "signed": true,
        "verification_status": "verified"
      }
    ],
    "anchor_refs": [],
    "child_sessions": []
  },
  "offline_verification": {
    "algorithm": "sha256(prev_hash || kind || canonical_json(payload) || seq_le)",
    "signature_algorithm": "ed25519",
    "status": "verified"
  }
}
```

## Consequences

The verdict view is a render of already-recorded journal data. It never reruns
checks, reconvenes reviewers, or recomputes a verdict. If chain or signature
verification fails, the view reports `unverified` and treats the trusted overall
as `UNVERIFIABLE` while still showing the recorded value as untrusted evidence.

The export bundle is self-contained for offline integrity checks: it carries the
signed entries, previous hashes, entry hashes, signatures, session public key id,
head hash, anchor references, and recursively included child sessions referenced
by child anchors. No storage schema changes or write paths are introduced.

Out of scope remains LR-3/LR-4: recall, ledger verify consolidation, and diffing
between runs.
