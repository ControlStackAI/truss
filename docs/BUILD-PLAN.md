# Truss Build Plan

*Status: historical implementation plan. Created 2026-06-20 from the
parity-via-plugins discussion.*

This is the build sequence for turning Truss from a strong verified-agent core into
a tool a Claude Code / Codex / OpenClaw user can actually switch to without losing
daily-driver capability.

This file records the sequencing rationale: what to build, why that order, and how each feature expands
or deliberately does *not* expand the verification/crypto and pause/resume
substrates.

## 1. Product Thesis

Truss wins by letting users review **proofs, not transcripts**. That means the core
must stay small and load-bearing:

- contracts with definition-of-done, budget, and constraints,
- independently produced evidence,
- N-reviewer adversarial verification,
- diff-aware review,
- hash-chained ed25519 signed ledger,
- durable journal-authoritative replay,
- governed memory and verified self-improvement,
- local-first execution with routing/escalation.

Parity with other tools comes through plugins and extension seams, but those plugins
must inherit the moat where it belongs. A plugin that mutates state, calls a tool,
fetches data, produces an artifact, spends budget, or affects a verdict should route
through policy, budget, provenance, and durable replay. A plugin that only renders a
screen or watches an event stream should stay light.

## 2. Design Gate For Every Feature

Every feature, milestone, and plugin must answer these three questions in its spec
or PR notes:

1. **Verification / crypto:** does this produce a claim, artifact, state change, tool
   result, or external observation that should be signed into the ledger, hashed,
   anchored, or reviewed by the panel?
2. **Pause / resume:** does this do I/O, spend money, spawn work, call a model, run a
   tool, or create a side effect that must execute once and replay from the journal
   on resume?
3. **Honest no:** if the answer is no, why is that correct? Presentation-only token
   streaming, transient UI layout, and local read-only diagnostics are legitimate
   no's.

This gate prevents the bad outcome: a differentiated verified core surrounded by a
pile of unverified, unresumable parity plugins.

## 3. Current State

Shipped foundation:

- durable runtime, signals, daemon, replay,
- signed ledger, anchoring, tamper/property tests,
- contracts and N-reviewer verification,
- diff-aware review,
- child-run, spawn, fan-out, panel unification,
- governed memory, verified lessons, paired eval,
- compaction controller and effective-context gauge,
- sandbox substrate SB-1,
- ledger read tools LR-1 through LR-3,
- mdBook docs site,
- MCP architecture spec.

Still missing for daily-driver parity:

- MCP client tool tier,
- plugin authoring docs and examples,
- Verified Playbooks as the on-moat skills replacement,
- verdict-review surface / interactive TUI,
- web/search/fetch and native git/GitHub integration,
- packaging/install,
- stronger sandbox isolation,
- apples-to-apples benchmark number.

## 4. Build Sequence

### Phase 1 - MCP Client Tier

**Why first:** MCP unlocks the largest parity gap in one move. Claude Code and
OpenClaw feel powerful because their tool ecosystem is broad. Truss should inherit
that ecosystem without losing policy, budget, provenance, and durable replay.

**Scope:**

- `[[mcp.servers]]` in layered config and contract/agent config.
- Stdio transport first; HTTP transport next.
- Connect/list/probe.
- Mount advertised MCP tools into the existing tool registry with namespaced IDs.
- Enforce contract-scoped allowlists, policy, budget, deadline, and cancellation.
- Record signed `mcp_tool_call` ledger entries with arg/result hashes and redaction.
- Memoize each call as a durable activity; resume replays recorded result.
- Async/long tools suspend and resume via a handle/signal pattern.

**Lens:**

- Verification / crypto: **yes.** Third-party tool calls become signed provenance.
- Pause / resume: **yes.** Tool calls are durable activities, never re-run on resume.
- Honest no: streaming partial tool output is presentation-only; record the completed
  result.

**Milestones:**

- MCP-1: stdio fake-server tests, list/mount/invoke, signed ledger entry.
- MCP-2: durable memoized replay.
- MCP-3: HTTP transport and auth redaction.
- MCP-4: async suspend/resume.
  Implemented via structured MCP pending results that suspend durable runs on a
  signal and memoize the completed signal payload.
- MCP-5: CLI surface (`truss mcp list|probe`) and docs examples.
  Implemented with human and JSON diagnostics for configured MCP servers.

**Done when:** a contract can use a scoped external MCP tool, the ledger proves what
was called and returned, and resume does not call the external server twice.

### Phase 2 - Plugin Authoring Docs And Examples

**Why second:** once MCP is real, Truss needs a clean story for extension authors.
The docs should make the default path the safe path: plugins opt into the moat
because the extension seams make that easier than bypassing them.

**Scope:**

- Website/mdBook "Write a plugin" section.
- Runnable examples for:
  - Provider plugin,
  - in-process Tool,
  - MCP server tool,
  - ContextProvider,
  - Sandbox,
  - Anchor backend,
  - event/ledger stream consumer.
- A per-example "verification + durability hook" checklist.
- A "when not to journal" section for presentation-only plugins.

**Lens:**

- Verification / crypto: **docs must teach the hook points.** Examples that affect
  work outcomes should emit events and ledger entries.
- Pause / resume: **docs must distinguish durable activities from observers.**
- Honest no: UI widgets and passive event consumers should not be forced into the
  durable journal.

**Done when:** a developer can add a new tool/provider/context plugin from docs
without reading Truss internals, and the examples demonstrate provenance rather than
just function calls.

### Phase 3 - Verified Playbooks

**Why third:** this is the on-moat replacement for skills/slash commands. Copying
Anthropic/OpenClaw skills as markdown blobs would be table stakes. Truss's version
is governed, verified, reversible competence.

**Scope:**

- `Playbook` memory entry kind with schema: purpose, trigger, prerequisites,
  procedure, checks, examples, known failures.
- Import path from markdown/SKILL.md into unverified playbook drafts.
- Promotion path through the N-reviewer panel and paired eval where possible.
- Recall into L2 context by task match and trust tier.
- Revocation/demotion on regression.
- CLI: `truss playbook import|list|show|promote|revoke`.

**Lens:**

- Verification / crypto: **yes.** Playbook promotion is a signed trust-tier
  transition with reviewer evidence and optional eval evidence.
- Pause / resume: **mostly no for authoring; yes for promotion/eval.** Importing a
  file is a local transform. Promotion and eval are durable contract-like work.
- Honest no: recall is context projection over existing memory; don't invent a
  parallel journal path.

**Done when:** a successful run can become a verified playbook, a future run can
recall it, and regression can demote it with provenance.

### Phase 4 - Verdict-Review Surface / Interactive TUI

**Why fourth:** Truss's moat is only useful if humans can review evidence quickly.
The surface should optimize for verdicts, proofs, diffs, ledger heads, and approval
actions, not chat-first transcript reading.

**Scope:**

- Event/ledger read service or CLI-backed stream for ClawTerm/Warp.
- Screens for active contracts, verdict bundle, evidence clauses, reviewer panel,
  diff summary, ledger chain/head hash, budget, and resume/signal controls.
- Approval gates for high-risk tools or spending thresholds. **Implemented in
  VR-4 / ADR 0037** as opt-in global→agent→contract policy that suspends durable
  runs and releases only on signed approved `human_review`.
- Transcript available as drill-down, not the default.

**Lens:**

- Verification / crypto: **partial.** The UI consumes signed evidence and should
  display chain/head status, but the UI itself is not the provenance substrate.
- Pause / resume: **underlying runs yes; UI state no.** The run is durable. Window
  layout, selected tab, and scroll position are presentation.
- Honest no: don't over-engineer UI state into the ledger.

**Done when:** Matt can approve or reject a real Truss task from the verdict view
without reading the full transcript.

### Phase 5 - Native Tool Parity Pack

**Why fifth:** after MCP and verdict review, fill the obvious daily-driver holes
that users expect from Claude Code/Codex/OpenClaw.

**Scope:**

- Web search/fetch tool.
- Native git and GitHub tools for status, diff, branch, commit, PR, issue, checks.
- Image/file artifact input.
- Hook/lifecycle observers.
- Optional delegated adapters for Slack/cron/OpenClaw surfaces where they support
  verified work rather than replacing OpenClaw.

**Lens:**

- Verification / crypto: **yes for fetched content, git artifacts, PRs, and claims.**
  Hash fetched content and diffs; ledger tool outputs used by verdicts.
- Pause / resume: **yes for network and git operations that produce side effects.**
  Replay recorded results or require explicit revalidation.
- Honest no: multi-channel chat delivery is mostly orchestration around Truss, not
  the verified-work core. Keep it as adapters/plugins or leave it to OpenClaw.

**Done when:** a normal coding/research task can run in Truss without reaching for
bash as the only integration layer.

### Phase 6 - Packaging And Install

**Why sixth:** table stakes, but a real adoption blocker. A user cannot evaluate
Truss if the install story is "clone and build the private repo."

**Scope:**

- `cargo install` path for public/open crate names that don't collide.
- Release workflow for Linux/macOS binaries.
- Versioned docs matching release tags.
- First-run sample config and example contracts.
- Optional shell completions.

**Lens:**

- Verification / crypto: **no.** Distribution does not affect work evidence.
- Pause / resume: **no.** Install is outside the runtime.
- Honest no: keep this simple; don't pretend packaging is moat.

**Done when:** a new user can install `truss`, run a sample verified contract, and
inspect the ledger without building from source.

### Phase 7 - MCP Server Tier

**Why after client:** useful but secondary. Truss can become a verification backend
for other harnesses, but the headline product should remain standalone verified
work, not "remote shell over MCP."

**Scope:**

- `truss serve-mcp`.
- `truss_run_contract` tool returning verdict/head hash/bundle reference.
- `truss_contract_status` for async handles.
- Ledger read tools exposed over MCP.
- Per-caller `Principal` and auth for HTTP mode.

**Lens:**

- Verification / crypto: **yes.** The server exposes the signed-verdict substrate.
- Pause / resume: **yes.** Long jobs return handles and reconnect to durable sessions.
- Honest no: do not expose raw internal read/write/bash tools; the unit of work is a
  contract, not an unverified shell.

**Done when:** Claude Code/Codex/OpenClaw can delegate a bounded contract to Truss
and receive a signed verdict without Truss becoming a generic tool server.

### Phase 8 - Sandbox Hardening

**Why here:** SB-1 makes self-validation real, but buyer diligence and multi-tenant
operation need actual isolation.

**Scope:**

- SB-2 namespace/bwrap sandbox.
- SB-3 Docker/Podman sandbox.
- SB-4 per-tenant isolation, seccomp, egress control.
- Attestation fields in sandbox evidence.

**Lens:**

- Verification / crypto: **yes.** Stronger sandboxing makes evidence more credible.
- Pause / resume: **yes where sandbox lifecycle is part of a run.**
- Honest no: local one-shot diagnostics can remain outside heavy isolation if they
  don't affect a verdict.

**Done when:** acceptance/self-check evidence can say not only "this passed" but
"this passed inside a confined, reproducible environment."

### Phase 9 - Benchmarks And Buyer Proof

**Why:** buyers need honest numbers. Truss should not claim "better than Claude
Code" without matched tasks and baselines.

**Scope:**

- Stratified SWE-bench style run.
- Matched baseline: native Claude Code/Codex vs Truss in-loop with same model where
  possible.
- Report pass rate, cost, wall-clock, verification catches, and failure taxonomy.
- Keep failed/ugly results; do not only publish wins.

**Lens:**

- Verification / crypto: **yes.** Benchmark reports should include signed eval
  provenance and official grader outputs.
- Pause / resume: **yes.** Long evals are durable jobs with resumable state.
- Honest no: marketing summaries are downstream projections; the signed eval report
  is the source of truth.

**Done when:** the website can make a narrow, defensible claim with receipts.

## 5. Parallel Work Policy

These can run in parallel if we keep ownership clean:

- MCP implementation and plugin docs can overlap after MCP-1 shape is stable.
- Verified Playbooks can start once the memory/playbook schema is agreed.
- UI can start against ledger/event read APIs before every backend feature lands.
- Packaging can proceed any time after public crate naming is resolved.
- Sandbox hardening should not block MCP unless MCP tool execution needs stronger
  isolation for a specific demo.

Avoid parallelizing two branches that both mutate the core event vocabulary, ledger
schema, or config layering without an explicit coordination note.

## 6. Website Documentation Plan

The website/docs should expose three tracks:

- **Use Truss:** install, configure, run a contract, verify a ledger, resume a run.
- **Extend Truss:** providers, tools, MCP, context providers, sandboxes, anchors,
  event consumers.
- **Trust Truss:** verification model, ledger model, replay model, threat model,
  benchmark receipts.

The plugin authoring section needs to be example-heavy. Each page should show:

- what interface to implement,
- what config block enables it,
- what events/ledger entries it emits,
- what gets replayed on resume,
- what secrets are redacted,
- what intentionally stays off-journal.

## 7. Release Gates

Every milestone should ship with:

- spec or ADR updated,
- docs page or example updated,
- CLI/config reachability if the feature is user-facing,
- tests for the core invariant,
- at least one negative test for trust boundaries,
- `cargo fmt --all --check`,
- `cargo clippy --all-targets --all-features -- -D warnings`,
- `cargo test`,
- `cargo test --all-features`,
- `mdbook build docs` when docs change.

For docs-only changes, `git diff --check` and `mdbook build docs` are enough.

## 8. Immediate Next Moves

1. Open MCP-1 implementation branch.
2. Decide `truss-mcp` crate vs provider module and SDK vs hand-rolled JSON-RPC.
3. Implement fake stdio MCP server tests first.
4. Draft the plugin-authoring docs skeleton in parallel after the MCP API surface
   stops moving.
5. Track remaining release work in GitHub issues.

## 9. What Not To Build Yet

- A broad multi-channel OpenClaw replacement inside Truss.
- Raw MCP server access to internal shell/file tools.
- Dynamic Rust `.so` plugins.
- A polished SDK before the primitive/event/provenance vocabulary settles.
- Public docs hosting if it leaks private ADR/GTM material before the website/domain
  decision.
