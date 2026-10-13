//! How a contract's WORK gets done — the executor seam (ADR 0003).
//!
//! Two honest abstractions, because a subscription can drive work in two
//! structurally different ways:
//!
//! - [`Executor::InLoop`] — **Truss owns the loop.** A durable agent drives a
//!   [`Provider`] (API key, or a Tier-1 OAuth-replay provider). Full per-step
//!   journal, policy, tool sandbox, the works.
//! - [`Executor::DelegatedCli`] — **a vendor agent owns the inner loop.** We shell
//!   out to the vendor's own CLI (`claude -p`, `codex exec`) with credentials
//!   *cleared from the child env* so the CLI uses its **own** subscription login —
//!   we never replay tokens. The CLI runs its loop/tools; we capture the result and
//!   verify at the contract boundary.
//!
//! Verification (checks + reviewer panel + signed verdict) is identical for both —
//! that's the invariant. Subscription auth never weakens the moat.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ctx::Ctx;
use crate::error::HarnessError;
use crate::provider::Provider;

/// How the contract's work gets executed.
pub enum Executor {
    /// Truss owns the loop: a durable agent drives this provider.
    InLoop(Arc<dyn Provider>),
    /// A vendor agent CLI owns the inner loop; we wrap at the contract boundary.
    DelegatedCli(CliExecutor),
}

/// Where a delegated CLI receives its task prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptVia {
    /// Pipe the prompt to the child's stdin (default — safe for long prompts).
    Stdin,
    /// Append the prompt as the final CLI argument.
    Arg,
}

/// A vendor agent CLI driven as a subprocess (ADR 0003, Tier 2).
///
/// `clear_env` is the load-bearing field: by removing the vendor's API/OAuth env
/// vars from the child, the CLI falls back to its **own** stored subscription login
/// (OpenClaw's model). Truss stores and replays **nothing**.
#[derive(Debug, Clone)]
pub struct CliExecutor {
    /// The program to run (e.g. `claude`).
    pub command: String,
    /// Fixed arguments (e.g. `["-p", "--output-format", "text"]`).
    pub args: Vec<String>,
    /// Optional working directory for the delegated CLI.
    pub cwd: Option<PathBuf>,
    /// Env vars stripped from the child so it uses its own login (e.g.
    /// `ANTHROPIC_API_KEY`).
    pub clear_env: Vec<String>,
    /// How the task prompt reaches the CLI.
    pub prompt_via: PromptVia,
}

impl CliExecutor {
    /// Spawn the CLI, deliver `prompt`, and return its stdout (trimmed) as the
    /// executor summary. Honors [`Ctx`] cancellation and deadline. A non-zero exit
    /// is an error carrying the captured stderr.
    pub async fn run(&self, ctx: &Ctx, prompt: &str) -> Result<String, HarnessError> {
        if ctx.is_cancelled() {
            return Err(HarnessError::Cancelled);
        }

        let mut cmd = Command::new(&self.command);
        cmd.args(&self.args);
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        for key in &self.clear_env {
            cmd.env_remove(key);
        }
        if self.prompt_via == PromptVia::Arg {
            cmd.arg(prompt);
            cmd.stdin(Stdio::null());
        } else {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            HarnessError::Other(format!(
                "failed to spawn delegated executor '{}': {e}",
                self.command
            ))
        })?;

        // Write the prompt concurrently with draining stdout/stderr, so a chatty
        // child can't deadlock us by filling its stdout pipe while we're still
        // feeding stdin.
        let stdin_task = if self.prompt_via == PromptVia::Stdin {
            child.stdin.take().map(|mut stdin| {
                let payload = prompt.to_string();
                tokio::spawn(async move {
                    let _ = stdin.write_all(payload.as_bytes()).await;
                    let _ = stdin.shutdown().await; // EOF so the child stops reading
                })
            })
        } else {
            None
        };

        let token = ctx.cancel_token();
        let wait = child.wait_with_output();
        let output = match ctx.remaining() {
            Some(remaining) => tokio::select! {
                _ = token.cancelled() => return Err(HarnessError::Cancelled),
                r = tokio::time::timeout(remaining, wait) => match r {
                    Ok(o) => o,
                    Err(_) => return Err(HarnessError::Cancelled), // deadline elapsed
                },
            },
            None => tokio::select! {
                _ = token.cancelled() => return Err(HarnessError::Cancelled),
                o = wait => o,
            },
        };

        if let Some(task) = stdin_task {
            let _ = task.await;
        }

        let output = output.map_err(|e| {
            HarnessError::Other(format!("delegated executor '{}' failed: {e}", self.command))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HarnessError::Other(format!(
                "delegated executor '{}' exited with {}: {}",
                self.command,
                output.status,
                truncate(stderr.trim(), 500),
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn stdin_prompt_is_delivered_and_captured() {
        let ex = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "cat".into()],
            cwd: None,
            clear_env: vec![],
            prompt_via: PromptVia::Stdin,
        };
        let out = ex
            .run(&Ctx::new(), "hello from the contract")
            .await
            .unwrap();
        assert_eq!(out, "hello from the contract");
    }

    #[tokio::test]
    async fn arg_prompt_is_delivered() {
        let ex = CliExecutor {
            command: "sh".into(),
            // $0 is the script, $1 is the appended prompt arg.
            args: vec!["-c".into(), "printf '%s' \"$1\"".into(), "sh".into()],
            cwd: None,
            clear_env: vec![],
            prompt_via: PromptVia::Arg,
        };
        let out = ex.run(&Ctx::new(), "via-arg").await.unwrap();
        assert_eq!(out, "via-arg");
    }

    #[tokio::test]
    async fn clear_env_makes_the_cli_use_its_own_login() {
        // Stand-in for a vendor key the parent process holds. Clearing it must make
        // the child see nothing — i.e. it would fall back to its own stored login.
        std::env::set_var("TRUSS_TEST_FAKE_KEY", "leak");

        let cleared = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "printf '[%s]' \"$TRUSS_TEST_FAKE_KEY\"".into()],
            cwd: None,
            clear_env: vec!["TRUSS_TEST_FAKE_KEY".into()],
            prompt_via: PromptVia::Stdin,
        };
        assert_eq!(cleared.run(&Ctx::new(), "").await.unwrap(), "[]");

        // Control: without clearing, the parent's value leaks through.
        let leaky = CliExecutor {
            clear_env: vec![],
            ..cleared.clone()
        };
        assert_eq!(leaky.run(&Ctx::new(), "").await.unwrap(), "[leak]");

        std::env::remove_var("TRUSS_TEST_FAKE_KEY");
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_stderr() {
        let ex = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "echo boom >&2; exit 3".into()],
            cwd: None,
            clear_env: vec![],
            prompt_via: PromptVia::Stdin,
        };
        let err = ex.run(&Ctx::new(), "").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exited"), "got: {msg}");
        assert!(msg.contains("boom"), "stderr should be surfaced: {msg}");
    }

    #[tokio::test]
    async fn already_cancelled_ctx_short_circuits() {
        let ctx = Ctx::new();
        ctx.cancel_token().cancel();
        let ex = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: None,
            clear_env: vec![],
            prompt_via: PromptVia::Stdin,
        };
        let err = ex.run(&ctx, "").await.unwrap_err();
        assert!(matches!(err, HarnessError::Cancelled), "got: {err}");
    }

    #[tokio::test]
    async fn deadline_elapsing_cancels_a_slow_child() {
        let ctx = Ctx::new().with_timeout(Duration::from_millis(150));
        let ex = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: None,
            clear_env: vec![],
            prompt_via: PromptVia::Stdin,
        };
        let err = ex.run(&ctx, "").await.unwrap_err();
        assert!(matches!(err, HarnessError::Cancelled), "got: {err}");
    }

    #[tokio::test]
    async fn cwd_runs_child_from_target_directory() {
        let dir = std::env::temp_dir().join(format!(
            "truss-cli-executor-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "ok").unwrap();
        let ex = CliExecutor {
            command: "sh".into(),
            args: vec!["-c".into(), "pwd; test -f marker.txt".into()],
            cwd: Some(dir.clone()),
            clear_env: vec![],
            prompt_via: PromptVia::Stdin,
        };
        let out = ex.run(&Ctx::new(), "").await.unwrap();
        assert_eq!(out, dir.to_string_lossy());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
