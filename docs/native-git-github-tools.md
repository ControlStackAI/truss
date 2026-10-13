# Native Git and GitHub Tools

Phase 5 P5-1 adds first-class git and GitHub tools so coding contracts do not
need to treat `bash` as the only integration layer.

Enable them with flags:

```bash
truss --durable --tools --git-tools --github-tools --workdir /path/to/repo \
  --config harness.toml \
  "inspect the diff, commit the fix, open a PR, and read checks"
```

Or enable them in config:

```toml
[tools]
git = true
github = true

[policy]
write_scope = ["/path/to/repo"]
read_scope = ["/path/to/repo"]
network = "allowlist"
allowlist = ["github.com", "api.github.com"]
allow_tools = [
  "read_file",
  "write_file",
  "list_dir",
  "git_status",
  "git_diff",
  "git_commit",
  "github_pr_create",
  "github_pr_checks",
]
```

`[tools]` controls registration. `[policy].allow_tools` still controls which
registered tools the model may call.

## Git Tools

All git tools accept `repo`, defaulting to the current directory. The repo path
must be allowed by the active policy. Read-only tools require read scope;
side-effecting tools require write scope.

| Tool | Purpose | Side effect |
|---|---|---|
| `git_status` | Structured `git status --porcelain=v1 --branch` output. | No |
| `git_diff` | Diff patch plus `patch_sha256`. Supports `staged`, `base`, `head`, and `paths`. | No |
| `git_branch` | `current`, `list`, `create`, `checkout`, or `create_checkout`. | Only create/checkout actions |
| `git_commit` | Optionally stage `all` or `paths`, then commit with `message`. | Yes |

`git_commit` returns the commit SHA, short SHA, previous head, and a hash of the
raw commit output. In a durable run, the result is memoized with signed
`git_tool_call` provenance so resume returns the recorded commit result instead
of creating a second commit.

## GitHub Tools

GitHub tools use the `gh` CLI for auth and transport, but the model-facing
surface is typed. Network policy must allow `github.com` or `api.github.com`.

| Tool | Purpose | Side effect |
|---|---|---|
| `github_pr_view` | View PR metadata, commits, files, and review state. | No |
| `github_pr_create` | Create a PR with `title`, `body`, optional `base`, `head`, and `draft`. | Yes |
| `github_pr_diff` | Fetch PR diff plus `diff_sha256`. | No |
| `github_pr_checks` | Read PR checks through `gh pr checks --json`. | No |
| `github_issue_view` | View issue metadata, labels, comments, and body. | No |
| `github_issue_list` | List issues by state and limit. | No |
| `github_check_status` | Fetch combined commit status and check runs for a ref. | No |

Read-only GitHub outputs are signed into the durable ledger as
`github_tool_call` entries with input and result hashes. `github_pr_create`
also writes a durable memo so resume replays the recorded PR result rather than
creating a duplicate pull request.

## Ledger Semantics

Native git/GitHub provenance follows the same journal discipline as MCP:

- each durable tool call records a signed ledger entry;
- ledger payloads store the tool name, input hash, result hash, error state,
  side-effect flag, optional producer ledger ref, and memo key;
- read-only calls are ledgered but not memoized;
- side-effecting calls are ledgered and memoized atomically;
- a memo is replayed only when a matching signed provenance entry exists.

This means a verdict can cite a git diff, PR diff, issue, or check result by
ledger reference and hash. It also means resume cannot silently repeat a commit
or PR creation after the side effect has already been recorded.
