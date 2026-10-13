# Sandbox And Anchor Backends

Sandbox and anchor backends replace infrastructure behind existing trust
boundaries. They should not change model prompts, verifier policy, or journal
entry shapes.

## Sandbox

`truss_core::sandbox::Sandbox` controls command execution:

```rust
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, cmd: SandboxCommand, ctx: &Ctx)
        -> Result<SandboxOutput, ToolError>;

    fn materialize(&self, checkout: &Path) -> Result<WorkDir, ToolError>;
    fn capabilities(&self) -> SandboxCaps;
}
```

Current implementations:

- `NoSandbox`: direct local execution, process reaping only.
- `WorkingCopySandbox`: copies a checkout to an ephemeral workdir and hashes the
  result.

Stronger backends can add containers, VM isolation, network isolation, or remote
builders as long as they keep the same output contract: stdout, stderr, exit
code, and `workdir_hash`.

Verification checklist:

- The backend honors `ctx` cancellation/deadlines.
- Child processes are reaped.
- `capabilities()` is honest.
- The same command produces a stable `workdir_hash` for the same output tree.
- Self-validation still emits the same evidence shape.

## Anchor

`truss_core::anchor::Anchor` publishes a session head hash:

```rust
pub trait Anchor {
    fn anchor(
        &self,
        session_id: &str,
        head_hash: &[u8],
        pubkey: &str,
    ) -> anyhow::Result<AnchorReceipt>;
}
```

Current implementations:

- `NoopAnchor`: default stand-in.
- `FileTransparencyLog`: local append-only JSONL hash chain.

A Rekor, transparency-log, or chain backend should anchor only hashes. Never send
contract text, tool outputs, evidence data, prompts, file contents, or secrets to
the anchor service.

Verification checklist:

- Anchor receipts include log id, position, head hash, entry hash, and timestamp.
- Anchoring failure is explicit; it must not silently claim a receipt.
- The backend has a verification story for the external log.
- Private evidence stays local; only salted roots or head hashes leave the box.
