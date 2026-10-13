# MCP architecture — client tier (headline) + server tier (secondary)

*Status: spec + MCP-1 implementation. 2026-06-20. Repo target v0.42.0.*

MCP (Model Context Protocol) is the keystone parity move: one protocol and Truss
inherits the entire external-tool ecosystem that Claude Code and OpenClaw rely on.
This spec covers **two directions**, run through the *verification + durability lens*
(described in the build plan):

- **Client (headline):** Truss *consumes* external MCP servers. The path to real
  parity — and parity *that's still on-moat*, because every external tool call is
  policy-gated, budget-metered, signed into the ledger, and replayed durably.
- **Server (secondary, not highlighted):** Truss *exposes itself* as an MCP server so
  another harness (Claude Code, Codex, an IDE) can delegate a **verified contract** to
  Truss and receive a signed verdict. Real benefit (Truss becomes a verification
  backend others plug into), but not a marquee feature — keep it lean.

Aligns with `docs/extensibility-architecture.md` ("custom tools → MCP out-of-process
tool tier") and ADR 0001 (plugin tiers).

---

## Part 1 — MCP client tier (headline)

### 1.1 The lens (why this is on-moat, not just parity)

- **Verification: YES.** Every MCP tool invocation becomes a **signed `mcp_tool_call`
  ledger entry** — server id, tool name, argument hash, result hash, provenance
  `LedgerRef`. Results that feed a claim are available to the N-reviewer panel and to
  `truss ledger`. So third-party tools inherit the audit trail by construction. *No
  competitor offers audited MCP execution.* This is the differentiator, not a
  checkbox.
- **Durability: YES.** Each call is a **memoized durable activity** (same machinery
  as the existing in-proc tool memo). On resume, recorded results replay — Truss does
  **not** re-invoke the external server (principle #1: replay recorded truth). Long /
  async tools map to a **suspend point** (reuse `await_signal`): suspend the run, let
  the external job finish, resume on signal with the recorded result.
- **Honest non-goals:** MCP *resources* and *prompts* (vs *tools*) are lower value for
  our loop initially; **tools first**. Streaming partial tool output to a screen is
  presentation (off-journal); the **completed** result is what's journaled.

### 1.2 Where servers are declared — contract/agent config, never global

Scoping is per-agent / per-contract **by construction** (this is the thing OpenClaw's
global MCP registry got wrong for multi-tenant). MCP servers are declared in the
contract or agent config, so a contract's tool surface is part of its signed
definition:

```toml
# in a contract or agent config
[[mcp.servers]]
name      = "github"
transport = "stdio"                 # stdio | http
command   = "gh-mcp-server"         # for stdio
args      = ["--repo", "ControlStackAI/truss"]
# url     = "https://..."           # for http transport
allow_tools = ["list_issues", "create_pr"]   # allowlist; empty = all advertised
budget_calls = 50                   # per-run cap, counts against Ctx budget
```

The set of declared servers + allowlists is part of the contract → it's covered by the
contract hash → the **verdict attests which external tools were even available**.

### 1.3 Lifecycle & wiring

1. **Connect** declared servers at run start (stdio child process with `kill_on_drop`,
   or HTTP client). Handshake, list tools.
2. **Mount** advertised tools (post-allowlist) into the loop's tool registry alongside
   in-proc tools (`ReadFile`/`WriteFile`/`ListDir`/`Bash`/`memory`/`spawn`). Namespace
   as `mcp:<server>:<tool>` to avoid collisions.
3. **Present** via provider-typed schema where it maps (precedent: `provider_alias.rs`,
   ADR 0019); otherwise as a generic custom tool. Execution stays **local to Truss**
   (we own provenance), only the tool *call* crosses to the server.
4. **Invoke** inside the agent loop: enforce `Policy` (network/forbidden) + `Ctx`
   budget/deadline/cancel → call server → record signed `mcp_tool_call` ledger entry +
   memo → return result to the model.
5. **Resume:** memoized result replays; no re-call. Async tool → suspend/await_signal.
6. **Verify:** `truss ledger` shows MCP calls with arg/result hashes; the panel can
   cite them.

### 1.4 New surface

- Core: `truss_core::mcp` module (client) — `McpServerConfig`, stdio JSON-RPC
  framing, and a tool adapter onto the existing `Tool` trait. Ledger entry kind
  `mcp_tool_call`.
- CLI: `truss mcp list` (declared servers + advertised tools), `--mcp-config` overlay,
  `truss mcp probe <server>` (connectivity/diagnostics).
- Config: `[[mcp.servers]]` block, layered global→agent→contract.

### 1.5 Milestones (client)

- **MCP-1** stdio transport + connect/list/mount + invoke wrapped in policy/budget +
  signed `mcp_tool_call` ledger. Hermetic fake-server tests. Implemented as the
  first PR.
- **MCP-2** durable memoized replay (same server/tool/input = recorded result,
  no re-call) backed by the signed `mcp_tool_call` provenance hash and existing
  `tool_memo` substrate. Implemented as the second PR.
- **MCP-3** HTTP transport + auth (env-injected, never journaled — see crypto note).
  Implemented behind the off-by-default `mcp-http` feature so `truss-core`
  remains vendor-free by default.
- **MCP-4** async tool suspend via `await_signal`. Implemented with structured
  MCP pending content that records `mcp_tool_pending`, suspends the durable run,
  and records the delivered signal payload as the final signed `mcp_tool_call`.
- **MCP-5** CLI diagnostics surface. Implemented as `truss mcp list` and
  `truss mcp probe <server>` with human and JSON output. Provider-typed
  presentation for mapped tools can ride the broader native-tool parity pack.

---

## Part 2 — MCP server tier (secondary, not highlighted)

### 2.1 The pitch (and why it's secondary)

Expose Truss over MCP so another harness can call it as a tool. The headline tool:

- `truss_run_contract(intent, done, budget, constraints, …) -> { verdict, head_hash,
  anchor, bundle_ref }` — the caller delegates work and gets back a **signed verdict +
  anchored evidence head**, not a transcript. Truss becomes the *verification backend*
  inside someone else's loop.
- Read tools: `truss_ledger_verdict(session)`, `truss_ledger_export(session)` — let an
  external UI/agent read proofs.

Why secondary: it's a genuine wedge (a Claude Code user could route high-stakes work
through Truss for the audit trail without leaving Claude Code), but it inverts the
"Truss owns the loop" thesis and the headline story is the standalone product. Build it
lean; don't let it pull design weight from the client tier.

### 2.2 The lens

- **Verification: YES — this *is* the substrate exposed.** The server's whole value is
  returning a signed verdict + anchor. Map directly onto `delegation::run_contract` +
  `truss ledger` reads. Nothing new on the trust path.
- **Durability: YES, and it's a feature.** A delegated contract is a normal durable
  Truss session → the caller gets a `session`/`head_hash` handle and can **reconnect to
  a long-running delegated job** (suspend/resume across the MCP boundary). Honest edge:
  MCP request/response is typically synchronous; long contracts need an async pattern
  (return a handle immediately + a `truss_contract_status(session)` poll/await tool)
  rather than blocking the MCP call.
- **Honest non-goal:** don't expose Truss's *internal* tools (read/write/bash) over the
  server — that's just remoting a shell and drops the verification point. The server's
  unit of work is a **contract**, never a raw tool.

### 2.3 Surface & milestones (server)

- Core/bin: `truss serve-mcp`, a thin MCP server exposing `truss_run_contract` +
  `truss_contract_status` + ledger read tools, backed by existing `delegation` +
  `LedgerReader`.
- **MCP-S1** stdio server + `truss_run_contract` (sync, bounded contracts) + verdict
  return. Implemented in the first server slice.
- **MCP-S2** async handle + `truss_contract_status` (durable reconnect). The stdio
  server can return a handle immediately, then poll in-process job state or the
  durable session by id. Resume semantics continue to ride the existing journal:
  recorded results are replayed, not re-executed.
- **MCP-S3** http transport + auth + per-caller `Principal` isolation. HTTP mode is
  currently an honest CLI error rather than a partial unauthenticated server.
- Sequencing: ships **after** the client tier (MCP-1..4). Spec'd now so the client
  design doesn't paint the server into a corner (shared `truss-mcp` transport code,
  shared tool-schema types).

### 2.4 Implemented stdio server slice

`truss serve-mcp --transport stdio` advertises exactly these tools:

- `truss_run_contract`
- `truss_contract_status`
- `truss_ledger_verdict`
- `truss_ledger_export`

`truss_run_contract` takes a contract TOML path and optional `session_id`. A
synchronous call returns the contract verdict, the signed ledger verdict bundle,
the session `head_hash`, and a `bundle_ref` pointing at `truss_ledger_export`.
With `async = true`, it returns a handle immediately and `truss_contract_status`
polls that handle.

Stdio mode carries one process-level principal:

```bash
truss --db harness.db --key harness-identity.key serve-mcp \
  --principal-tenant local \
  --principal-user alice
```

The principal is threaded into the contract `Ctx` and store session. HTTP auth and
request-scoped principals are deliberately left for the HTTP slice.

The server trust boundary is negative as well as positive: raw internal tools
such as `bash`, `read_file`, `write_file`, and `list_dir` are not advertised and
return an unknown-tool JSON-RPC error if called. Contracts may still use allowed
tools internally, but those calls remain inside Truss policy, budget, sandbox,
provenance, and durable replay.

---

## Cross-cutting

- **Crypto/secrets:** MCP server auth (tokens, API keys) is **injected from env/config,
  never written to the journal** (same discipline as provider auth — `codex_auth.rs`
  has no `Debug`). Argument hashes in the ledger must redact declared-secret fields.
- **Shared code:** one `truss-mcp` module owns transport (stdio/http) + JSON-RPC framing
  + tool-schema types, used by both client and server. Adding MCP must not add a vendor
  dep to `truss-core`'s vendor-free guarantee — put the client/server behind features in
  an appropriate crate (likely `truss-providers` or a new `truss-mcp` crate; decide at
  MCP-1).
- **Plugin-authoring docs (paired deliverable):** the website's "write a plugin" guide
  documents MCP as the primary out-of-process extension path, *with the verification +
  durability hooks called out* so third-party tools opt into the moat by default.

## Open questions (resolve at MCP-1)

1. New `truss-mcp` crate vs module in `truss-providers`? (Vendor-dep boundary.)
2. MCP Rust SDK (`rmcp`/official) vs hand-rolled JSON-RPC? (Dep weight vs control —
   mirrors the SSE hand-roll decision.)
3. Async-tool suspend: reuse `await_signal` as-is, or a dedicated `mcp_pending` await
   kind?
4. Server async pattern: handle+poll vs MCP progress notifications.
