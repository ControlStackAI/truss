# ADR 0035: Read-Only Review TUI

Status: Accepted

## Context

ADR 0034 made the human approve / reject / needs-changes decision signed,
chained ledger provenance, reachable from a scriptable, terminal-free CLI. The
verdict-review surface is only realized, though, if a human can actually *look*
at the proofs — the definition-of-done clauses, the independent evidence, the
reviewer panel result, the diff, and the chain head/verify status — and form a
judgement without scrolling a raw transcript. That viewing surface is VR-2.

The load-bearing rule from the Phase 4 spec is the provenance boundary:
*provenance-capturing is core; stream-consuming is plugin/CLI.* The viewing
surface captures no provenance, so it must not touch core.

## Decision

Truss ships a read-only interactive review TUI inside `truss-cli`, built on
`ratatui` + `crossterm`, behind an off-by-default `tui` cargo feature so
headless/CI and `--no-default-features` builds never pull in the terminal stack.

The TUI is a **pure consumer** over `truss_core::ledger::LedgerReader`. It calls
`list_sessions`, `verdict`, `verify`, and `entries`, snapshots the results into
transient in-memory state, and renders four screens:

1. **List** — recent/active sessions with the ledger trust label, the machine
   verdict status, and the human review status (with a stale marker when the
   sign-off no longer matches the reviewable head).
2. **Verdict bundle** — definition-of-done clauses, clause evidence,
   self-validation evidence, the reviewer panel majority and per-reviewer
   verdicts, the diff summary, and the budget/spend. The machine verdict and the
   human sign-off render side by side (design decision #4): they are distinct
   trust signals and neither launders the other.
3. **Ledger chain** — head hash plus the verify status and per-entry hashes,
   so an intact-vs-tampered chain reads at a glance.
4. **Transcript** — reachable but never the default view.

`truss tui` launches it; `truss review` with no decision subcommand launches it
too. The existing non-interactive `truss review approve|reject|needs-changes`
forms are unchanged. When the binary is built without the `tui` feature, both
entry points fail with a clear "rebuild with `--features tui`" message rather
than panicking.

UI state — selected session, active screen, scroll offset — is presentation and
is never written to the ledger (design decision #5). The rendering layer is a
pure function of the in-memory snapshot, so every screen is exercised by
hermetic render tests over a fixture database using ratatui's `TestBackend`;
the tests require no terminal, network, or credentials.

## Consequences

The "done when" viewing half of the verdict-review surface is met: a human can
see the verdict, evidence, panel, diff, budget, and chain status without reading
the transcript, and the trust tiers read at a glance. No new provenance is
introduced and no core code changes — the TUI rides the stable read API, the
same seam an external front end (ClawTerm/Warp) would consume.

The TUI is read-only by construction: it has no write path to the ledger. Wiring
the on-screen approve / reject / needs-changes actions to the ADR 0034 signed
primitive is deferred to VR-3, and policy-driven approval gates to VR-4.
