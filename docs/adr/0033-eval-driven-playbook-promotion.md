# ADR 0033: Eval-Driven Playbook Promotion

Status: Accepted

## Context

ADR 0032 made Verified Playbooks promote through the same reviewer-panel trust
lane as lessons, then recall only when playbook recall is explicitly enabled.
That proves a draft is plausible, but it does not prove that recalling the
playbook improves a real durable task. Verified Playbooks M3 needs the same
paired learning-on/off evidence discipline introduced for SI-4 lessons.

## Decision

Truss adds an eval-backed promotion route:

```bash
truss playbook promote <id-or-key> --eval --contract contract.toml --repeats 3
```

The command runs the paired eval harness against the supplied contract. The
learning-on arm enables all verified-memory recall under the learning umbrella:
lesson extraction, lesson recall, and bounded playbook recall. The learning-off
arm disables extraction, verification, lesson recall, and playbook recall.

During playbook promotion only, the candidate draft is exposed to the eval's
memory view as a temporary verified playbook. The stored memory entry is not
mutated during the eval. This lets the on-arm measure the honest question:
"would this playbook help if it were trusted?" The off-arm keeps playbook recall
disabled, so any pass-rate delta must come from learning-enabled context.

Promotion requires a positive pass-rate delta:

- `learning_on.pass_rate > learning_off.pass_rate` promotes;
- zero lift and regressions reject.

A passing eval records a signed `playbook_promoted` ledger event containing the
`EvalReport`, derives a memory `Verdict` from that report, and calls
`promote_with_verdict_ref`. A rejected eval records a signed
`playbook_promotion_rejected` event containing the same report and leaves the
draft `Unverified`.

## Consequences

The local panel route remains available as:

```bash
truss playbook promote <id-or-key>
```

Eval-driven promotion is heavier: it requires a real contract and executor, and
it consumes durable child runs. That cost is intentional. The eval path is the
"prove lift before trust" route, not a lightweight import or panel convenience.

The no-fake-trust boundary remains explicit. A playbook draft is never marked
`Verified` because it exists, parses, or sounds useful. Eval promotion needs
signed eval evidence with positive pass-rate lift; no lift means no promotion.
