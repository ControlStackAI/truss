# ADR 0037: Approval Gates

Status: Accepted

## Context

VR-1 made human review decisions signed, chained, and head-pinned. VR-3 gave
the verdict TUI a controller path for those decisions and for existing durable
signals. VR-4 closes Phase 4 by making selected risky work actually stop until
a human decision exists.

The boundary matters: the TUI cannot become a provenance substrate, and a gate
cannot be a separate pause mechanism. A gate must be durable, visible in the
ledger, opt-in by policy, and default-to-fail.

## Decision

Truss adds an opt-in approval-gate policy resolved global -> agent -> contract:

- `enabled`
- `high_risk_tools`
- `spend_threshold_usd`
- `per_call_spend_threshold_usd`

Tool patterns support exact names and `*` wildcards. The default policy is off
and has no thresholds, so existing runs do not change.

When a configured high-risk tool is about to execute, or model spend crosses a
configured threshold, the durable loop appends a signed `suspended` entry whose
payload carries an `approval_gate` object and awaits the existing
`approval_gate` signal. This reuses the durable suspend/signal substrate; it
does not introduce a second pause table or UI-only state.

`Store::record_human_review` remains the single write primitive for the human
decision. An approved `human_review` entry on a pending approval gate also
queues the existing wake signal. On resume, the durable runner releases an
approval gate only if the ledger contains a signed approved `human_review`
entry after that gate. A raw signal without the signed review does not release
the run.

Reject and needs-changes are blocking decisions. They are recorded as signed
`human_review` entries, but they do not queue the release signal; the run stays
`suspended` and later resumes return still-waiting. A later approved review can
release the same pending gate. This keeps rejection reversible for human
operations while remaining default-to-fail.

## Consequences

High-risk tools are gated before execution. Spend gates are raised after the
model result and spend are journaled, but before the run can complete or act on
the assistant's next tool request.

The machine verdict is never overwritten. Human approval is separate signed
provenance that releases a pause; it does not launder a failing verdict into a
passing one.

The TUI surfaces gated runs from the same ledger data and records approve /
reject / needs-changes through `Store::record_human_review`. It cannot bypass an
approval gate with the generic signal keybinding.
