# ADR 0007: Layered Injection + Identity Persona

Status: Accepted

## Context

M1-M3 gave Truss a governed memory store, the native `memory_20250818` tool, and
trust-tier promotion. M4 changes how that state reaches the model: stop dumping
all identity, memory, and task notes as one volatile markdown blob; assemble the
prompt by lifetime tier and align the slow tiers to the provider prompt cache.

Matt's design directive for M4 is that policy choices remain configurable so real
work can A/B evaluate alternatives. Even knobs not exposed in a friendly UI are
typed config with layered resolution: global `[memory]` / `[identity]`, then
`[agent.memory]`, then contract `[memory]`.

## Decision

Truss assembles context in this order:

1. tools
2. system blocks: L0 core identity, L0 self identity, L1 principal
3. messages: L3 goal ledger, projected history, L2 memory deltas, L4 scratch

The last L0/L1 system block carries `cache_control = { type = "ephemeral" }`
for providers that support prompt caching. The cache-prefix key is computed only
from tools plus L0/L1 system content, so L3 goal changes and L4 scratch changes
do not invalidate the stable prefix.

Persona is first-class:

- `PersonaCore` is operator-signed with `identity::Signer`, verified on load, and
  refused if the signature no longer matches.
- `PersonaSelf` contains amendable voice/working identity and version metadata.
- `Persona::render()` emits L0 core before L0 self.

Identity amendments are deliberate:

- `amend_identity` is a dedicated signed tool.
- Amendments targeting `core` are denied.
- `IdentityPolicy::propose` buffers for approval.
- `IdentityPolicy::self_amend` buffers `CommitAtBoundary`, applies on the next
  session build, and records a reversible journal event.
- The amendment is verdict-checked against `core` before commit.

L3 goal ledger pins the active contract intent and definition of done. It is
injected on the configured cadence and remains outside compaction.

Resume/re-pin uses `MemoryStore::delta_since(cursor)` for L2 rehydration. The
model sees only memory changes after its cursor, never the whole store unless the
cursor is zero.

## Configurable Knobs

| Knob | Default | Values | Layering |
| --- | --- | --- | --- |
| `verification` | `lazy` | `off`, `lazy`, `eager`, `tiered` | global -> agent -> contract |
| `identity` | `propose` | `locked`, `propose`, `self_amend` | global -> agent -> contract |
| `scope` | `per-agent` | `per-agent`, `per-project`, `shared` | global -> agent -> contract |
| `conflict` | `verified-wins` | `verified-wins`, `latest-wins`, `panel-arbitrated`, `keep-both-flagged` | global -> agent -> contract |
| `goal_repin_every` | `8` | positive integer | global -> agent -> contract |
| `projection.keep_tail` | `0` | integer, `0` disables projection | global -> agent -> contract |
| `projection.compaction_exempt_tiers` | `l0_core`, `l0_self`, `l1_principal`, `l3_goal` | any `ContextTier` list | global -> agent -> contract |
| `salience` | `off` | `off`, `harness-recency`, `model-scored`, `hybrid` | global -> agent -> contract |

Salience is an M5 seam only. M4 parses and resolves the policy but does not
evict or rank memory differently based on it.

## Consequences

The default path stays conservative: no projection unless configured, memory is
per-agent, conflicts prefer verified entries, identity self-amendment still needs
approval, and salience does nothing. When M4 surfaces the layered context, the
provider can reuse the L0/L1 cached prefix across long runs while the L3 goal
ledger keeps the active intent fresh across compaction.
