# Session TUI — interactive daily-driver surface (ST slices)

Status: accepted direction (Matt, 2026-07-09). First slice ST-1.

## Why

Truss is functional as a contract runner and one-shot CLI, but daily-driver use
needs an interactive multi-turn session surface. The substrate already exists:
durable sessions with resume/suspend/signal, the `AgentEvent` stream, approval
gates (VR-4), and the ratatui verdict-review TUI (VR-2/VR-3). This work names a
session front end over those seams — it adds **no new trust machinery**.

ClawTerm (external, egui) remains the rich front end and consumes the same
seams later; the in-repo TUI is the minimal built-in surface. Boundary
discipline from ADR 0036 holds: **TUI is presentation/controller; core remains
the provenance substrate. No UI state in the ledger.**

## ST-1 scope (first slice)

- `truss tui --session <id>` (new session if absent): interactive chat view
  driving the durable agent loop on the existing resume semantics — each user
  message continues the same journal-authoritative session. Quitting mid-run
  and reopening resumes from recorded truth (that's the demo).
- Chat pane renders from the `AgentEvent` stream: streaming
  `AssistantTextDelta`, tool start/result activity lines, context-pressure and
  compaction notices.
- Approval gates inline: when a gated run suspends, the TUI prompts
  approve / reject / needs-changes and routes through the existing
  `Store::record_human_review` + signal path (same as VR-3 actions).
- One keybind to jump to the existing verdict-review view for the current
  session and back.
- Input ergonomics: multi-line editor, Enter-to-send, Ctrl-C cancels the
  in-flight step (Ctx cancel), not the TUI.

## Honest no (ST-1)

- No multi-session tabs, no editor pane, no themes — ClawTerm's job.
- No token deltas persisted to the journal (journal records completed
  results only; streaming is presentation).
- No new ledger entry kinds. Human decisions ride the existing signed
  `human_review` entry.
- Delegated-CLI executors are out of scope for interactive mode (they own
  their own loop); ST-1 is the in-loop path.

## Proof (hermetic)

- Scripted-provider test: send message → streamed deltas rendered → journal
  gains the completed step; kill TUI mid-session → reopen → conversation
  reconstructed from journal, recorded steps not re-executed.
- Gate test: gated tool call suspends → TUI approve → signed `human_review`
  appended → run completes; reject → stays blocked.

## Later slices

- ST-2: session list/picker + new-session flow, model/provider switcher.
- ST-3: richer evidence inline (diff viewer for `work_diff`, check outputs).
- ST-4: ClawTerm transport decision (direct crate vs `serve-mcp` vs WS/JSON
  event server) — decided in the `clawterm` project, consumed here.
