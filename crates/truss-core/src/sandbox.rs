//! Sandboxed command execution seam.
//!
//! SB-1 keeps the default path identical with [`NoSandbox`], but routes shell
//! execution through this trait so stronger isolation can replace the
//! implementation later without changing tools, journal shape, or verification.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::process::Command as TokioCommand;

use crate::ctx::Ctx;
use crate::error::ToolError;

static WORKDIR_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    #[default]
    NoSandbox,
    WorkingCopy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfigLayer {
    #[serde(default)]
    pub mode: Option<SandboxMode>,
    #[serde(default)]
    pub checkout: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxConfig {
    pub mode: SandboxMode,
    pub checkout: Option<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: SandboxMode::NoSandbox,
            checkout: None,
        }
    }
}

impl SandboxConfig {
    pub fn resolve(
        global: Option<&SandboxConfigLayer>,
        agent: Option<&SandboxConfigLayer>,
        contract: Option<&SandboxConfigLayer>,
    ) -> Self {
        let mut cfg = SandboxConfig::default();
        for layer in [global, agent, contract].into_iter().flatten() {
            if let Some(mode) = layer.mode {
                cfg.mode = mode;
            }
            if let Some(checkout) = &layer.checkout {
                cfg.checkout = Some(checkout.clone());
            }
        }
        cfg
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCaps {
    pub working_copy: bool,
    pub process_reaped: bool,
    pub kernel_isolation: bool,
    pub network_isolation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCommand {
    pub command: String,
    pub cwd: Option<PathBuf>,
}

impl SandboxCommand {
    pub fn shell(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit: Option<i32>,
    pub workdir_hash: String,
}

impl SandboxOutput {
    pub fn combined_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&String::from_utf8_lossy(&self.stdout));
        let stderr = String::from_utf8_lossy(&self.stderr);
        if !stderr.is_empty() {
            out.push_str("\n[stderr]\n");
            out.push_str(&stderr);
        }
        if self.exit != Some(0) {
            out.push_str(&format!("\n[exit code: {}]", self.exit.unwrap_or(-1)));
        }
        out
    }
}

#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    cleanup: bool,
}

impl WorkDir {
    fn borrowed(path: PathBuf) -> Self {
        Self {
            path,
            cleanup: false,
        }
    }

    fn ephemeral(path: PathBuf) -> Self {
        Self {
            path,
            cleanup: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn run(&self, cmd: SandboxCommand, ctx: &Ctx) -> Result<SandboxOutput, ToolError>;
    fn materialize(&self, checkout: &Path) -> Result<WorkDir, ToolError>;
    fn capabilities(&self) -> SandboxCaps;
}

#[derive(Debug, Default)]
pub struct NoSandbox;

#[async_trait]
impl Sandbox for NoSandbox {
    async fn run(&self, cmd: SandboxCommand, ctx: &Ctx) -> Result<SandboxOutput, ToolError> {
        run_shell(cmd, ctx, None).await
    }

    fn materialize(&self, checkout: &Path) -> Result<WorkDir, ToolError> {
        Ok(WorkDir::borrowed(checkout.to_path_buf()))
    }

    fn capabilities(&self) -> SandboxCaps {
        SandboxCaps {
            working_copy: false,
            process_reaped: true,
            kernel_isolation: false,
            network_isolation: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkingCopySandbox {
    checkout: PathBuf,
}

impl WorkingCopySandbox {
    pub fn new(checkout: impl Into<PathBuf>) -> Self {
        Self {
            checkout: checkout.into(),
        }
    }
}

#[async_trait]
impl Sandbox for WorkingCopySandbox {
    async fn run(&self, cmd: SandboxCommand, ctx: &Ctx) -> Result<SandboxOutput, ToolError> {
        let workdir = self.materialize(&self.checkout)?;
        let cwd = cmd
            .cwd
            .clone()
            .unwrap_or_else(|| workdir.path().to_path_buf());
        let mut output = run_shell(
            SandboxCommand {
                command: cmd.command,
                cwd: Some(cwd),
            },
            ctx,
            Some(workdir.path()),
        )
        .await?;
        output.workdir_hash = hash_workdir(workdir.path())?;
        Ok(output)
    }

    fn materialize(&self, checkout: &Path) -> Result<WorkDir, ToolError> {
        let root = unique_workdir_path();
        std::fs::create_dir_all(&root).map_err(io_error)?;
        copy_tree(checkout, &root).map_err(io_error)?;
        Ok(WorkDir::ephemeral(root))
    }

    fn capabilities(&self) -> SandboxCaps {
        SandboxCaps {
            working_copy: true,
            process_reaped: true,
            kernel_isolation: false,
            network_isolation: false,
        }
    }
}

pub fn sandbox_from_config(
    cfg: &SandboxConfig,
    default_checkout: impl AsRef<Path>,
) -> Arc<dyn Sandbox> {
    match cfg.mode {
        SandboxMode::NoSandbox => Arc::new(NoSandbox),
        SandboxMode::WorkingCopy => Arc::new(WorkingCopySandbox::new(
            cfg.checkout
                .clone()
                .unwrap_or_else(|| default_checkout.as_ref().to_path_buf()),
        )),
    }
}

pub fn hash_workdir(root: &Path) -> Result<String, ToolError> {
    let mut entries = Vec::new();
    collect_entries(root, root, &mut entries).map_err(io_error)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, path) in entries {
        hasher.update(rel.as_bytes());
        let meta = std::fs::symlink_metadata(&path).map_err(io_error)?;
        if meta.file_type().is_symlink() {
            hasher.update(b"symlink");
            hasher.update(
                std::fs::read_link(&path)
                    .map_err(io_error)?
                    .to_string_lossy()
                    .as_bytes(),
            );
        } else if meta.is_file() {
            hasher.update(b"file");
            hasher.update(std::fs::read(&path).map_err(io_error)?);
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn run_shell(
    cmd: SandboxCommand,
    ctx: &Ctx,
    hash_root: Option<&Path>,
) -> Result<SandboxOutput, ToolError> {
    let mut command = TokioCommand::new("sh");
    command
        .arg("-c")
        .arg(&cmd.command)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(cwd) = &cmd.cwd {
        command.current_dir(cwd);
    }
    let child = command
        .spawn()
        .map_err(|e| ToolError::Execution(format!("spawn failed: {e}")))?;

    let deadline = ctx.remaining();
    let token = ctx.cancel_token();
    let output = tokio::select! {
        _ = token.cancelled() => return Err(ToolError::Cancelled),
        _ = sleep_opt(deadline) => return Err(ToolError::Cancelled),
        res = child.wait_with_output() => {
            res.map_err(|e| ToolError::Execution(e.to_string()))?
        }
    };

    let workdir_hash = match hash_root {
        Some(root) => hash_workdir(root)?,
        None => String::new(),
    };
    Ok(SandboxOutput {
        stdout: output.stdout,
        stderr: output.stderr,
        exit: output.status.code(),
        workdir_hash,
    })
}

fn unique_workdir_path() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = WORKDIR_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "truss-working-copy-{}-{nanos:x}-{n:x}",
        std::process::id()
    ))
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&src_path)?;
        if meta.file_type().is_symlink() {
            copy_symlink(&src_path, &dst_path)?;
        } else if meta.is_dir() {
            std::fs::create_dir_all(&dst_path)?;
            copy_tree(&src_path, &dst_path)?;
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, dst: &Path) -> std::io::Result<()> {
    let target = std::fs::read_link(src)?;
    let target_path = if target.is_absolute() {
        target
    } else {
        src.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
    std::fs::copy(target_path, dst).map(|_| ())
}

fn collect_entries(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            collect_entries(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push((rel, path));
        }
    }
    Ok(())
}

fn io_error(e: std::io::Error) -> ToolError {
    ToolError::Execution(e.to_string())
}

async fn sleep_opt(dur: Option<std::time::Duration>) {
    match dur {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending::<()>().await,
    }
}
