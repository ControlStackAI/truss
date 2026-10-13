# ADR 0038: Native Git and GitHub Tools

Status: Accepted

## Context

Phase 5 starts filling daily-driver parity gaps. Coding tasks need git and
GitHub operations, but routing every operation through `bash` makes the tool
surface untyped and weakens Truss's provenance story. A normal coding loop
should be able to run status, diff, commit, PR creation, and check inspection
through first-class tools.

The design gate from `docs/BUILD-PLAN.md` applies:

- git diffs, PR diffs, issue text, and check results can support a verdict, so
  fetched/read artifacts need signed hashes in the ledger;
- commit and PR creation are side effects and must not silently execute twice
  after a durable resume;
- read-only diagnostics should stay lightweight and avoid unnecessary replay
  machinery.

## Decision

Truss adds native tool modules for:

- `git_status`
- `git_diff`
- `git_branch`
- `git_commit`
- `github_pr_view`
- `github_pr_create`
- `github_pr_diff`
- `github_pr_checks`
- `github_issue_view`
- `github_issue_list`
- `github_check_status`

The git tools invoke the `git` executable directly via `tokio::process::Command`
with structured arguments. GitHub tools invoke the `gh` CLI directly for auth
and transport, while exposing typed Truss tool schemas and structured JSON
returns. This keeps the implementation consistent with existing in-process
native tools and avoids turning `bash` into the only integration layer.

Tools are opt-in through `--git-tools`, `--github-tools`, or:

```toml
[tools]
git = true
github = true
```

Tool registration is separate from authorization. `Policy.allow_tools` still
decides which registered tools are advertised and callable. Git tools check
read/write scope against the target repo. GitHub tools require network policy
allowing `github.com` or `api.github.com`.

Durable runs attach the current store/session to the tools. Each durable call
records a signed `git_tool_call` or `github_tool_call` entry with:

- tool name;
- memo key;
- input SHA-256;
- result SHA-256;
- error state;
- side-effect flag;
- optional `produced_by` ledger ref.

Read-only calls record signed provenance but do not memoize. Side-effecting
calls (`git_commit`, `github_pr_create`, and mutating branch actions) use
`Store::append_with_memo` so the ledger entry and replay memo commit
atomically. On a later identical call, the tool replays the memo only if a
matching signed provenance entry exists; otherwise it refuses to use the memo.

## Consequences

Coding contracts can use native git and GitHub operations without shelling out
through the generic bash tool.

Verdicts can cite ledgered hashes for status/diff/PR/check artifacts. The full
tool output is still available in the normal `tool_result` entry created by the
durable loop, while the provenance entry carries compact hashes suitable for
proof review.

Durable resume replays recorded commit/PR-create results instead of
double-applying side effects. A raw memo row or tampered memo cannot bypass the
ledger because replay requires matching signed provenance.

The first milestone deliberately does not add web search/fetch, artifact input,
or multi-channel delivery. Those remain later Phase 5 work.
