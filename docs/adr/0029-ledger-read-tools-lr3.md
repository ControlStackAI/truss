# ADR 0029: Ledger Read Tools LR-3

Status: Accepted

Date: 2026-06-18

## Context

LR-1 made signed ledger chains readable. LR-2 added verdict and export views.
Two read gaps remained:

- governed compaction stores `ContextSummary` memory entries and
  `compaction_performed` journal events, but there was no ledger-native recall
  view proving the original elided journal range was still recoverable;
- verification existed as top-level `truss --verify`, outside the `truss
  ledger` namespace that now owns ledger reads.

## Decision

Add two read-only ledger commands:

- `truss ledger recall <run-id> [--summary <memory-id>] [--json]`
- `truss ledger verify <run-id> [--json]`

`recall` renders memory entries whose provenance references the run, including
typed `EntryKind`, trust tier, provenance refs, and verification status. It also
renders each `compaction_performed` entry with its `summary_memory_id` and
inclusive `elided_range`. When `--summary` is supplied, the reader expands that
summary id back to the exact signed journal entries in the elided range. The
journal remains authoritative; recall never recomputes or re-summarizes content.

`verify` runs the same chain/signature/pinned-key verification as the top-level
`--verify` path and reports both overall and per-entry status. The old
`truss --verify <run-id>` flag remains supported as a back-compat alias that now
delegates to the same reader/rendering path as `truss ledger verify`.

All LR-3 views stay under the existing stable schema version,
`truss.ledger.v1`.

## JSON Schema

`recall --json`:

```json
{
  "schema_version": "truss.ledger.v1",
  "command": "recall",
  "recall": {
    "session_id": "...",
    "head_hash": "...",
    "verification": {},
    "memory_entries": [
      {
        "id": "...",
        "kind": "context_summary",
        "key": "...",
        "body": "...",
        "trust": "unverified",
        "produced_by": { "session": "...", "seq": 4 },
        "verified_by_ref": null,
        "sources": [{ "session": "...", "seq": 3 }],
        "created_ms": 0,
        "verification_status": "verified"
      }
    ],
    "compactions": [
      {
        "summary_memory_id": "...",
        "mode": "summarize",
        "elided_range": {
          "start": { "session": "...", "seq": 3 },
          "end": { "session": "...", "seq": 4 }
        },
        "original_entry_count": 2,
        "recoverable": true,
        "entry": {}
      }
    ],
    "expanded_summary": {
      "summary_memory_id": "...",
      "elided_range": {},
      "entries": []
    }
  }
}
```

`verify --json`:

```json
{
  "schema_version": "truss.ledger.v1",
  "command": "verify",
  "verify": {
    "session_id": "...",
    "head_hash": "...",
    "verification": {
      "status": "verified",
      "verified_entries": 5,
      "failed_at_seq": null
    },
    "entries": [
      {
        "seq": 0,
        "kind": "run_started",
        "hash": "...",
        "short_hash": "...",
        "signed": true,
        "payload": {},
        "verification_status": "verified"
      }
    ]
  }
}
```

## Consequences

LR-3 remains read-only: no new storage schema, no new write paths, no replayed
model calls, no reviewer reruns, and no summary recomputation. Recall is a
render of already-recorded memory and journal data.

Tampered or unsigned material is surfaced as `unverified`. The reader still
renders available data for inspection, but does not silently trust it.

Out of scope remains LR-4: diffing two runs.
