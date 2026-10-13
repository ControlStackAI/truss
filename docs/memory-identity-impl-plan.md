# Truss Memory & Identity — Implementation Plan

Status: build-ready, 2026-06-16. Companion to
[`memory-identity-architecture.md`](memory-identity-architecture.md) (the design)
and the drafted skeleton at `crates/truss-core/src/memory/`. Each milestone is
independently shippable, behind the same ADR/feature discipline as the provider
arc (v0.7→0.11). Acceptance = tests green + clippy/fmt clean.

**Goal restated** (from the `#truss` discussion): give Truss a *typed,
trust-tiered, provenance-tracked* memory the model reaches via the native
`memory_20250818` tool, plus a *layered, cache-aligned* identity/context injection
(L0–L4) that replaces flat-markdown dumping — optimized for Fable-5/Mythos, the
model with the highest return on a real memory surface. Two user-selectable knobs:
`VerificationPolicy` and `IdentityPolicy`.

Naming: `truss-core::identity` = ed25519 signing key (existing). Agent persona =
`memory::persona` (new). Memory lives as a **module in truss-core**, reusing the
SQLite store, `Signer`, and `delegation::Verdict`.

---

## M1 — Store + provenance (foundation)

**Why first:** everything else writes to or reads from the store. It's
self-contained, pure-ish, and fully unit-testable with no model in the loop —
exactly how the durable ledger was built.

Tasks:
1. Wire the module: add `pub mod memory;` to `lib.rs`. Resolve any compile drift
   in the drafted skeleton (it references `delegation::Verdict`, `store`, `identity`).
2. `SqliteMemoryStore`: implement the `MemoryStore` trait against the existing
   durable DB connection (share `Arc<Mutex<Connection>>` with `crate::store::Store`,
   or open the same file). Add the `memory` table from the schema sketch in
   `store.rs` (+ `write_seq` monotonic counter, index on `(mem_key, trust)` and
   `write_seq`).
3. Content-addressing: `MemoryId` = hex sha256 over `kind || key || body` (reuse
   the `sha2` dep already in `store.rs`).
4. Supersede semantics: `write` of a `Fact`/`Lesson` with an existing `mem_key`
   marks the prior entry `superseded_by` + bumps `write_seq`.
5. `delta_since(cursor)` = `WHERE write_seq > cursor ORDER BY write_seq`.
6. Provenance: persist `produced_by` (session, seq) and (on promote) the full
   `Verdict` JSON.

Tests (`crates/truss-core/tests/memory_store_test.rs`):
- write → `get` round-trips; lands `Unverified`.
- supersede-by-key sets `superseded_by`, old entry no longer in default query.
- `query` default returns `Verified` only; `min_trust` floor respected.
- `promote` attaches verdict, flips tier, appears in next `delta_since`.
- `delta_since` returns only post-cursor changes (incl. tier changes).

Acceptance: new tests green; existing 60 unchanged; clippy/fmt clean.
ADR 0004 "Governed memory substrate."

---

## M2 — Memory tool binding (`memory_20250818`)

**Why second:** makes the store reachable by the model using the tool Anthropic
already trained Fable on — no bespoke protocol.

Tasks:
1. Register a `memory` tool (`type: "memory_20250818"`) in the tool registry
   (`tools/mod.rs`), gated by a session flag so existing runs are unaffected.
2. Map commands → `MemoryStore`:
   - `create`/`insert`/`str_replace` → `write` (Unverified). Infer `kind` from the
     path prefix (`/memories/goal/…`, `/decisions/…`, `/lessons/…`, `/facts/…`,
     `/artifacts/…`, else `scratch`).
   - `view` → `query` (default `Verified`-only); render as a dir/file listing the
     model expects.
   - `delete`/`rename` → supersede / `mark_stale`.
3. Provenance capture: stamp `produced_by` with the current journal `seq` at the
   moment the tool call is journaled (the binding runs inside the agent loop, which
   already writes the ledger).
4. Emit `AgentEvent`s for memory ops (extend `events.rs`: `MemoryWrite`,
   `MemoryRecall`) so the UI/audit log sees them.

Tests (hermetic, fake provider emitting tool calls — mirror `agent_loop_test.rs`):
- model `create` under `/memories/lessons/x` → entry exists, `kind=Lesson`, Unverified.
- model `view` returns only Verified entries.
- path→kind inference table.

Acceptance: hermetic tests green; flag-off path identical to today.

---

## M3 — Trust tiers + promotion (the moat)

**Why third:** turns "stored" into "trusted." Reuses the N-reviewer panel +
signed verdict end-to-end — the same code that gates contracts now gates belief.

Tasks:
1. `VerificationPolicy` resolution at session build (`MemoryConfig`), layered
   global → per-agent → per-contract (extend the contract `[memory]` block).
2. Promotion driver:
   - `Eager`: on write, run the N-reviewer panel (`delegation`) on the claim →
     PASS calls `promote`, FAIL calls `reject`.
   - `Lazy`: on `view`/recall of an `Unverified` entry, verify-then-surface;
     cache the verdict so it runs once.
   - `Tiered`: heuristic gate (cheap model or rule) always; full panel only for
     `Fact`/`Decision` (`MemoryConfig::verify_on_write`).
   - `Off`: never promote; `view` may still return Unverified but **tagged**.
3. Contradiction handling: a new Verified entry that supersedes a key demotes the
   prior to `Stale`.
4. Recall honesty: entries surfaced to the model always carry their trust tier in
   the rendered view (no silent trust laundering).

Tests:
- Eager: a verdict-PASS claim promotes; FAIL rejects and is never surfaced.
- Lazy: Unverified entry triggers one verification on first recall, then cached.
- Off: Unverified entries surface tagged, never as Verified.
- supersede demotes prior to Stale.

Acceptance: each policy mode covered; verdict reuse verified by sharing
`delegation` types (no new verdict path).

---

## M4 — Layered injection (L0–L4) + identity

**Why fourth:** assembles the prompt from the tiers and aligns them to the cache.
This is where the "stop dumping 37KB markdown" win actually lands.

Tasks:
1. Context assembler: build `tools → system(L0 core, L0 self, L1) → messages(L3
   goal ledger, history, L2 pulls, L4 scratch)`. Place `cache_control` so L0/L1
   are the stable cached prefix.
2. `Persona`: load/persist `PersonaCore` (operator-signed via `identity::Signer`)
   + `PersonaSelf`; `render()` into L0. Verify the core signature on load.
3. Goal ledger (L3): pin the active contract intent + DoD; re-inject every N
   steps; never compacted.
4. Delta rehydration: on resume/re-pin, inject `delta_since(cursor)` for L2, not
   the whole store.
5. `IdentityPolicy` + amendment flow: `route_amendment` → buffer
   `CommitAtBoundary`; apply at next session build as a reversible journal event;
   verdict-check `self` against `core` before commit. A dedicated signed
   `amend_identity` tool (deliberate, injection-resistant).

Tests:
- assembled prompt has stable L0/L1 prefix across turns (cache key unchanged when
  only L3/L4 change).
- core signature mismatch refuses to load.
- amendment under `Propose` needs approval; under `SelfAmend` commits at boundary
  and is rollback-able; an amendment targeting `core` is `Denied`.

Acceptance: a long-run example shows cache-read tokens dominating (prefix held),
and goal-ledger re-pin keeps intent across compaction.

---

## M5 — Fable tuning + salience

Tasks:
1. Provider-aware L0/L3 memory-format instruction (Fable: "write learnings here,
   this format, consult next session"); provider-agnostic fallback tool pair
   (`read_memory`/`write_memory`) for models without `memory_20250818`.
2. Salience scoring + eviction (recency + recall-count + optional model score);
   `Stale` decay policy.
3. Optional vector backing behind `MemoryStore` for semantic recall (no
   agent-loop change).
4. Async sub-agent shared-L2 plumbing (ties into the multi-agent thread).

Acceptance: Fable run uses the memory tool unprompted-but-guided; salience evicts
without losing Verified high-salience entries.

---

## Cross-cutting

- **Config surface:** `[memory]` contract block + global config: `verification`,
  `identity`, `format`, re-pin cadence `N`.
- **Open questions to settle before M4** (M1–M3 don't depend on them): salience
  policy details, L2 scope (per-agent / per-project / shared trust boundaries),
  conflict resolution when two Verified facts collide.
- **Each milestone = one PR + one ADR section**, same cadence as the provider arc.

---

## Where to start

**M1, task 1–2: wire the module + `SqliteMemoryStore`.** It's the foundation
every later milestone reads/writes, it's fully testable without a model or a
provider, and it reuses machinery that already exists (SQLite store, `sha2`,
`Signer`, `delegation::Verdict`). One PR, hermetic tests, zero risk to the green
build until `pub mod memory;` flips on. Ship M1 → M2 → M3 in order; M1–M3 are the
moat, M4 is the injection win, M5 is the Fable polish.
