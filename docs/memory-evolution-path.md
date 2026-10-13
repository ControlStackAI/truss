# Memory Evolution Path (simple now → complex later, swap not rewrite)

> Status: forward-compat contract. Captured 2026-06-17 per Matt: "the simple
> memory is OK for now, but know the path forward and build with it in mind —
> make it easy to update to a more complex memory scheme."
> This doc names the seam, the staged upgrade path, and the invariants that must
> hold so each upgrade is a backend swap behind a stable interface.

## The upgrade seam (already in place)

Everything above the substrate talks to memory through **three stable types** in
`truss-core::memory`. A more complex scheme is a new implementation behind these,
never a change to the callers:

- **`trait MemoryStore`** — `write / promote / reject / mark_stale / get / query /
  delta_since / write_policy / get_policy`. The ONE thing the rest of the harness
  depends on. `SqliteMemoryStore` is the current impl; a complex scheme is a new
  impl (or a decorator) implementing the same trait.
- **`MemoryEntry`** — already carries `trust`, `provenance`, `salience` (0–1 for
  ranking/eviction), `key` (supersede slug), `superseded_by`. The fields a richer
  scheme needs already exist.
- **`MemoryQuery`** — `kind / key / min_trust / contains / limit`. `contains` is
  already annotated "backed by a vector index" — semantic recall slots in here
  without changing the call sites.

**Rule we build by:** callers (SI-1 extraction, SI-2 promotion, SI-3 recall, M4
injection) only ever go through `MemoryStore` + `MemoryQuery`. No caller reaches
into SQLite or assumes exact-key lookup. (Verified: SI-3 recall is query-shaped
and token-budgeted, not key-coupled.)

## Invariants every future scheme must preserve

These are the moat; an upgrade may NOT weaken them:
1. **Writes always enter `Unverified`.** Promotion stays a separate, panel-gated step.
2. **`Verified` always means "N-reviewer panel signed off."** Retrieval can get
   smarter; the trust label's meaning is fixed.
3. **Provenance (`LedgerRef`) on every entry**, validated under `--verify`.
4. **Reversibility** — supersede/demote-to-`Stale` stays traceable in the ledger.
5. **Verified-only default view** — richer recall must still default to trusted-only.

So long as a new backend honors these, it can change *how* entries are stored and
ranked freely.

## Staged path forward (each stage = new/extended `MemoryStore`, callers untouched)

- **S0 — current (simple).** SQLite content-addressed store; exact-key + `contains`
  substring; trust tiers + provenance + supersede. Good enough for L2 now.
- **S1 — shipped in 0.55.0: semantic recall (vector).** Adds an embedder seam,
  embedding-on-write for semantic stores, explicit `truss memory reindex`
  backfill, and vector-ranked `MemoryQuery.contains` behind `query()`. The
  config gate is `memory.recall = "exact" | "semantic" | "hybrid"`, with
  `exact` as the default. The baseline ranker is exact brute-force cosine over
  stored vectors; sqlite-vec / ANN remains the S2-era scale swap.
- **S2 — hybrid ranking.** BM25 + vector + `salience` + recency blend in `query()`
  (mirrors OpenClaw's hybrid search). Still just `query()` internals.
- **S3 — salience & eviction / compaction.** Activate the existing `salience`
  field: a scoring pass + budgeted eviction/summarization of cold entries
  (hierarchical/episodic compaction). Caps already exist (recall token budget).
- **S4 — episodic vs semantic split / associative (graph) memory.** New
  `EntryKind`s and/or a graph decorator over `MemoryStore` for linked recall.
  Additive — new kinds, same trait.
- **S5 — scope tiers as backends.** Per-agent / per-project / shared L2 can each be
  a different `MemoryStore` impl composed behind one facade (already an enumerated
  scope knob).

## What this means for current building

- Keep all memory access behind `MemoryStore` + `MemoryQuery` (enforced now).
- Treat `contains` as the semantic-recall entry point; don't add a parallel
  retrieval API.
- Keep `salience` populated-or-zero so S3 can turn on without a migration.
- S1 is now in place behind config. Keep future recall improvements behind
  `MemoryStore::query`; do not claim task-outcome gains until SI-4 paired eval
  measures them.

Net: the simple scheme ships now; the complex scheme is a backend swap we've
already cleared the runway for.
