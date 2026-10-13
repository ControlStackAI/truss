# Truss — live demo runbook (AI Clubhouse Meetup, Jun 28, LA)

Audience: founders, builders, creatives, enthusiasts. Keep it visual and fast.
Total target: **5–7 minutes**. A 30-sec cold open + four beats. Every beat has a fallback.

> **Golden rule:** the most reliable beats (verify, tamper, suspend/resume) need
> **no model at all**. Pre-build the demo DB before you walk on stage so those
> always work even with no wifi.

---

## Prep (before the talk)

```bash
cd /path/to/truss
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

**Executor model — pick one, in order of demo-safety:**
1. **Local (best story, offline):** Ollama running `llama3.1` or similar.
   `--provider openai --base-url http://localhost:11434/v1 --model llama3.1`
2. **Anthropic (cleanest output):** `export ANTHROPIC_API_KEY=…` (needs credits).
3. **Pre-recorded:** play `presentation/assets/demo.cast` / screen capture.

Set a shell function so every command uses the demo database:
```bash
truss() { command truss --db demo.db "$@"; }
```

**Pre-build the no-model beats** (so Beat 3 + 4 are bulletproof):
```bash
rm -f demo.db demo-identity.key anchors.jsonl
# (Beat 1 will create the session; if no model on stage, run Beat 1 now and
#  keep demo.db so verify/tamper still demo live.)
```

---

## Beat 0 — Cold open (≈30 sec) — no model needed

Open on the **"Proof, not promises"** slide (below). Deliver straight to the room:

> *"Quick question: when an AI agent tells you it finished the job… do you believe
> it? Right now you kind of have to — every tool out there takes the model at its
> word. We built the opposite. You define what **done** actually means, and Truss
> proves whether the agent hit it — independently, with its own checks and a second
> model trying to **disprove** the work. Then it hands you a signed, tamper-evident
> receipt. Agents don't grade their own homework anymore. **This** is Truss."*

Bridge straight into Beat 1:

> *"Let me show you — here's an agent that thinks it nailed it, and a contract that
> says otherwise."*

### Opening slide — "Proof, not promises"

On-slide (terse):
- Others trust the agent's "done ✅". Truss **proves** it.
- Human writes the Definition-of-Done → Truss verifies it independently
- `check` = harness-owned commands · `review` = no-stake refuter model
- Agent never grades its own homework
- Signed, hash-chained verdict = a receipt you can audit

Speaker note (if you linger on the slide instead of going straight to the cold open):
> *"Every harness today takes the model's word for it. The agent says 'done,' and
> that's the verdict. We flip that. You write the Definition-of-Done; **Truss**
> checks it — running its own commands in an environment the agent didn't control,
> and sending the work to a separate reviewer whose job is to **refute** it. If
> reality doesn't match the contract, it fails — even if the agent swears it's
> finished. And every verdict is signed and hash-chained, so you don't get a vibe,
> you get a receipt you can hand an auditor."*

---

## Beat 1 — "Don't give it a prompt. Give it a contract." (≈2 min)

Show the contract first (it's short and human-readable):
```bash
bat demo/contracts/squares.toml      # or: cat
```
Talk track: *"I'm not telling the agent what to do step by step. I'm giving it a
**definition of done**, a budget, and hard limits on what it can touch."*

Run it:
```bash
truss --contract demo/contracts/squares.toml --tools --key demo-identity.key
```
What the room sees: the executor works, then Truss prints a **verdict bundle** —
green checks, the reviewer's note, budget spent, and a signed ledger head.

Talk track: *"Truss didn't take the agent's word for it. It **ran the code itself**
to check the output, and a **second model that did none of the work** reviewed it
and tried to find a reason to reject it."*

---

## Beat 2 — "What if the agent lies?" (≈1.5 min) ⭐ the money moment

Run the **trap** contract — same task, but the agent's output won't actually satisfy
the check (or use `--reviewer-model` of a model that rubber-stamps):
```bash
truss --contract demo/contracts/squares-trap.toml --tools --key demo-identity.key
```
What the room sees: executor "finishes", reviewer even approves — **VERDICT: Fail**,
because Truss's *own* check caught that the work doesn't meet the contract.

Talk track: *"The agent said it was done. The reviewer said it looked good. Truss
still failed it — because trust comes from **evidence the agent didn't generate**,
not from its word. That's the whole idea."*

> Fallback if no model on stage: this is covered conceptually by the slide; skip to
> Beat 3, which is fully local.

---

## Beat 3 — "It can pause for days. And survive a crash." (≈1.5 min) — no model needed*

Show a run that suspends on a signal, kill the process, then resume:
```bash
truss --durable --session onboard --tools --key demo-identity.key \
  "Start onboarding, then await_signal docs_signed before finishing."
# → [suspended] awaiting signal 'docs_signed'   (process exits — point this out)

truss --signal docs_signed --session onboard          # deliver from "anywhere"
truss --resume onboard --tools --key demo-identity.key # continues, exact context
```
Talk track: *"The process fully exited — it could've been three days. It resumes
exactly where it left off, replaying what already happened instead of redoing it."*

\* the suspend/resume *mechanism* is model-free; the surrounding turns use the same
executor as Beat 1. If offline, demo the timer variant you pre-recorded.

---

## Beat 4 — "Every run is signed. Try to fake one." (≈1 min) — no model needed

```bash
truss --verify squares-0001                 # OK: N entries, hash chain + signatures intact
sqlite3 demo.db "UPDATE journal SET payload='tampered' WHERE seq=2;"   # tamper
truss --verify squares-0001                 # TAMPER: verification failed at seq 2
```
Optionally anchor it:
```bash
truss --anchor squares-0001 --anchor-log anchors.jsonl
truss --verify-anchor-log --anchor-log anchors.jsonl
```
Talk track: *"Every step is hash-chained and cryptographically signed. Change one
byte and verification screams. And I can publish just the **hash** to an external
log — proof it existed, unchanged — without leaking a single line of the work.
That matters when the work is under NDA."*

---

## Close (15 sec)
*"Generation isn't the bottleneck anymore — trust is. Truss is the runtime that
makes delegating to agents something you can actually verify. It runs on this
laptop today."*

---

## Timing & safety checklist
- [ ] `cargo build --release` done, `truss` shell function set, font size large.
- [ ] `demo.db` pre-built so Beat 3/4 work with no wifi.
- [ ] Executor model chosen + warmed up (one throwaway run).
- [ ] `presentation/assets/demo.cast` recorded as the ultimate fallback.
- [ ] Terminal theme high-contrast; hide secrets (`--key` file is fine, it's a path).
