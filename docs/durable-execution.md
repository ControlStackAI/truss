# Durable Execution + Verifiable Evidence Ledger — design

Status: DRAFT for review · 2026-06-14 · target: harness v0.3+

This is the architecture spec for making the harness a **durable, verifiable agent
runtime** — runs that survive process death and multi-day pauses and resume with
*exact* context, on a journal that is simultaneously the execution source-of-truth,
the audit trail, and a tamper-evident evidence ledger.

Decisions locked with Matt (2026-06-14):
- **C-grows-into-A** staging (hybrid first, full event-sourced replay as the
  destination — see §3).
- **Embedded SQLite** as the durable store (local-first, zero-ops, scales up).
- **Verifiable ledger is native**, not a separate layer: the journal is
  hash-chained and signed per entry (§9). Real blockchain is reserved for the
  future economy/settlement layer only (§12).

## 1. Goal & non-goals

**Goals**
- A run can crash (or be Ctrl-C'd, or the box can reboot) and resume exactly.
- A run can *deliberately suspend* for minutes-to-days awaiting an external event
  (human approval, long job) with the process fully exited, and resume exactly.
- "Never lose context" is achieved by *replaying recorded truth*, not by
  re-prompting from a reconstructed snapshot (the failure mode of the ADK-blog
  approach, which re-injects state and lets the model hallucinate the gap).
- Side effects are not duplicated on resume (no double-charge / double-send).
- The execution record is tamper-evident and signed — provenance for the trust
  layer comes from the same substrate that powers durability.

**Non-goals (now)**
- No external workflow server (Temporal/cluster) — it would be the opposite of
  "native." We implement a minimal embedded durable runtime.
- No on-chain anything in the core. Anchoring is an optional trait (§11).
- No infinite context. Durable ≠ unlimited; the model is fed a *projection* of the
  journal (§10).

## 2. The core inversion

Today the event stream (`src/events.rs` → `src/state.rs`) is **observational**:
the loop runs and emits events as a side effect. Durable execution **inverts**
this: the journal becomes the **source of truth for execution**, and the agent
loop becomes a deterministic reducer over it.

Every non-deterministic / side-effecting step — a model call, a tool call, a clock
read, a timer, a random draw — is an **activity**: it is executed once, its outcome
is appended to the journal (durably, fsync'd), and on any later replay the recorded
outcome is *returned without re-execution*. The orchestration *between* activities
is deterministic, so replay reproduces the identical sequence of activity requests
and matches them to recorded outcomes. (This is the Temporal/DBOS workflow-vs-
activity split, applied to an agent loop and embedded rather than served.)

The model being nondeterministic is fine: we never re-run a model call on replay,
we replay its *recorded output*. Determinism is required only of *our* orchestration
code, not the model or the tools.

## 3. C-grows-into-A staging

Both stages read the **same SQLite schema** (§4). The difference is only *how the
loop rehydrates*. No migration between them.

**Stage C (hybrid — ships first):**
- *Context continuity by snapshot.* After each step, persist the full message list
  + structured state. Resume = load the latest snapshot and re-enter the loop. No
  determinism constraint on orchestration yet.
- *Idempotency by side-effect ledger.* Every activity (tool/model call) is recorded
  with its result. Before executing, check the ledger; if the result exists, return
  it. This gives no-double-execute even though we're snapshot-restoring, not
  replaying.

**Stage A (full event-sourced replay — destination):**
- The loop replays the journal positionally from seq 0, returning recorded activity
  outcomes until the frontier, then runs live. Snapshots demote to *replay
  checkpoints* (a performance optimization: replay from the nearest checkpoint
  instead of seq 0). The side-effect ledger *is* the journal.

**Why C grows into A with no rewrite:** the journal is a superset. In C we already
write every activity result to it (for idempotency); we simply *also* keep
snapshots and restore from them. To become A, we start *replaying* the journal we
were already writing, and treat snapshots as optional. Same tables, same writes —
we flip the read path. The one discipline we adopt from day one to make this free:
**keep orchestration deterministic** (route clock/rand/IO through the durable
context even in C — see §8), so the C loop is already replay-faithful when we turn
A on.

## 4. SQLite schema

Embedded via `rusqlite` (bundled SQLite, no system dependency) behind a small
blocking DB actor task (`spawn_blocking`), or `sqlx` if we want async-native — to
decide at implementation (lean `rusqlite` for simplicity + zero async coupling).

```sql
-- One row per durable run.
CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,          -- session/run id
  status        TEXT NOT NULL,             -- running|suspended|completed|failed
  code_version  TEXT NOT NULL,             -- for replay compatibility (§13)
  model         TEXT,
  tenant        TEXT NOT NULL DEFAULT 'local',
  user          TEXT NOT NULL DEFAULT 'local',
  head_hash     BLOB,                      -- tip of the hash chain (§9)
  pubkey_id     TEXT,                      -- signing identity (§9)
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- The authoritative, append-only, hash-chained, signed log.
CREATE TABLE journal (
  session_id    TEXT NOT NULL,
  seq           INTEGER NOT NULL,          -- 0-based, contiguous
  kind          TEXT NOT NULL,             -- see §5
  payload       TEXT NOT NULL,             -- canonical JSON (or blob ref)
  prev_hash     BLOB NOT NULL,             -- entry_hash of seq-1 (or zero)
  entry_hash    BLOB NOT NULL,             -- H(prev_hash || kind || canon(payload) || seq)
  signature     BLOB NOT NULL,            -- Sign(sk, entry_hash)
  created_at    INTEGER NOT NULL,
  PRIMARY KEY (session_id, seq)
);

-- Optional restore/replay checkpoints (C: restore; A: replay shortcut).
CREATE TABLE snapshots (
  session_id    TEXT NOT NULL,
  at_seq        INTEGER NOT NULL,          -- journal seq this snapshot reflects
  messages      TEXT NOT NULL,             -- serialized Vec<Message> (or blob ref)
  state         TEXT NOT NULL,             -- structured key/value state
  PRIMARY KEY (session_id, at_seq)
);

-- Content-addressed blobs: large tool outputs, large payloads, redactable secrets.
CREATE TABLE blobs (
  hash          BLOB PRIMARY KEY,          -- sha256 of bytes
  bytes         BLOB,                      -- NULL after crypto-shred (§11 redaction)
  size          INTEGER NOT NULL,
  created_at    INTEGER NOT NULL
);

-- External wake sources.
CREATE TABLE signals (
  session_id    TEXT NOT NULL,
  name          TEXT NOT NULL,
  payload       TEXT,
  status        TEXT NOT NULL,             -- pending|delivered
  created_at    INTEGER NOT NULL,
  PRIMARY KEY (session_id, name, created_at)
);

CREATE TABLE timers (
  session_id    TEXT NOT NULL,
  seq           INTEGER NOT NULL,          -- journal seq that set the timer
  fire_at       INTEGER NOT NULL,
  status        TEXT NOT NULL,             -- armed|fired|cancelled
  PRIMARY KEY (session_id, seq)
);
```

The **durability boundary** is the single SQLite transaction that appends a
journal entry (+ updates `snapshots`/`sessions.head_hash`). It is atomic and
fsync'd. A crash before commit means the activity "didn't officially happen" and is
safely re-executed on resume; a crash after means it's memoized. External side
effects are therefore **at-least-once** unless the tool passes an idempotency key to
the downstream system — documented as the irreducible boundary (§8).

## 5. Journal entry kinds

Each entry is one completed activity or one control event:

- `run_started` — `{ prompt, system, model }`
- `model_result` — `{ request_digest, content[], stop_reason, usage }` (the recorded
  output of one model call)
- `tool_result` — `{ call_id, name, input, content|blob_ref, is_error }` (recorded
  output of one tool call; memoized on replay)
- `state_set` — `{ key, value }` (structured state, decoupled from transcript)
- `timer_set` — `{ fire_at }` / `timer_fired` — `{ }`
- `suspended` — `{ reason, await: { signal: name } | { timer: seq } }`
- `signal_received` — `{ name, payload }`
- `run_completed` — `{ final_text, usage }`
- `run_failed` — `{ error }`

## 6. Execution model

```text
resume(session):
  load session row; if absent -> new run
  rehydrate:
     C: load latest snapshot -> messages, state ; cursor = snapshot.at_seq
     A: replay journal from nearest checkpoint -> messages, state ; cursor = frontier
  loop:
     deterministic orchestration decides next activity
     for each activity (model call / tool call / now / sleep / random):
        if journal has a recorded outcome at this logical position -> return it
        else execute -> append (txn: journal entry + snapshot + head_hash) -> return
     on tool/loop signalling SUSPEND:
        write `suspended` entry; set session.status=suspended; return (process may exit)
     on terminal stop -> write run_completed; status=completed
```

In C, "recorded outcome at this logical position" is keyed by a per-session
monotonic activity counter persisted in state (so a re-entered loop that re-issues
the same call finds the prior result). In A it's keyed by journal `seq` (positional,
exact).

## 7. Suspend / resume / signal

- **Suspend:** a tool returns a `Suspend { await }` outcome (new tool result
  variant), or the loop hits a durable `sleep_until`. We append `suspended`, mark
  the session, and return cleanly. The process can exit / scale to zero.
- **Resume triggers (harness owns these natively — no OpenClaw):**
  - `truss resume <session>` (CLI) — cold resume on demand.
  - `truss signal <session> <name> [payload]` — deliver an external
    signal; appends `signal_received`, then resumes.
  - Durable **timers**: an optional long-lived `truss daemon` holds armed
    timers and fires them (resuming the owning session). Without the daemon, timers
    are checked on the next resume. Either way the timer is in SQLite, so nothing is
    lost if the daemon is down.
- A local HTTP endpoint (`signal`/`resume`) can wrap the same calls for webhook-
  style delivery — our own surface, not a dependency.

## 8. Idempotency, side effects & determinism

- **Internal memoization** (model/tool/clock/rand): exact, via the journal.
- **External effects** (a tool that emails, pays, mutates a remote system): the
  journal makes them *at-least-once*. Tools that touch the outside world SHOULD
  accept and forward an **idempotency key** (we derive a stable one per activity
  position) so the downstream system dedupes. Tools declare whether they're
  `pure` / `idempotent` / `effectful` in their def; effectful tools without a key
  get a loud warning in the ledger.
- **Determinism rules for A (adopt now in C so growth is free):** orchestration
  must not read wall-clock, randomness, env, or do ad-hoc IO. Use durable context
  accessors instead — `ctx.now()`, `ctx.sleep_until()`, `ctx.random()` — each a
  journaled activity. Model + tools are the only other nondeterminism, and both are
  journaled.

## 9. Verifiable evidence ledger (native)

Every journal entry is hash-chained and signed:

```text
entry_hash[0] = H( ZERO || kind || canon(payload) || 0 )
entry_hash[n] = H( entry_hash[n-1] || kind || canon(payload) || n )
signature[n]  = Sign(sk, entry_hash[n])
session.head_hash = entry_hash[last]
```

- **Hashing:** SHA-256 over a canonical (stable-key-ordered) encoding.
- **Signing:** ed25519. A harness identity keypair (per install); optionally a
  per-agent key so multi-agent runs are attributable. `pubkey_id` recorded on the
  session.
- **Verify:** `truss verify <session>` recomputes the chain and checks
  every signature against `pubkey_id`. Any edit, deletion, or reordering breaks the
  chain from that point — tamper-*evident*.
- **What this buys, without a blockchain:** tamper-evidence + non-repudiation +
  provenance. This is the Certificate-Transparency / git model: cryptographic
  verifiability is a *data-structure + signatures* property; consensus and tokens
  are a separate concern we don't need for single-operator trust.

## 10. Context projection (durable ≠ infinite)

The journal/messages grow without bound; the model's context does not. What we feed
the model each step is a **derived projection** over the journal, recomputed as a
pure function:

- **Head:** system prompt + (later) contract + pinned structured state.
- **Tail:** the most recent N turns verbatim.
- **Middle:** compacted/summarized older turns (summaries are themselves journaled
  activities, so they're durable and replay-faithful).

The full journal stays authoritative; the projection is disposable and
recomputable. This is exactly where the Context-Management Engine plugs in — as a
projection function, not a bolt-on. ADK-style structured-state injection
(`{current_step}` etc.) is the Head portion done right.

## 11. External anchoring & redaction

- **Anchoring trait** (optional, default off):
  ```rust,ignore
  trait Anchor { async fn anchor(&self, session_id: &str, head_hash: &[u8]) -> Result<AnchorReceipt>; }
  ```
  - `NoopAnchor` (default) — local signatures only.
  - `TransparencyLogAnchor` — publish the head/Merkle root to Sigstore/Rekor (public,
    checkable timestamp; no private data leaves the box).
  - `ChainAnchor` (future) — publish the root to an L2 *only* if a counterparty
    demands permissionless proof. Reserved; not core.
  **Rule: only ever publish a hash/root, never evidence data.** Work may be
  confidential; anchoring a salted root proves existence-at-time without disclosure.
- **Redaction / crypto-shred:** because A replays the journal, entries can't be
  deleted. Sensitive payloads are stored by reference in `blobs`; the journal holds
  only the blob *hash*. Deleting the blob bytes (set `bytes = NULL`) crypto-shreds
  the data while leaving the hash chain intact (replay degrades gracefully for that
  one value; chain still verifies). This is how we satisfy "delete the secret"
  without breaking durability or the ledger.

## 12. How the trust & economy layers plug in later

- **Verified delegation** (the north-star): a *contract* is the first journal entry
  after `run_started`; evidence (acceptance checks, no-stake review) are journaled
  activities; the *verdict* is a signed entry. The `head_hash` over a completed
  contract IS the portable, tamper-evident evidence bundle. Durable execution and
  verified delegation share one substrate — no second system.
- **Agent economy / settlement** (where real blockchain earns its place): an escrow
  smart contract can use a session's anchored `head_hash` + an accepted verdict as
  its release condition — an oracle attests the verdict, funds release. The ledger
  is the *bridge*; we build none of it now, but the design leaves the door open.

## 13. Versioning & replay compatibility

- `sessions.code_version` stamps the orchestration version that wrote the journal.
- On resume, if the current binary's version differs, we either (a) replay under a
  compatibility shim, or (b) in C, simply restore from snapshot (snapshot restore is
  version-tolerant because it doesn't re-derive — another reason C ships first).
- Activity *payloads* are versioned JSON; add fields, never repurpose them.
- A "patching" mechanism (à la Temporal `getVersion`) is the long-term answer for
  changing orchestration mid-flight; out of scope for v0.3.

## 14. Mapping to current code

- `src/ctx.rs` — add `session_id`; add durable accessors `now()/sleep_until()/
  random()` (journaled in A; thin in C). `Principal` already present.
- `src/state.rs` — evolve `RunLog` (JSONL) into a SQLite-backed `Journal` + `Ledger`
  + `SnapshotStore`. JSONL becomes an *export* of the journal, not the store.
- `src/agent.rs` — wrap model/tool calls through a `Durable` layer (memoize via
  journal; per-step transactional append of result + snapshot); recognize a
  `Suspend` outcome; rehydrate on resume.
- `src/provider.rs` / `src/tools` — tools gain an effect class (`pure|idempotent|
  effectful`) and may return `Suspend { await }`.
- New module `src/journal.rs` (store + hash-chain + signing) and `src/anchor.rs`
  (trait + Noop). New deps: `rusqlite` (bundled), `ed25519-dalek`, `sha2`.
- CLI: `resume`, `signal`, `verify`, and an optional `daemon`.

## 15. Milestones

1. **M1 — durable spike (Stage C):** SQLite journal + snapshot/restore; a
   long-running tool that suspends on a timer; process exits; `resume <session>`
   continues with exact context; tool memoization proven (no double-exec). *Kills or
   confirms the design cheaply.*
2. **M2 — ledger:** hash-chain + ed25519 signing per entry; `verify <session>`.
3. **M3 — signals & timers:** external `signal` suspend/resume; optional `daemon`
   for durable timers.
4. **M4 — anchoring + context projection:** `Anchor` trait + transparency-log
   anchor; Head/Tail/compacted-middle projection hook.
5. **M5 — Stage A:** positional journal replay; durable `now/sleep/random`;
   snapshots demote to checkpoints.

Then the verified-delegation/contract layer (§12) lands on top of this substrate.
