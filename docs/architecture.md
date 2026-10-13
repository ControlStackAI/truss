# Truss — Architecture & How It Differs

> **Thesis:** generation isn't the bottleneck anymore — *trust* is. Truss is a
> durable, verifiable agent runtime. You delegate work as a **contract** and review
> a **signed verdict backed by independent evidence** — not a transcript.

This document is the map. Each diagram describes how a part of the harness is
*intended* to work; the implementation is held to match it.

---

## 1. The big picture — contract in, signed verdict out

Everything else in Truss exists to make this one path trustworthy.

```mermaid
flowchart TD
    H[Human] -->|"contract.toml<br/>(intent + definition-of-done<br/>+ budget + constraints)"| RC[run_contract]

    subgraph EXEC["1 · Execute (durable, signed)"]
        RC --> EX[Executor agent<br/>does the work]
        EX <-->|tool calls| TOOLS[Tools<br/>read/write/bash]
        EX -->|every step| J[(Journal<br/>hash-chained + ed25519)]
    end

    subgraph VERIFY["2 · Verify (independent evidence)"]
        J --> CK["check clauses<br/>harness runs shell cmds<br/>executor never controlled"]
        J --> RV["review clauses<br/>N-reviewer adversarial panel"]
    end

    CK --> VD{{Verdict<br/>PASS / FAIL / UNVERIFIABLE}}
    RV --> VD
    VD -->|signed journal entry| J
    J -->|head hash = portable<br/>evidence bundle| ANCHOR[(Transparency log<br/>only hashes leave the box)]

    VD -->|"proof, not transcript"| H

    style VD fill:#1f6feb,color:#fff
    style J fill:#2d2d2d,color:#fff
    style ANCHOR fill:#2d2d2d,color:#fff
```

**The invariant:** the executor *never grades its own homework*. Checks run in an
environment it didn't control; reviewers are separate model/contexts prompted to
refute. The human sees a verdict + evidence, not a wall of tool calls.

---

## 2. The harness loop — one `Ctx` threads every concern

A provider API is stateless: one request in, one response out. The *harness* is the
loop around it. Truss threads a single `Ctx` handle into every model and tool call,
so cancellation, budget, policy, and audit all flow through one place — adding a
cross-cutting concern later puts a field on `Ctx`, it never re-breaks signatures.

```mermaid
flowchart LR
    P[Prompt / contract] --> L{{Agent loop}}
    L -->|"history + request"| PROV[Provider.complete<br/>or complete_stream]
    PROV -->|text + tool calls| L
    L -->|"tool call?"| T[ToolRegistry.call]
    T -->|result appended| L
    L -->|"done?"| OUT[Final text]

    CTX[["Ctx (one handle)"]]
    CTX -.cancel.-> PROV
    CTX -.budget meter.-> PROV
    CTX -.policy / scopes.-> T
    CTX -.audit events.-> L

    style CTX fill:#1f6feb,color:#fff
    style L fill:#2d2d2d,color:#fff
```

- **Cancellation** — Ctrl-C aborts the in-flight model call *and* any running child
  process; the partial event log is flushed.
- **Budget** — hard ceilings on USD / tokens / steps, checked before each call.
- **Policy** — write-scope, read-scope, network mode, tool allowlist, forbidden
  substrings — enforced at the tool boundary, not by trusting the model.

---

## 3. Durable execution — suspend, crash, resume, replay

A contract run is event-sourced. Each step is a journal entry; the agent state is
rebuilt by replaying entries (positional replay). This is what lets a run survive a
crash, suspend on an external await (timer / signal), and resume exactly where it
left off — without re-doing or double-charging completed work.

```mermaid
stateDiagram-v2
    [*] --> Running
    Running --> Running: step → append journal entry<br/>(replayable)
    Running --> Suspended: await timer / signal
    Suspended --> Running: signal delivered /<br/>timer elapsed (daemon resumes)
    Running --> Crashed: process dies
    Crashed --> Running: restart → replay journal<br/>to last good position
    Running --> Completed: final text
    Completed --> [*]

    note right of Suspended
        State lives in SQLite, not RAM.
        Resume is positional replay:
        completed steps return cached
        results instantly; only the
        first unfinished step re-runs.
    end note
```

**Why it matters:** most harnesses keep the loop in memory — kill the process and
the run is gone. Truss treats a run like a database transaction log. The same
mechanism powers human-in-the-loop pauses and long-horizon (hours/days) work.

---

## 4. The verifiable ledger — hash chain → sign → anchor

Every journal entry is content-addressed and chained to its predecessor, then the
head is signed. Tampering with any past entry breaks the chain. Periodically the
head hash is anchored to an external append-only transparency log — so even the
machine that produced the run can't silently rewrite history.

```mermaid
flowchart LR
    E0["entry₀<br/>prev: ∅"] --> E1["entry₁<br/>prev: H₀"]
    E1 --> E2["entry₂<br/>prev: H₁"]
    E2 --> EH["head<br/>prev: H₂"]
    EH -->|ed25519 sign| SIG["signature<br/>(identity key)"]
    EH -->|salted hash only| ANCH[(Transparency log<br/>append-only, chained)]

    subgraph BOX["inside the box (private — NDA-safe)"]
        E0
        E1
        E2
        EH
        SIG
    end

    style EH fill:#1f6feb,color:#fff
    style ANCH fill:#2d2d2d,color:#fff
    style BOX fill:#0d1117,color:#fff,stroke:#30363d
```

**Privacy invariant:** *only salted hashes leave the box.* Evidence content (which
may be NDA / client work) is never put on-chain or in the anchor log. Anchoring
proves *when* a head existed, not *what* it contained. `--verify` re-walks the chain
and checks the signature; `--verify-anchor-log` checks the anchor log's own chain.

---

## 5. N-reviewer adversarial verification

A single reviewer is a single point of failure — a lenient model waves bad work
through. Truss convenes a **panel of N independent reviewers** for each `review`
clause. They run in parallel, each assigned a different *lens*, each prompted to
refute. A strict majority must say "met" or the clause fails. Unverifiable votes
count as not-met (default-to-fail). All-unverifiable → the clause is Unverifiable.

```mermaid
flowchart TD
    WORK[Executor summary<br/>+ reviewer_context files] --> PANEL{{Reviewer panel<br/>run in parallel}}

    PANEL --> R1["Reviewer 1 · lens:<br/>does it satisfy intent?"]
    PANEL --> R2["Reviewer 2 · lens:<br/>completeness — anything missing?"]
    PANEL --> R3["Reviewer 3 · lens:<br/>correctness — subtle errors?"]
    PANEL --> RN["Reviewer N · lens:<br/>shortcuts — corners cut?"]

    R1 --> V{Strict majority<br/>met · 2 &gt; N?}
    R2 --> V
    R3 --> V
    RN --> V

    V -->|yes| PASS[✓ Pass]
    V -->|no| FAIL[✗ Fail]
    V -->|all unverifiable| UNV[? Unverifiable]

    PASS --> EV["evidence:<br/>'panel: K/N reviewers say met'<br/>+ each seat's reasoning"]
    FAIL --> EV
    UNV --> EV

    style PANEL fill:#1f6feb,color:#fff
    style V fill:#2d2d2d,color:#fff
```

Lenses are assigned round-robin, so even a panel of *one model* gets perspective
diversity. Set the size with `reviewers = N` in the contract or `--reviewers N`.
(Diverse *models* per seat is a planned enhancement — the panel is already a
`Vec<Arc<dyn Provider>>`.)

---

## 6. How Truss differs from other harnesses

Most harnesses optimize **generation** — get the model to produce the artifact.
Truss optimizes **trust** — let you accept the artifact without re-reading the work.

```mermaid
flowchart LR
    subgraph TYP["Typical harness (Claude CLI / OpenClaw / Hermes)"]
        direction TB
        TP[Prompt] --> TL[Agent loop<br/>in memory] --> TT[Transcript]
        TT --> TH[Human reads<br/>the whole transcript<br/>to decide trust]
    end

    subgraph TRUSS["Truss"]
        direction TB
        CP[Contract] --> CL[Durable, signed loop] --> CE[Independent evidence<br/>+ N-reviewer panel]
        CE --> CV[Signed verdict<br/>+ evidence bundle]
        CV --> CH[Human reviews<br/>the proof]
    end

    style TH fill:#6e2d2d,color:#fff
    style CH fill:#1f6feb,color:#fff
    style CV fill:#1f6feb,color:#fff
```

| Dimension | Typical harness | **Truss** |
|---|---|---|
| Unit of work | prompt | **contract** (intent + checkable done + budget + constraints) |
| What you review | transcript | **signed verdict + independent evidence** |
| Who verifies | the executor (self-reported "done") | **harness-run checks + N-reviewer adversarial panel** |
| Run state | in-memory; dies with the process | **durable journal (SQLite); suspend / crash / resume** |
| Audit | logs you trust | **hash-chained, ed25519-signed ledger; externally anchored** |
| Privacy | n/a | **only salted hashes leave the box (NDA-safe)** |
| Provider lock-in | usually one vendor | **`Provider` trait; core is vendor-free, providers are plugins** |

---

## 7. Crate layout — small, vendor-free core

The moat is in the core; vendors are swappable plugins. The core has *no* vendor
code and *no* `reqwest` — it depends only on the `Provider` trait.

```mermaid
flowchart TD
    CLI[truss-cli<br/>flags, config, render] --> CORE
    PROV[truss-providers<br/>anthropic · openai · local<br/>SSE streaming, auth] -->|implements<br/>Provider trait| CORE

    subgraph CORE["truss-core (vendor-free)"]
        AG[agent loop]
        DUR[durable execution]
        DEL[delegation / contracts]
        LED[ledger: store · identity · anchor]
        CTX2[Ctx · policy · budget]
        PT[[Provider trait]]
    end

    style CORE fill:#0d1117,color:#fff,stroke:#1f6feb
    style PT fill:#1f6feb,color:#fff
```

See [`adr/0001-plugin-architecture.md`](adr/0001-plugin-architecture.md) for why the
core stays small and how providers plug in, and
[`auth-strategy.md`](auth-strategy.md) for the provider-agnostic auth model
(API key / OAuth subscription / CLI subprocess).
