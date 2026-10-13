# Native Memory Tools

Truss provides two default-off read-only native memory tools:

- `memory_query` searches L2 durable memory using the configured recall mode
  (`exact`, `semantic`, or `hybrid`).
- `memory_view` fetches one or more memory entries by id.

These tools let the model pull durable memory on demand instead of relying only
on run-start recall injection. They do not create, edit, promote, reject, or
stale memory entries.

## Enabling

Enable the tools with the CLI flag:

```bash
truss --durable --tools --memory-tools --config examples/harness.toml "continue the task"
```

Or in TOML:

```toml
[tools]
memory = true
```

Tool registration and policy are separate. If `policy.allow_tools` is set, allow
the exact tool names:

```toml
[policy]
allow_tools = ["read_file", "memory_query", "memory_view"]
```

The older singular `--memory-tool` flag still controls the governed
`memory_20250818` write binding. `--memory-tools` is read-only and does not open
a model-facing write or promote path.

## Trust Floor

Both tools default to:

```json
{ "min_trust": "verified" }
```

The trust floor is applied before ranking. With semantic and hybrid recall, the
store filters by trust, kind, and key before embedding similarity is computed or
sorted. That keeps unverified, rejected, and stale-below-floor entries out of the
candidate set.

Callers may explicitly widen the floor:

```json
{ "query": "migration", "min_trust": "unverified" }
```

Rejected memory is never surfaced by these tools.

## `memory_query`

Input:

```json
{
  "query": "rust borrow checker",
  "kind": "lesson",
  "key": "optional-key",
  "limit": 32,
  "min_trust": "verified",
  "revalidate": false
}
```

`query` and `contains` are aliases. `kind` may be `goal`, `decision`, `fact`,
`lesson`, `playbook`, `artifact`, `scratch`, or `context_summary`.

Output includes:

- `recall_mode`: the resolved memory recall backend.
- `min_trust`: the trust floor used.
- `entries`: typed memory entries with `id`, `kind`, `key`, `body`, `trust`,
  `ledger_ref`, and full `provenance`.

`ledger_ref` is the entry's producing `LedgerRef`; `provenance` also includes
sources and verification verdict references when present.

## `memory_view`

Input:

```json
{
  "id": "memory-id",
  "ids": ["another-memory-id"],
  "min_trust": "verified",
  "revalidate": false
}
```

Output returns matching entries plus:

- `filtered`: ids that exist but are below the trust floor.
- `missing`: ids not found in the memory store.

## Ledger And Replay

In durable and contract runs, both tools record signed `memory_tool_call` entries
with:

- tool name,
- deterministic memo key,
- input SHA-256,
- result SHA-256,
- error flag,
- optional producing ledger reference.

Successful and failed results are written to the tool memo table in the same
transaction as the signed journal entry. Resume/replay returns the recorded
result and verifies that the memo still matches a signed ledger entry, instead
of querying a mutated memory store.

To request a fresh observation, pass `revalidate = true`. Revalidation bypasses
the existing memo, queries the current store, and records a new signed
`memory_tool_call` entry plus replacement memo for the same logical input.

If a memo is changed without a matching signed ledger entry, replay fails with:

```text
memory memo missing matching signed provenance
```

That failure prevents a replayed verdict from consuming unsigned memory output.
