# ADR 0005 — Memory tool binding

- **Status:** Proposed
- **Date:** 2026-06-17
- **Deciders:** Matt (owner), ALINA
- **Builds on:** [ADR 0004 — Governed memory substrate](0004-governed-memory-substrate.md)

## Context

ADR 0004 landed the governed SQLite memory substrate but left it unreachable by
the model. M2 binds that substrate to the native Anthropic-trained
`memory_20250818` surface while preserving the M1 trust boundary: model writes
are stored, but not trusted.

## Decision

Register a `memory` tool in the Truss tool registry and advertise it as the
native `memory_20250818` tool only when a per-session `memory_tool` flag is
enabled. The default remains off, so existing durable and non-durable runs do
not see or execute the memory tool.

The binding maps filesystem-like memory commands onto `MemoryStore`:

- `create`, `insert`, and `str_replace` write `MemoryDraft`s that always land as
  `Unverified`.
- `view` queries with the default `Verified` trust floor and renders a
  directory/file-style listing.
- `delete` marks active entries stale.
- `rename` writes a replacement entry and marks the source stale.

Entry kind is inferred from path prefixes:

- `/memories/goal/` or `/goals/` → `Goal`
- `/decisions/` → `Decision`
- `/lessons/` or `/memories/lessons/` → `Lesson`
- `/facts/` → `Fact`
- `/artifacts/` → `Artifact`
- anything else → `Scratch`

Durable runs pass the journal `model_result` sequence containing the tool call
into tool execution as `LedgerRef { session, seq }`. Memory writes stamp that
reference as `produced_by`, so each entry points at the signed journal record
that caused it.

`AgentEvent` now includes `MemoryWrite` and `MemoryRecall` so UIs and audit
consumers can observe memory activity without scraping tool output.

## Consequences

The model can now use governed memory through a native tool surface, but no
memory appears unless the session explicitly enables it. Default recall remains
verified-only; M3 can add promotion policy without changing the tool protocol.
