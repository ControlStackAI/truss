# Ledger Read Tools — querying the verification chain

Status: design (pre-build). Milestone arc: LR-1 → LR-4. First ADR: 0027 (with LR-1).

## Why

Truss already produces a signed, hash-chained journal per run — contracts, model results,
tool/self-validation runs, compaction events, child anchors, and the final verdict, all
ed25519-signed (`store.rs`). But the **only** way to read it today is `truss --verify`, which
answers a single yes/no ("does the chain verify?"). The whole point of the moat — *review
proofs, not transcripts* — needs a way to actually **read the proofs**: what was the contract,
what evidence was produced, what did the N-reviewer panel say, what got compacted, what can be
recalled. Without a read surface, the verification chain is a vault with no door.

This also unblocks two downstream things: the **verdict-review UI** (it consumes a read API,
not the raw DB) and **buyer/diligence** (export a portable evidence bundle).

## What exists to build on

`Store` already exposes the needed read primitives — this is mostly a clean *read API + CLI
surface*, not new storage:
`get_session`, `entries`, `last_entry`, `head_hash_hex`, `anchor_count`,
`list_suspended_sessions`, `verify_chain`, `verify_chain_pinned`. (LR-1 adds a read-only
`list_sessions` query; everything else is composition.)

## Design

### `LedgerReader` (truss-core, read-only)

A thin read-only view over the existing `Store`. Never mutates the chain. Every read reports
**verification status** alongside content (so a reader always knows whether what they're
looking at is signature-valid). Output is structured; the CLI renders human-readable by
default and `--json` for programmatic/UI consumption — **the `--json` schema IS the
provenance read API** the verdict-review UI and diligence tooling consume.

### CLI: `truss ledger <subcommand>`

- `truss ledger list` — sessions/runs in the db: id, started, head hash, verdict status, #entries.
- `truss ledger show <run-id>` — the chain in order: each entry's kind, seq, hash, signed?, and a
  one-line summary; head hash; overall verify result. The "read the proof" view.
- `truss ledger get <run-id> <seq>` — full payload of one entry (`--json` for raw).
- `truss ledger verdict <run-id>` — the verdict bundle: contract intent + DoD, harness-run
  check results, N-reviewer panel verdicts (per-lens), PASS/FAIL/UNVERIFIABLE. (LR-2)
- `truss ledger export <run-id>` — portable signed evidence bundle (JSON): contract → evidence →
  verdict → head hash + anchor refs. Anchorable, diff-able, hand to a buyer. (LR-2)
- `truss ledger recall <run-id> [--memory <id>]` — recall memory entries / compacted
  `ContextSummary` content for a run (ties to the compaction controller — "show me what was
  summarized"). (LR-3)
- `truss ledger diff <run-a> <run-b>` — structural diff of two runs (regression/eval narrative). (LR-4)
- `truss ledger verify <run-id>` — consolidates today's `--verify` under the `ledger` family
  (keep `--verify` as a back-compat alias). (LR-3)

## Milestones (1 milestone = 1 PR + 1 ADR section)

- **LR-1 — `LedgerReader` + `truss ledger list|show|get` + `--json` (START HERE).**
  Read-only reader over `Store`; add `list_sessions`; render chain with per-entry verify status;
  stable `--json` schema. No storage changes, no write paths. Hermetic tests (list/show/get on a
  seeded db; `--json` shape; tampered entry shows as unverified, not a crash). → ADR 0027.
- **LR-2 — `verdict` + `export`.** Verdict bundle view + portable signed evidence-bundle JSON
  (the diligence artifact). → ADR 0028 (renumber if taken).
- **LR-3 — `recall` + `verify` consolidation.** Recall memory/compacted summaries; fold
  `--verify` under `ledger verify` (alias kept).
- **LR-4 — `diff`.** Compare two runs for regression/eval stories.

## Invariants

1. **Read-only.** The ledger tools never mutate the chain (no new write paths in this arc).
2. Every read reports verification status; a tampered/unsigned entry renders as *unverified*,
   never silently as trusted, and never panics.
3. The `--json` output is a **stable schema** = the programmatic provenance read API (UI +
   diligence depend on it); version it from LR-1.
4. No new dependencies in `truss-core` (composition over the existing `Store`).
5. CLI surface follows the existing flag/config conventions; clap stays out of `truss-core`.
