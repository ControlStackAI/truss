# ADR 0036: TUI Review Actions

Status: Accepted

## Context

VR-1 made human review decisions signed ledger provenance through
`human_review`. VR-2 made the proof surface readable in a terminal, but still
forced the operator to leave the view and run a separate `truss review
approve|reject|needs-changes` command. That split is clunky: the human forms
the judgment in the TUI, then has to context-switch to record it.

The risk is architectural drift. The TUI must not become a parallel provenance
system, and it must not duplicate the durable runner just to provide a "resume"
button. The load-bearing boundary remains: core records facts; the TUI only
drives core APIs.

## Decision

The review TUI now exposes action keybindings:

- `a` approve
- `r` reject
- `n` needs changes
- `s` deliver the pending signal for a suspended run
- `x` show the existing resume command for the selected run

Approve/reject/needs-changes open an in-terminal note prompt. Pressing enter
calls `Store::record_human_review`, which appends the same signed
`human_review` entry introduced by ADR 0034. The TUI then refreshes its
`LedgerReader` snapshot, so the human sign-off appears beside the machine
verdict.

Signal delivery opens an optional payload prompt, then calls
`Store::add_signal` with the pending signal name found in the selected run's
latest `suspended` ledger entry. This reuses the existing durable
suspend/signal substrate. The TUI does not record a bespoke UI event.

Resume is intentionally not executed inside the TUI. Resuming requires provider
construction, tool registry setup, memory config, MCP mounting, budgets, and
policy resolution from the normal CLI path. The TUI exposes a resume control by
showing the exact command:

```text
truss --resume <session> --db <db>
```

That keeps one durable runner in the system.

## Consequences

The "review proofs, then act" loop is now single-surface for human sign-off:
the same view that shows evidence can append the signed decision. The ledger
remains the source of truth and the machine verdict remains separate from the
human sign-off.

Suspended runs can be nudged from the review surface without adding a new
signal mechanism. Full resume remains on the established CLI runner path, which
is less cute but much cleaner: no duplicated provider/tool setup, no silent
semantic fork, no UI-side provenance.

The TUI is no longer read-only, but its write set is deliberately tiny:
`human_review` entries and pending durable signals, both through core `Store`
APIs.
