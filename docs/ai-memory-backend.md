# ai-memory Backend

Truss core includes an opt-in `AiMemoryStore` implementation of the governed
`MemoryStore` contract. It connects to an OS-selected ai-memory Unix socket
using broker protocol v1; requests contain no principal or caller-selected
identity.

The backend preserves Truss's memory invariants:

- writes enter `Unverified`;
- `Verified` and `Rejected` require reviewer verdicts;
- promotion, rejection, and stale transitions use content-hash compare-and-set;
- signed `LedgerRef` provenance remains attached to every record;
- queries default to verified-only and never return rejected memory;
- content-addressed IDs match `SqliteMemoryStore`.

Canonical Markdown is the durable source of truth. Truss's signed execution
journal remains local because it is the evidence chain referenced by memory,
not a duplicate memory database.

Select the backend explicitly in Truss configuration:

```toml
[memory]
store_backend = "ai_memory"
ai_memory_socket = "/run/user/1000/ai-memory.sock"
recall = "exact"
```

SQLite remains the default. Selecting `ai_memory` without an absolute socket
path fails closed, as does selecting semantic or hybrid recall, which the
broker does not yet support.

Until the broker exposes a durable global mutation cursor, `delta_since`
returns the full accessible projection with cursor zero. This is conservative
and correct, but less efficient than the SQLite implementation.

Broker `compare_set_many` now preserves Truss's cross-record supersede-by-key
semantics without a partial-read window: replacement creation and predecessor
linking commit together, promotion stales the verified predecessor, and
rejection restores it.
