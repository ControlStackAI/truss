# Verified Playbooks

Verified Playbooks are the on-moat replacement for skills and slash-command
snippets. A playbook is a governed memory entry, not an untrusted prompt blob.
It starts as `Unverified`, carries signed ledger provenance, and can later move
through the same reviewer/eval promotion machinery as lessons.

The M1 substrate shipped:

- `EntryKind::Playbook` in governed memory;
- markdown/SKILL.md import into structured playbook JSON;
- signed `playbook_imported` journal entries;
- `truss playbook import|list|show|revoke`;
- revocation by demoting the memory entry to `Stale` and recording
  `playbook_revoked`.

M2 adds the trust gate and recall path:

- `truss playbook promote <id-or-key>`;
- the same N-reviewer panel used for lesson promotion and verified delegation;
- signed `playbook_promoted` or `playbook_promotion_rejected` journal entries;
- `promote_with_verdict_ref` provenance for passing verdicts;
- opt-in L2 recall of verified playbooks.

M3 adds eval-backed promotion:

- `truss playbook promote <id-or-key> --eval --contract <file> [--repeats N]`;
- paired learning-on/off evidence over a real contract;
- signed promotion or rejection events containing the `EvalReport`;
- default-to-fail semantics when the eval shows no positive pass-rate lift.

Imported files are still drafts. Truss does not pretend an imported file is
trusted.

## Import

```bash
truss --db harness.db playbook import skills/coding/SKILL.md --key coding-agent
```

The importer extracts a stable schema from headings when present:

- purpose,
- trigger,
- prerequisites,
- procedure,
- checks,
- examples,
- known failures.

If a section is absent, the corresponding field is empty. The original file is
not executed; import is a local transform plus a signed journal event.

## Inspect

```bash
truss --db harness.db playbook list
truss --db harness.db playbook show coding-agent
truss --db harness.db playbook show <memory-id> --json
```

`list` shows playbooks at `Unverified` or above by default so operators can see
fresh imports. Use `--verified-only` when a caller wants only promoted playbooks.

## Promote

```bash
truss --db harness.db --provider openai --model gpt-5.5 \
  --reviewers 3 playbook promote coding-agent
truss --db harness.db playbook promote <memory-id> --json
truss --db harness.db --provider openai --model gpt-5.5 \
  playbook promote coding-agent --eval --contract contracts/rust-review.toml --repeats 3
```

Promotion loads the playbook draft and asks the existing no-stake reviewer panel
whether the playbook is accurate, non-misleading, and safe to rely on later. A
strict majority must pass. A passing panel records a signed `playbook_promoted`
event and promotes the memory entry to `Verified` with `verified_by_ref` pointing
at that event.

With `--eval`, promotion uses paired eval evidence instead of the local panel.
The command requires a real TOML contract and executor. The learning-on arm
enables verified-memory recall, including bounded playbook recall; the
learning-off arm disables learned-memory recall. During this promotion eval, the
candidate draft is visible only to the eval memory view as a temporary verified
playbook so Truss can measure whether recalling it would improve the task. The
stored draft remains `Unverified` unless the eval passes the promotion rule.

Eval promotion is stricter than the eval report's general verdict: the playbook
is promoted only when `learning_on.pass_rate` is greater than
`learning_off.pass_rate`. Zero lift and regressions record a signed
`playbook_promotion_rejected` event containing the `EvalReport`, exit nonzero,
and leave the draft `Unverified`.

Failing or unverifiable panels record a signed `playbook_promotion_rejected`
event and leave the playbook `Unverified`. This is the honest boundary: rejected
promotion attempts are auditable, but they do not create fake trust and do not
surface as trusted recall material.

## Revoke

```bash
truss --db harness.db playbook revoke coding-agent --reason "regressed on rustfmt"
```

Revocation records a signed `playbook_revoked` journal entry and marks the memory
entry `Stale`. It does not delete the old playbook, because the audit trail should
still explain what changed and why.

## Trust Model

- Imported playbooks are drafts.
- Drafts can be listed, inspected, and submitted for promotion, but they are not
  trusted recall material.
- Promotion attaches a reviewer-panel verdict and flips the trust tier to
  `Verified` only on `Pass`.
- Failing or unverifiable promotion attempts leave the draft `Unverified`.
- Verified playbooks can be recalled into L2 context only when playbook recall
  limits are explicitly enabled. Unverified playbooks are never recalled as
  trusted playbooks.
- Future regression should demote the entry to `Stale`, not erase it.
