# Configuration Surface

Truss accepts configuration from a TOML file and then lets CLI flags override the
same resolved values. The normal shape is:

```toml
[provider]
kind = "anthropic"            # anthropic | openai
model = "claude-opus-4-8"
# base_url = "http://localhost:11434/v1"
# api_key_env = "ANTHROPIC_API_KEY"

[agent]
system = "You are a precise engineering assistant."
max_tokens = 4096
max_steps = 24
# reasoning = "high"          # minimal | low | medium | high
# temperature = 0.2

[budget]
max_cost_usd = 2.00
max_total_tokens = 500000
max_steps = 40

[tools]
memory = false
git = false
github = false
web = false

# [tools.web_search]
# endpoint = "https://api.search.brave.com/res/v1/web/search"
# api_key_env = "TRUSS_WEB_SEARCH_API_KEY"

[policy]
write_scope = ["/home/you/project"]
read_scope = ["/home/you/project"]
network = "none"              # none | open | allowlist
allow_tools = ["read_file", "write_file", "list_dir", "bash"]
forbidden = ["rm -rf /", "git push"]

[[mcp.servers]]
name = "github"
transport = "stdio"
command = "gh-mcp-server"
args = ["--repo", "ControlStackAI/truss"]
allow_tools = ["list_issues", "create_pr"]
```

Use it with:

```bash
truss --config examples/harness.toml --tools "summarize this repository"
```

## Memory Recall

Memory recall is default-exact. Existing runs keep the same substring and
lexical ranking behavior unless a config layer explicitly opts into semantic
recall.

```toml
[memory]
store_backend = "sqlite"       # sqlite | ai_memory
# ai_memory_socket = "/run/user/1000/ai-memory.sock" # required for ai_memory
recall = "exact"              # exact | semantic | hybrid
embedding_backend = "open-ai" # open-ai | deterministic
embedding_model = "text-embedding-3-small"
embedding_api_key_env = "OPENAI_API_KEY"
embedding_dims = 1536
# embedding_base_url = "https://api.openai.com/v1"
```

`semantic` embeds new writes and backs `MemoryQuery.contains` with brute-force
cosine ranking over stored vectors. `hybrid` combines exact substring matches
with vector ranking and prefers exact hits before semantic-only hits. Both modes
still compose with `min_trust`, `kind`, `key`, supersession, and query limits;
the default memory view remains verified-only.

The real backend is OpenAI-compatible. `embedding_api_key_env` names the env var
that holds the API key and defaults to `OPENAI_API_KEY`; `embedding_base_url` can
point at a compatible endpoint. The deterministic backend is for hermetic tests
and local smoke runs, not quality recall.

`ai_memory` is an explicit opt-in and currently supports exact recall only.
The socket path must be absolute. SQLite remains the default when
`store_backend` is omitted.

Rows written before S1 have no vector. Backfill them explicitly:

```bash
truss --config examples/harness.toml --db harness.db memory reindex
truss --config examples/harness.toml --db harness.db memory reindex --json
```

Semantic queries fail with a reindex-needed error if otherwise eligible entries
lack vectors for the current embedder id/dimensions, so old memory is not
silently missed.

See [ADR 0040](adr/0040-semantic-memory-recall.md) for the backend decision and
invariant preservation notes.

## Memory Compaction

Governed compaction is default-off. When enabled, it only changes the projected
context sent to the provider; the durable journal remains authoritative and every
compaction is recorded as a signed ledger event.

```toml
[memory.compaction]
mode = "off"          # off | summarize | evict
threshold = 0.70      # context pressure, 0.0 through 1.0
min_tail = 6          # recent messages retained verbatim
anticipate = false    # opt in to compact-ahead behavior
# lookahead_steps = 1
# exempt_tiers = ["l0_core", "l0_self", "l1_principal", "l3_goal"]
```

The block can be set globally as `[memory.compaction]`, under
`[agent.memory.compaction]`, or in a contract's `[memory.compaction]`. Resolution
is global, then agent, then contract. CLI flags override the resolved policy:

```bash
truss --durable \
  --compaction-mode summarize \
  --compaction-threshold 0.70 \
  --compaction-min-tail 6 \
  --compaction-anticipate \
  "..."
```

See [ADR 0024](adr/0024-compaction-cli-config-surface.md) for the accepted CLI
surface and [Contract with Compaction](examples/contract-compaction.md) for a
worked example.

## Approval Gates

Approval gates are default-off. When enabled, they suspend durable runs before
configured high-risk tools execute or after spend crosses a configured threshold.
The run releases only when a signed approved `human_review` entry is recorded;
reject and needs-changes entries keep it suspended.

```toml
[approval_gates]
enabled = false
high_risk_tools = ["bash", "write_file", "mcp:payments:*"]
spend_threshold_usd = 1.00
per_call_spend_threshold_usd = 0.25

[agent.approval_gates]
enabled = true
high_risk_tools = ["bash"]
```

Contract files can set `[approval_gates]` as the highest-precedence layer. Tool
patterns support exact names and `*` wildcards. See
[ADR 0037](adr/0037-approval-gates.md) for the release/block semantics.

## Native Git and GitHub Tools

Native git and GitHub tools are default-off. Enable them with CLI flags:

```bash
truss --durable --tools --git-tools --github-tools --workdir /home/you/project "..."
```

Or enable them in config:

```toml
[tools]
git = true
github = true

[policy]
write_scope = ["/home/you/project"]
read_scope = ["/home/you/project"]
network = "allowlist"
allowlist = ["github.com", "api.github.com"]
allow_tools = [
  "read_file",
  "write_file",
  "git_status",
  "git_diff",
  "git_commit",
  "github_pr_create",
  "github_pr_checks",
]
```

Git tools are registered as `git_status`, `git_diff`, `git_branch`, and
`git_commit`. GitHub tools are registered as `github_pr_view`,
`github_pr_create`, `github_pr_diff`, `github_pr_checks`,
`github_issue_view`, `github_issue_list`, and `github_check_status`.

In durable and contract runs, these tools record signed `git_tool_call` and
`github_tool_call` provenance entries. Read-only outputs are hashed into the
ledger; side-effecting operations such as `git_commit` and `github_pr_create`
also memoize the result so resume does not repeat the side effect. See
[Native Git and GitHub Tools](native-git-github-tools.md) and
[ADR 0038](adr/0038-native-git-github-tools.md).

## Native Memory Tools

Native memory query/view tools are default-off. Enable them with a CLI flag:

```bash
truss --durable --tools --memory-tools --workdir /home/you/project "..."
```

Or with config:

```toml
[tools]
memory = true

[policy]
allow_tools = ["read_file", "memory_query", "memory_view"]
```

`memory_query` searches L2 durable memory using the resolved memory recall mode.
`memory_view` fetches entries by id. Both tools are read-only and separate from
the governed singular `--memory-tool` write binding.

Both default `min_trust` to `verified`; callers must explicitly opt in to widen
the floor. Rejected memory is never surfaced. In durable and contract runs, both
tools record signed `memory_tool_call` provenance and memoize the exact result.
Resume replays the recorded memo after verifying it against the signed ledger.
Pass `revalidate = true` to query the current memory store and record a new
observation.

See [Native Memory Tools](native-memory-tools.md) and
[ADR 0041](adr/0041-native-memory-tools.md).

## Native Web Tools

Native web tools are default-off. Enable them with a CLI flag:

```bash
truss --durable --tools --web-tools --workdir /home/you/project "..."
```

Or with config:

```toml
[tools]
web = true

[tools.web_search]
endpoint = "https://api.search.brave.com/res/v1/web/search"
api_key_env = "TRUSS_WEB_SEARCH_API_KEY"

[policy]
network = "allowlist"
allowlist = ["example.com", "api.search.brave.com"]
allow_tools = ["read_file", "web_fetch", "web_search"]
```

`web_fetch` fetches HTTP(S) URLs and returns status, final URL after redirects,
content type, readable content, `raw_sha256`, and `content_sha256`.
`web_search` returns ranked search results and `results_sha256`.

The default search backend is Brave Search. The API key is read from
`TRUSS_WEB_SEARCH_API_KEY` unless `[tools.web_search].api_key_env` points at a
different env var. The endpoint defaults to Brave Search and can be overridden by
`TRUSS_WEB_SEARCH_ENDPOINT` or `[tools.web_search].endpoint`.

In durable and contract runs, both tools record signed `web_tool_call` provenance
and memoize the exact result. Resume replays the recorded memo after verifying it
against the signed ledger. Pass `revalidate = true` to fetch/search again and
record a new observation.

The verification boundary is intentionally narrow: Truss hashes and signs content
as retrieved. It does not certify that a fetched page or search result is true.
See [Native Web Tools](native-web-tools.md) and
[ADR 0039](adr/0039-native-web-tools.md).

## Sandbox

Sandbox selection is also layered global, then agent, then contract. The default
is `no_sandbox`, which preserves the host shell execution path. `working_copy`
materializes an ephemeral checkout for self-validation evidence and removes it
when the command finishes.

```toml
[sandbox]
mode = "no_sandbox"       # no_sandbox | working_copy
checkout = "."

[agent.sandbox]
mode = "working_copy"
```

Contract files can set `[sandbox]` as their highest-precedence layer. See
[ADR 0025](adr/0025-self-validation-sandbox.md) and
[Self-Validation Sandbox Architecture](self-validation-sandbox-architecture.md)
for the verification boundary and limitations.

## MCP Servers

MCP servers can be declared globally in `--config` or inside a contract. Stdio is
the default transport. HTTP transport is available when Truss is built with the
`mcp-http` feature. Tools are mounted as `mcp:<server>:<tool>` and still flow
through `Policy.allow_tools`.

```toml
[[mcp.servers]]
name = "demo"
transport = "stdio"
command = "python3"
args = ["examples/fake-mcp-server.py"]
allow_tools = ["echo"]
```

HTTP servers use `url`. Static non-secret headers can go in `headers`; secret
values should use `env_headers`, mapping header name to environment variable.
Header values are used only at request time and are never written to the ledger.
Network policy still applies, so set `policy.network = "open"` or an allowlist
that includes the MCP host.

```toml
[[mcp.servers]]
name = "remote"
transport = "http"
url = "https://mcp.example.com/rpc"
allow_tools = ["search"]

[mcp.servers.headers]
"x-client" = "truss"

[mcp.servers.env_headers]
"authorization" = "REMOTE_MCP_AUTH_HEADER"
```

In durable and contract runs, mounted MCP calls record a signed `mcp_tool_call`
ledger entry with transport, server/tool names, argument hash, result hash, and
memo key. URLs and headers are intentionally omitted. See
[Contract with MCP Tool](examples/contract-mcp.md).

MCP tools can also return a Truss pending content item:
`{ "type": "pending", "signal": "job-name", "reason": "optional reason" }`.
Durable runs record signed `mcp_tool_pending` provenance, suspend on that signal,
and, when the signal is delivered, record the signal payload as the final signed
`mcp_tool_call` result and memo. Resume therefore never re-invokes the external
MCP server for the pending operation.

Operator diagnostics are available without starting a model run:

```bash
truss --config examples/harness.toml mcp list
truss --config examples/harness.toml mcp probe demo
truss --config examples/harness.toml mcp probe demo --json
```

`list` reads configured servers without launching them. `probe` connects to one
server through the normal MCP mount path and reports the advertised
`mcp:<server>:<tool>` definitions. HTTP probes obey the configured network
policy.

## CLI Flags

Common run flags:

| Flag | Config equivalent | Purpose |
|---|---|---|
| `--config` | n/a | Load a TOML config file. |
| `--provider` | `provider.kind` | Select `anthropic` or `openai`. |
| `--model` | `provider.model` | Select the model id. |
| `--base-url` | `provider.base_url` | Point OpenAI-compatible providers at local or alternate endpoints. |
| `--system` | `agent.system` | Override the system prompt. |
| `--max-steps` | `agent.max_steps` | Cap model/tool loop steps. |
| `--max-tokens` | `agent.max_tokens` | Cap output tokens per model call. |
| `--reasoning` | `agent.reasoning` | Set minimal, low, medium, or high reasoning effort. |
| `--temperature` | `agent.temperature` | Set sampling temperature where supported. |
| `--tools` | n/a | Register built-in tools. |
| `--git-tools` | `tools.git` | Register native git tools. |
| `--github-tools` | `tools.github` | Register native GitHub tools backed by `gh`. |
| `--web-tools` | `tools.web` | Register native web tools (`web_fetch`, `web_search`). |
| `--memory-tools` | `tools.memory` | Register read-only native memory tools (`memory_query`, `memory_view`). |
| `--memory-tool` | n/a | Register the governed memory tool for durable sessions. |
| `--spawn-tool` | n/a | Register store-backed child-run spawning for durable or contract runs. |
| `--workdir` | policy scopes | Confine reads and writes to a directory. |
| `--budget-usd` | `budget.max_cost_usd` | Set a hard run cost ceiling. |
| `--budget-tokens` | `budget.max_total_tokens` | Set a hard token ceiling. |
| `--timeout` | n/a | Set a wall-clock deadline in seconds. |
| `--log` | n/a | Append JSONL events to a file. |
| `--verbose` | n/a | Print every event as it happens. |

Durable and verification flags:

| Flag | Purpose |
|---|---|
| `--durable` | Enable SQLite-backed durable execution. |
| `--db` | Set the durable store path, default `harness.db`. |
| `--session` | Set or target a durable session id. |
| `--resume` | Resume a durable session by id. |
| `--verify` | Verify a durable session hash chain and signatures. |
| `--key` | Select the ed25519 signing identity path. |
| `--expect-key` | Pin the expected public key while verifying. |
| `--signal` / `--payload` | Deliver a named durable signal. |
| `--daemon` / `--poll-secs` | Auto-resume sessions whose waits are satisfied. |
| `--anchor` / `--anchor-log` | Anchor a session head hash to a transparency log. |
| `--verify-anchor-log` | Verify the transparency log chain. |
| `--max-context-msgs` | Legacy projection: keep Head plus this many recent turns. |

Contract flags:

| Flag | Purpose |
|---|---|
| `--contract` | Execute a TOML contract, then independently verify it. |
| `--reviewer-model` | Select the no-stake reviewer model. |
| `--reviewers` | Override the contract reviewer panel size. |

Compaction flags:

| Flag | Purpose |
|---|---|
| `--compaction-mode` | `off`, `summarize`, or `evict`. |
| `--compaction-threshold` | Context pressure threshold, `0.0..=1.0`. |
| `--compaction-min-tail` | Verbatim tail messages retained after compaction. |
| `--compaction-anticipate` | Enable compact-ahead behavior. |
