# demo/

Everything for the **live demo** (AI Clubhouse Meetup, Jun 28, LA).

- **`runbook.md`** — the four-beat stage script with exact commands, talk tracks,
  timing, and fallbacks. Start here.
- **`contracts/`** — demo contracts:
  - `squares.toml` — happy path → **PASS** (Truss runs the code to verify).
  - `squares-trap.toml` — the agent "finishes" and a reviewer approves, but Truss's
    independent check **FAILS** it (the human's acceptance bar wins).
  - `greeting.toml` — the simplest possible contract (file write + content check).

## The 30-second version of the demo

1. **Give it a contract, not a prompt.** `truss --contract squares.toml --tools` →
   the agent writes the code, Truss *runs it* to check, a no-stake reviewer judges,
   you get a **green verdict** — not a transcript.
2. **The agent lies.** `truss --contract squares-trap.toml --tools` → executor says
   done, reviewer approves, **VERDICT: Fail** — Truss's own evidence caught it.
3. **It survives crashes & pauses for days.** suspend on a signal → process exits →
   `--signal` → `--resume` → exact context.
4. **Every run is signed.** `--verify` passes; tamper one byte → `--verify` fails.

## Recording the fallback
Before the event, capture a clean run (e.g. `asciinema rec presentation/assets/demo.cast`
or a screen recording) so the demo survives no-wifi / cold-model conditions.
