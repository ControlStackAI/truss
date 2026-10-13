# Truss Memory & Identity Architecture

Status: DRAFT v0 spec, 2026-06-16. Build target — next workstream after the
OpenAI subscription arc (v0.11.0). The data model and phasing should be
pressure-tested before writing Rust.

Related docs: [`architecture.md`](architecture.md) (Provider/Executor/loop),
[`durable-execution.md`](durable-execution.md) (ledger, resumability),
and [`examples/contract.md`](examples/contract.md) (contracts and executable
checks). This doc extends the **same trust machinery** (ledger + N-reviewer
panel + signed verdict) from *what the agent produces* to *what the agent
believes and remembers*.

---

## 0. Why this exists (the empirical finding)

We ran Fable 5 locally in two harnesses (native Claude Code CLI and
OpenClaw-driven Claude Code) and inspected the artifacts. Findings:

- **The client-side memory tool (`memory_20250818`) is real and documented** —
  the model issues `view`/`create`/`str_replace`/`insert`/`delete`/`rename`
  calls against a `/memories` directory the *harness* backs. But **neither
  harness enabled it.** Capability ≠ activation.
- A native **470-turn local test run** used *zero* memory-tool
  calls, had *no* `/memories` dir, and every `context_management` field resolved
  to `{"applied_edits":[]}` (present in the request envelope, never fired).
- What Fable *did* reach for unprompted on that long-horizon task: a **`GOAL.md`**
  (7.7KB) it consulted and edited — i.e. it hand-rolled a goal ledger because the
  harness gave it nowhere structured to put durable state.
- Current harnesses (OpenClaw included) inject identity + memory as **one flat
  static markdown blob every call** — `AGENTS.md` + `SOUL.md` + `IDENTITY.md` +
  `USER.md` + a 37KB `MEMORY.md` — undifferentiated, unverified, cache-hostile,
  push-only.

**Fable-specific disposition (from Anthropic's migration guidance):** the memory
tool is a 4.x-family feature, *not* Fable-exclusive — but Fable 5 is documented as
**notably better at using a memory surface**: "performs notably better when it can
write learnings somewhere for future reference… tell it where, tell it to consult
that file in future sessions, and give it a format," and "good at updating skills
on the fly from what it learns mid-task." So memory is the capability with the
**highest return on the leading model**. Under-building it leaves Fable/Mythos's
best trait on the table.

**Thesis:** identity and memory are the same architecture viewed at different
*lifetimes*. Truss should model the entire injected context as a **layered,
lifetime-tiered, cache-aligned stack** with a **typed, trust-tiered,
provenance-tracked** memory store the model reaches via the native memory tool —
not a markdown dump.

---

## 1. Design principles

1. **Push the frozen essentials, pull the rest.** Identity and pinned goals are
   pushed (small, stable). Durable memory is *pulled on demand* by the model via
   the memory tool — stop shoving 37KB every turn.
2. **Separate by lifetime, align to the cache.** The Anthropic prompt cache is a
   prefix match (`tools → system → messages`). Each layer's churn rate must match
   its position so a memory write never invalidates the identity prefix.
3. **No unverified belief is "trusted."** A memory write is *quarantined* until a
   verification pass promotes it — reusing the N-reviewer panel + signed verdict
   that already gate contract completion.
4. **Every memory carries provenance.** Each entry links to the ledger event /
   tool result that produced it. "Why do you believe X?" is always answerable.
5. **The model curates; the harness governs.** The model decides *what* to write
   (it's good at this — especially Fable). The harness owns *trust, lifetime,
   eviction, and injection policy*.
6. **Substrate-agnostic, like Provider/Executor.** The memory tool is the
   model-facing interface; the *backing store* is a Truss trait so we can swap
   flat-file → SQLite → vector index without touching the agent loop.

---

## 2. The layered injection model

Replace "inject all the markdown" with five lifetime tiers. Each has its own
churn rate, cache position, trust level, and injection rule.

| Layer | Content | Lifetime | Cache position | Injection |
|---|---|---|---|---|
| **L0 Identity** | persona, values, voice, operating boundaries | ~never | front of cached `system` prefix | push, signed, frozen |
| **L1 Principal** | who the user is, prefs, standing constraints | per-user, slow | after L0, still cached | push, per-user cache key |
| **L2 Durable memory** | verified facts, lessons, decisions | grows | **not pushed** | **pull** via memory tool |
| **L3 Goal ledger** | active contract intent + DoD, north-star | per-task | head of `messages`, re-pinned every N turns | push, never compacted |
| **L4 Working scratch** | in-run notes, tool-result digests | ephemeral | tail of `messages` | model-written, compacted server-side |

Key consequences:

- **Identity stops being a markdown file in the pile.** L0 is a first-class,
  signed, frozen artifact at the very front of the prefix. It never sits next to
  volatile notes, so memory churn never busts the identity cache. This is the
  "agent identity, done better" Matt called for.
- **L0 splits into `core` + `self`** (load-bearing — see §3.5). `core` =
  operator-signed boundaries, safety, mandate; never self-amendable. `self` =
  voice, learned preferences, working persona; self-amendable under policy. The
  agent can evolve its personality without touching its own guardrails.
- **L2 is addressable, not ambient.** The model queries durable memory through the
  memory tool when relevant, instead of reading a 37KB wall it mostly ignores.
  Token spend drops; attention sharpens; the cache prefix stays stable.
- **L3 is the `GOAL.md` Fable already wanted** — promoted to first-class, pinned,
  and re-injected on a cadence so the goal never drifts across a long run. This is
  the answer to the original "how do we keep the end goal from getting lost"
  question, now mechanized.

### 2.1 Cache alignment (concrete)

```text
[ tools ]                      ← memory tool + task tools (stable set; don't churn mid-run)
[ system ]
   L0 identity (frozen, signed)        ┐ cached prefix — invalidated ~never
   L1 principal (per-user)             ┘
[ messages ]
   L3 goal ledger (re-pinned every N turns)   ← small, deliberate re-inject
   …conversation / tool calls…
   L2 pulls land here as tool_results          ← model-initiated, on demand
   L4 scratch (tail)                            ← compaction edits here
```

Rule: **nothing above the goal ledger churns within a run.** Identity, principal,
and the tool set are frozen for the session. All volatility lives in `messages`.

---

## 3. The memory store (L2) — typed, trust-tiered, provenance-tracked

### 3.1 Entry types (typed, not flat)

| Type | Meaning | Mutability | Compaction |
|---|---|---|---|
| `goal` | north-star DoD / intent for a contract | append + supersede | **never** |
| `decision` | a choice made + rationale | append-only (immutable log) | never |
| `fact` | a durable learned fact about the world/codebase | supersede-by-key | salience-scored |
| `lesson` | a correction / confirmed approach + why | supersede-by-key | salience-scored |
| `artifact` | a verified output + where it lives | append | by reference |
| `scratch` | ephemeral working note | free | aggressive |

(Maps onto the SOUL/feedback/project/reference taxonomy ALINA already uses for
durable memory — generalized and typed for the harness.)

### 3.2 Trust tiers (the novel part)

Every entry has a trust state. Writes do **not** enter trusted memory directly.

```text
unverified ──(verification pass)──▶ verified ──(contradiction)──▶ stale/retired
     │                                  │
     └────────(refuted)────────▶ rejected
```

- **unverified** — just written by the model. Visible to the *writing* run as
  scratch, but flagged; not promoted to L2 durable, not surfaced to future runs.
- **verified** — promoted after a verification pass confirms it. Reuses the
  **N-reviewer panel + signed verdict** from the contract system: a claim like
  "the build passes" is checked (machine `check`) or judged (no-stake `review`)
  exactly as a DoD clause is. Only `PASS` promotes.
- **rejected** — refuted by the panel. Kept (audit) but never surfaced.
- **stale** — was verified, later contradicted by a newer verified entry or a
  failed re-check. Demoted, not deleted.

This is what kills the classic failure: a model hallucinates progress ("auth
fixed"), writes it to memory, and a later run builds on the lie. In Truss the lie
sits in `unverified` until evidence promotes it — and the same machinery that
guards contract completion guards belief.

### 3.3 Provenance

Every entry links to the **ledger** (see `durable-execution.md`): the
`ledger_event_id` (tool call / check / verdict) that produced or verified it.
Querying memory can always return the evidence chain. Verified entries carry the
**signed verdict** that promoted them — same signature scheme as contract
verdicts. Memory becomes auditable, not vibes.

### 3.4 Rust-shaped data model (strawman)

```rust,ignore
// crate: truss-memory
pub struct MemoryEntry {
    pub id: MemoryId,              // content-addressed
    pub kind: EntryKind,          // Goal | Decision | Fact | Lesson | Artifact | Scratch
    pub key: Option<String>,      // supersede key for Fact/Lesson
    pub body: String,             // the content (model-authored)
    pub trust: Trust,             // Unverified | Verified | Rejected | Stale
    pub provenance: Provenance,   // ledger_event_id(s), produced_by, verified_by
    pub salience: f32,            // relevance score for eviction/ranking
    pub created: Timestamp,       // passed in (no Date::now in pure core)
    pub superseded_by: Option<MemoryId>,
}

pub struct Provenance {
    pub produced_by: LedgerEventId,
    pub verified_by: Option<SignedVerdict>,   // reuse contract verdict type
    pub sources: Vec<LedgerEventId>,
}

pub trait MemoryStore {           // substrate-agnostic, like Provider/Executor
    fn write(&mut self, draft: MemoryDraft) -> MemoryId;        // lands Unverified
    fn promote(&mut self, id: MemoryId, verdict: SignedVerdict);
    fn query(&self, q: MemoryQuery) -> Vec<MemoryEntry>;        // by kind/key/salience/trust
    fn delta_since(&self, cursor: MemoryCursor) -> MemoryDelta; // for L2 delta rehydration
}
```

v0 backing impl: flat content-addressed files + an index (mirrors the
durable-execution ledger on disk). Later: SQLite, then optional vector index for
salience/semantic recall — swapped behind `MemoryStore` with no agent-loop change.

---

### 3.5 Policy knobs (user-selectable, layered defaults)

Both verification cost and identity self-amendment are **settings**, not
hard-coded. Granularity: global default → per-agent → per-contract override
(contracts already carry config blocks).

**`VerificationPolicy`** — when/whether `Unverified → Verified` promotion runs:

| Mode | Behavior | Cost | Use |
|---|---|---|---|
| `off` | writes stay `Unverified`, usable in-run, never promoted/surfaced to future runs as trusted | none | throwaway / solo speed |
| `lazy` *(default)* | verify on first **recall**, not on write — pay only for memory actually re-used | amortized | most work |
| `eager` | sync N-reviewer pass before promotion on write | high | high-stakes / shared memory |
| `tiered` | cheap heuristic gate always; full panel only for `fact`/`decision` or flagged writes | medium | balanced |

**The invariant that makes this safe to expose:** `Verified` *always* means "a
panel signed off." The knob changes *when* verification runs and *what gets
verified* — **never what the label asserts.** `off` doesn't relabel junk as
trusted; it leaves everything `Unverified` and *honestly labeled*. Every consumer
of memory branches on trust tier regardless of mode (it should anyway). So we lose
almost nothing by making this selectable — the only real cost is test surface and
keeping defaults honest. Don't let `off` silently surface unverified content to
future runs as if trusted; that's the one footgun to guard.

**M4 adds these layered knobs** (all global → agent → contract, documented in
ADR 0007 and `examples/harness.toml`):

| Knob | Default | Values |
|---|---|---|
| `scope` | `per-agent` | `per-agent`, `per-project`, `shared` |
| `conflict` | `verified-wins` | `verified-wins`, `latest-wins`, `panel-arbitrated`, `keep-both-flagged` |
| `goal_repin_every` | `8` | positive integer |
| `projection.keep_tail` | `0` (legacy no-projection) | integer |
| `projection.compaction_exempt_tiers` | `l0_core`, `l0_self`, `l1_principal`, `l3_goal` | tier list |
| `salience` | `off` | `off`, `harness-recency`, `model-scored`, `hybrid` (M5 seam only) |

**`IdentityPolicy`** — who can amend L0 `self`:

| Mode | Behavior |
|---|---|
| `locked` | operator-signed only; agent may *propose*, not commit |
| `propose` *(default)* | agent drafts an amendment → verdict gate / operator approval → commit |
| `self_amend` | agent commits directly (what Matt does in OpenClaw today) |

`self_amend` is safe to expose **only because of four structural guards that hold
in every mode** — these are the "what we'd lose" made into design, not bans:

1. **`core` is never amendable.** Self-amend touches `self` only. Safety/boundary
   erosion is structurally impossible — the dangerous half of identity is locked.
2. **Amendments commit at run boundaries, not mid-run.** L0 is the frozen cache
   prefix; a mid-run rewrite busts the most expensive cache and breaks the
   "identity never churns" guarantee. Buffer the change, apply at session start.
3. **Amendment is a deliberate signed tool call**, provenance-logged to the
   ledger — never inducible from untrusted/injected content. Even in `self_amend`
   the proposed change is verdict-checked against `core` before commit (reuses the
   N-reviewer machinery — an identity edit is just a high-stakes memory write, so
   it *also* respects `VerificationPolicy`).
4. **Every identity change is a reversible ledger event** — diff + rollback across
   all modes, non-negotiable.

With those four, `self_amend` loses nothing we care about: the agent evolves its
voice/persona freely, can't rewrite its guardrails, can't be hijacked into doing
so, and every change is auditable and reversible. Without them, `self_amend` risks
value drift, cache thrash, and a prompt-injection takeover surface — so the guards
ship with the knob, not as a later hardening pass.

**The two knobs unify:** an identity amendment *is* a high-stakes memory write, so
it flows through the same trust-tier + N-reviewer + provenance machinery as any
`fact`/`decision`. One mechanism, two surfaces.

## 4. Model-facing interface — the native memory tool

Expose L2 to the model through the **documented `memory_20250818` tool** (so we
ride Fable's trained behavior, not a bespoke protocol). Truss intercepts the tool
calls and maps them onto `MemoryStore`:

- `create`/`str_replace`/`insert` → `MemoryStore::write` (lands `Unverified`),
  with the harness inferring `kind` from the path (`/memories/goal/…`,
  `/memories/decisions/…`, `/memories/lessons/…`).
- `view` → `MemoryStore::query` filtered to **`Verified` only** by default (the
  model pulls trusted memory on demand — the "pull, not push" of L2).
- A verification pass (sync or deferred) runs the N-reviewer panel on new
  `Unverified` writes and promotes the survivors.

**Delta rehydration:** on session resume / re-pin, inject only
`MemoryStore::delta_since(cursor)` — what changed since the model last saw L2 —
not the whole store.

This means the agent loop doesn't learn a new memory API: it uses the tool
Anthropic already trained Fable on, and Truss provides the trustworthy substrate
underneath.

---

## 5. Fable-5 / Mythos tuning (optimize for the leading model)

Per the migration guidance, when the active provider is Fable-5/Mythos:

- **Give it the memory surface and tell it the format.** Inject a short L0/L3
  instruction: where memory lives, when to consult it, the one-lesson-per-entry +
  one-line-summary format. (Anthropic shows this materially improves Fable's
  memory use.)
- **Let it update lessons mid-task.** Don't gate `write`; gate *promotion*. Fable
  is good at on-the-fly curation — capture it, verify it lazily.
- **Async sub-agent memory.** Long-lived sub-agents keep their own L4 + shared L2;
  they communicate asynchronously with the orchestrator (Fable is documented as
  reliable at this) rather than spawn-and-block. Shared L2 is the substrate.
- **Don't over-prescribe.** Fable degrades with heavy step-by-step scaffolding —
  state the memory *goal and format*, not a procedure.

Provider-agnostic fallback: models without the memory tool get an L2 surfaced as a
synthetic `read_memory`/`write_memory` tool pair over the same `MemoryStore`.

---

## 6. Build phasing

- **M1 — Store + provenance.** `truss-memory` crate: `MemoryStore` trait, flat-file
  impl, typed entries, provenance into the existing ledger. No model yet; unit
  tests. (Mirrors how `truss-core` ledger was built.)
- **M2 — Memory tool binding.** Intercept `memory_20250818` in the agent loop, map
  to `MemoryStore`, path-based `kind` inference, `view` returns Verified-only.
  Hermetic tests with a fake provider emitting tool calls.
- **M3 — Trust tiers + promotion.** Wire the N-reviewer panel + signed verdict to
  promote `Unverified → Verified`; contradiction → `Stale`. Reuse contract verdict
  types end-to-end.
- **M4 — Layered injection.** L0–L4 assembly with cache-aligned placement; goal
  ledger re-pin cadence; delta rehydration. Identity (L0) becomes a signed frozen
  artifact.
- **M5 — Fable tuning + salience.** Format-prompt injection per provider; salience
  scoring + eviction; optional vector backing behind `MemoryStore`.

Each milestone is independently testable and lands behind the same
feature-flag/ADR discipline as the provider arc.

---

## 7. Open questions (pressure-test before M1)

- ~~**Verification cost.**~~ **RESOLVED → §3.5 `VerificationPolicy`** (`off` /
  `lazy` *(default)* / `eager` / `tiered`, user-selectable, layered defaults).
- ~~**Who writes L0 identity, and can the agent self-amend?**~~ **RESOLVED → §3.5
  `IdentityPolicy`** (`locked` / `propose` *(default)* / `self_amend`) + the L0
  `core`/`self` split in §2. Self-amend is exposable because of four structural
  guards (core locked, run-boundary commit, signed deliberate tool, reversible
  ledger event).
- **Salience / eviction policy.** M4 adds a default-off `SaliencePolicy` seam;
  scoring and eviction remain M5.
- ~~**Cross-run vs cross-agent scope.**~~ **RESOLVED in M4 → configurable
  `scope`** (`per-agent` default, `per-project`, `shared`).
- ~~**Conflict resolution.**~~ **RESOLVED in M4 → configurable `conflict`**
  (`verified-wins` default, `latest-wins`, `panel-arbitrated`,
  `keep-both-flagged`).
- **Identity vs memory leakage.** Keep L0/L1 out of L2 so persona can't be
  silently rewritten by a memory write (prompt-injection surface).
```
```

---

*Provenance of this doc: design discussion on 2026-06-16, grounded in local
inspection of a 470-turn Fable 5 run and the Anthropic API documentation for
memory tools and context editing.*
