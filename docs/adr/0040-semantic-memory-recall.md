# ADR 0040: Semantic Memory Recall Backend

Status: Accepted

## Context

The governed memory substrate already exposes the right seam:
`MemoryStore::query(MemoryQuery)` with `contains` reserved for richer recall.
S1 uses that seam to improve retrieval quality behind the backend boundary while
preserving the trust, provenance, and reversibility invariants from
`memory-evolution-path.md`.

This is not an outcome claim. Semantic recall is default-off infrastructure; SI-4
paired eval remains responsible for proving whether it improves task results.

## Decision

Add `memory.recall = "exact" | "semantic" | "hybrid"` to the existing layered
memory config. The default is `exact`, which preserves the previous substring
and lexical-ranking behavior. `semantic` enables vector-backed
`MemoryQuery.contains`. `hybrid` is accepted as a named future mode but returns a
clear S2-reserved error instead of blending scores prematurely.

`truss-core::memory` owns a small synchronous `Embedder` trait plus a
deterministic feature-hashing embedder for hermetic tests. Real provider-backed
embedding lives outside core. The first real backend is an OpenAI-compatible
`/v1/embeddings` embedder in `truss-providers`; it reads credentials from
`memory.embedding_api_key_env`, defaulting to `OPENAI_API_KEY`, and can target
an alternate endpoint with `memory.embedding_base_url`.

SQLite stores vectors in a side table:

- `memory_id`
- `embedder_id`
- `dims`
- `vector_json`
- `created_at`

Semantic query first applies the same active-entry, `min_trust`, `kind`, `key`,
and `limit` constraints. It then embeds the query text and brute-force ranks
eligible vectors by cosine similarity. At current memory scale this avoids an ANN
dependency. sqlite-vec or another ANN index is the documented S2 scale swap.

Existing rows written before S1 have no vector. Truss therefore exposes an
explicit backfill path:

```bash
truss --config examples/harness.toml --db harness.db memory reindex
```

Semantic queries fail with a reindex-needed error if otherwise eligible entries
lack a vector for the current embedder id/dims, so old memory is not silently
missed.

## Invariant Preservation

Writes still enter `Unverified`; embedding persistence does not change trust.

`Verified` still means the reviewer panel signed off. No recall mode promotes,
relabels, or launders unverified content.

Every entry still carries `LedgerRef` provenance. Embeddings are derived indexes,
not provenance.

Reversibility is unchanged. Supersede and stale transitions still update memory
entries and remain ledger-traceable.

The default view remains verified-only. Semantic ranking composes with
`MemoryQuery::default().min_trust = Verified`; tests cover an unverified entry
that is semantically closer but excluded from the default trusted view.

## Consequences

Default runs have zero behavior change and do not require embedding credentials.

Semantic mode requires an embedder. The OpenAI-compatible backend uses
`OPENAI_API_KEY` by default and returns a configuration error when the env var is
missing. Tests use the deterministic embedder and never call the network.

Changing embedding model, dimensions, or backend changes the stored
`embedder_id`/`dims` and requires `truss memory reindex` before semantic recall
can safely use existing entries.
