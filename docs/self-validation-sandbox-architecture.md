# Self-Validation Sandbox — isolated, reproducible execution for in-loop testing

Status: design (pre-build). Milestone arc: SB-1 → SB-4. First ADR: 0025 (with SB-1).

## Why (surfaced from the SWE in-loop run)

The in-loop SWE-bench finding was concrete: Truss's own agent loop leaned on `bash`, did
**weak local validation**, and **never ran the task's own tests during the solve** — the
Docker grader was the only source of truth. So the model coded blind until the verify
boundary. Two problems fall out of that one observation:

1. **Capability gap** — the agent has no trustworthy, reproducible way to *check its own work
   mid-solve* (run the project's tests, see them pass) before we grade it. Tighter feedback =
   better solves, fewer wasted steps, richer evidence.
2. **Security gap** — today's "sandbox" is application-level only: `Policy` (write/read scope,
   network, `forbidden_substrings`, `allow_tools`) + bash `kill_on_drop` + Ctx deadline. That
   runs as the same user with substring matching — a permission layer, **not** isolation. It
   will not survive buyer security diligence and blocks multi-tenant.

These are different questions (capability vs. security) but they **share one substrate**:
*run tool/test execution inside a confined, reproducible environment.* Build that substrate
once; deliver the capability first (SB-1), harden it into the security boundary after
(SB-2..4). This is the "don't build the box twice" plan.

## Design

### The seam: a `Sandbox` trait (truss-core)

All tool execution (bash today; `run_tests`/file ops next) routes **through** the active
`Sandbox`. `Policy` still applies *on top* (sandbox confines; policy authorizes). Default is
`NoSandbox` = today's behavior exactly → **zero behavior change unless opted in** (same
discipline as the compaction controller).

```rust,ignore
trait Sandbox {
    fn run(&self, cmd: Command, ctx: &Ctx) -> Result<Output>;   // confined exec
    fn materialize(&self, checkout: &Path) -> Result<WorkDir>;  // reproducible workspace
    fn capabilities(&self) -> SandboxCaps;                       // fs/net/pid isolation level
}
```

Implementations, in build order:
- `NoSandbox` — host exec under Policy (current). Default.
- `WorkingCopySandbox` (SB-1) — ephemeral overlay/copy of the checkout so tests run without
  polluting the real tree; process reaped; result captured. Real *reproducibility +
  blast-radius* win even before kernel isolation.
- `LocalSandbox` (SB-2) — unprivileged Linux namespaces (bubblewrap-style: mount/pid/net),
  Policy scopes enforced at the namespace boundary, optional network egress off.
- `ContainerSandbox` (SB-3) — Docker/Podman image for full reproducibility (SWE-bench parity,
  commercial multi-tenant).

### Self-validation flow (the capability)

A model-facing `self_check` path (or sandboxed `run_tests`) runs the project's declared test
command inside the active sandbox during the solve. **Its result is EVIDENCE, never the
verdict** — the executor still cannot grade its own homework. The verify boundary still runs
harness-owned checks + the N-reviewer panel. A self-validation run is recorded as a signed
`self_validation { cmd, exit, summary, workdir_hash }` journal entry → it becomes part of the
provenance the verdict can cite ("the agent's own tests passed, and independently so did
ours").

### Why this is on-thesis

- Reproducible + recorded self-checks make the *evidence bundle stronger*, not just the solve.
- It composes with everything: durable journal (records runs), Policy (authorizes), Ctx
  (deadline/cancel/budget), per-tenant Principal (SB-4 scoping).
- It's the substrate the security isolation needs anyway — capability now, security next, one box.

## Milestones (1 milestone = 1 PR + 1 ADR section)

- **SB-1 — Sandbox trait + WorkingCopySandbox + self-validation evidence (START HERE).**
  `Sandbox` trait; `NoSandbox` default (no behavior change); `WorkingCopySandbox` (ephemeral
  overlay/copy, reaped, captured); model-facing sandboxed test-run path; signed
  `self_validation` journal entry (evidence, NOT verdict). Config-selectable global→agent→
  contract, default off. Hermetic tests (overlay isolation, evidence recorded, default-off
  unchanged). → ADR 0025.
- **SB-2 — LocalSandbox (namespaces).** Unprivileged mount/pid/net isolation (bubblewrap-style);
  Policy scopes enforced at the namespace boundary; network egress toggle. → ADR 0026.
- **SB-3 — ContainerSandbox.** Docker/Podman reproducible env; image pinning; SWE-bench parity
  path reuses it. → ADR 0027.
- **SB-4 — Per-tenant isolation + hardening.** `Principal` → sandbox boundary; seccomp/egress
  policy; closes the documented execution-isolation gap. → ADR 0028.

## Invariants

1. Self-validation output is **evidence, never the verdict** (executor never grades own homework;
   verify boundary unchanged).
2. Default `NoSandbox` — no behavior change unless opted in.
3. Sandbox **composes with `Policy`** (sandbox confines, policy authorizes) — neither replaces
   the other.
4. Every sandbox run is recorded in the signed journal (provenance/recallable).
5. Every knob configurable global → agent → contract with a safe default.
6. The trait is the stable seam; swapping WorkingCopy → Local → Container must not change the
   loop, the journal shape, or the verify boundary.
