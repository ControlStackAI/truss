# Tool Architecture: Harness Tools ↔ Provider Tools (design, pre-build)

> Status: DESIGN for review (Matt, 2026-06-18). No implementation yet. Resolves
> the question: should Truss expose its tools through each provider's trained
> typed-tool schemas, and how do harness tools and provider tools work together?
> Principle to protect: verification + provenance (local execution, signed ledger).

## Two kinds of tools

1. **Harness / client tools** — Truss defines the schema; the model emits a
   tool call; **Truss executes locally** and returns the result. Execution stays
   in our control → sandboxing + signed-ledger provenance preserved. (Truss's
   `read_file`, `write_file`, `list_dir`, `bash`, `spawn` are these.)
2. **Provider tools** — schemas the model was **post-trained on**:
   - **(a) typed, client-executed** — e.g. Anthropic `text_editor` / `bash`
     (`text_editor_20250728`, `bash_20250124`); the model knows the exact
     interface, but **the client still executes** and returns results. *Best of
     both: native model fluency + local execution/provenance.*
   - **(b) server-executed** — e.g. web_search, code_execution; the **provider**
     runs them. These **bypass our local execution and provenance**, so use
     selectively and never for the trust-critical path.

## Current state (verified in code)
- Generic custom function tools for `read_file`/`write_file`/`list_dir`/`bash`.
- **Precedent already exists:** the memory tool is wired to Anthropic's typed
  `memory_20250818` schema (`anthropic.rs`). So the pattern is proven — just not
  applied to the file/shell tools.
- Observed cost: the in-loop SWE-bench run leaned on `bash`, used the structured
  file tools little, and validated poorly — consistent with handing the model
  ad-hoc tools instead of the editor/bash interface it was trained on.

## Principle
**Adopt the provider's canonical typed-tool SCHEMA when one exists; keep
EXECUTION local.** One internal capability → mapped to the best available
provider surface. Never trade provenance for a server-executed tool on the
verified path.

## Design
- A **provider-aware tool mapping layer**: each harness capability (edit file,
  read file, run shell) declares a canonical generic schema AND an optional
  per-provider typed-schema binding.
  - Anthropic → `text_editor` for file edits, `bash` for shell (client-executed).
  - OpenAI / codex → their equivalent typed tool surface (confirm exact schemas
    via the `claude-api` / provider guides at build time).
  - Unknown/local providers (Ollama etc.) → fall back to the generic custom tools.
- Execution path is **unchanged and local** regardless of which schema was used
  to present the tool — so the ledger/verification story is identical.
- Tool results normalize back into one internal representation before the loop
  continues, so the agent loop and provenance don't care which surface was used.

## Explicitly NOT doing
- **No SWE-bench-specific tools.** This is a general capability, not benchmark
  scaffolding. We measure SWE-bench *after*, to verify the general improvement.

## Separate, larger question (defer + design on its own)
The in-loop run also showed Truss's agent has **no reliable sandboxed environment
to validate its own work** before the verification boundary (it hit host
Python/dep friction and never ran the task's real tests). That's a general
"execution sandbox for self-validation" capability — valuable for *any* coding
task — but it's a bigger design (sandbox model, per-task env, security) and should
get its own spec, NOT a quick SWE-bench bolt-on.

## Sequence
1. (this) agree the principle: typed schema + local execution.
2. implement provider-aware mapping for file/shell tools (reuse the memory-tool
   pattern).
3. re-run the in-loop benchmark → measure improvement vs the current
   generic-tool baseline (the ~50-task run starting now is that baseline).
4. separately spec the self-validation sandbox.
