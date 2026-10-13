//! Tool permission policy — enforced, not advisory.
//!
//! This is the "executor never gets to do whatever it wants" boundary. Tools
//! consult the [`Policy`] on the [`crate::ctx::Ctx`] before touching the
//! filesystem, network, or shell. A denial returns
//! [`crate::error::ToolError::PermissionDenied`], which is fed back to the model
//! so it can adapt rather than crash.
//!
//! The default policy is **deny-writes, allow-reads, no-network** — safe by
//! default; widen explicitly via config or the builder.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum NetworkPolicy {
    /// No network access for tools.
    None,
    /// Only these hosts (exact or suffix match) are reachable.
    Allowlist(Vec<String>),
    /// Unrestricted.
    Open,
}

#[derive(Debug, Clone)]
pub struct Policy {
    /// Directories the agent may write under. Empty = no writes allowed.
    pub write_scope: Vec<PathBuf>,
    /// Directories the agent may read under. `None` = read anywhere.
    pub read_scope: Option<Vec<PathBuf>>,
    pub network: NetworkPolicy,
    /// Substrings that, if present in a shell command, are rejected outright.
    pub forbidden_substrings: Vec<String>,
    /// Tools the agent may call. `None` = all registered tools allowed.
    pub allow_tools: Option<HashSet<String>>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            write_scope: Vec::new(),
            read_scope: None,
            network: NetworkPolicy::None,
            forbidden_substrings: vec!["rm -rf /".into(), ":(){".into()],
            allow_tools: None,
        }
    }
}

impl Policy {
    /// A permissive policy scoped to a single working directory (reads + writes
    /// confined to `root`, no network). Handy for CLI runs.
    pub fn workdir(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Policy {
            write_scope: vec![root.clone()],
            read_scope: Some(vec![root]),
            network: NetworkPolicy::None,
            ..Default::default()
        }
    }

    pub fn can_use_tool(&self, name: &str) -> bool {
        self.allow_tools
            .as_ref()
            .map(|set| set.contains(name))
            .unwrap_or(true)
    }

    pub fn can_read(&self, path: &Path) -> bool {
        match &self.read_scope {
            None => true,
            Some(scopes) => path_within_any(path, scopes),
        }
    }

    pub fn can_write(&self, path: &Path) -> bool {
        path_within_any(path, &self.write_scope)
    }

    /// Returns the offending substring if a command is forbidden.
    pub fn command_violation(&self, command: &str) -> Option<String> {
        self.forbidden_substrings
            .iter()
            .find(|bad| command.contains(bad.as_str()))
            .cloned()
    }

    pub fn host_allowed(&self, host: &str) -> bool {
        match &self.network {
            NetworkPolicy::Open => true,
            NetworkPolicy::None => false,
            NetworkPolicy::Allowlist(hosts) => hosts.iter().any(|h| host == h || host.ends_with(h)),
        }
    }
}

/// Best-effort containment check. Normalizes lexically (handles `.`/`..`) without
/// requiring the path to exist, then verifies it stays under one scope root.
fn path_within_any(path: &Path, scopes: &[PathBuf]) -> bool {
    let norm = normalize(path);
    scopes
        .iter()
        .any(|scope| norm.starts_with(normalize(scope)))
}

fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_scope_blocks_escape() {
        let p = Policy::workdir("/home/u/project");
        assert!(p.can_write(Path::new("/home/u/project/src/main.rs")));
        assert!(!p.can_write(Path::new("/home/u/project/../secrets")));
        assert!(!p.can_write(Path::new("/etc/passwd")));
    }

    #[test]
    fn default_denies_writes_and_network() {
        let p = Policy::default();
        assert!(!p.can_write(Path::new("/tmp/x")));
        assert!(!p.host_allowed("example.com"));
        assert!(p.can_read(Path::new("/anywhere")));
    }

    #[test]
    fn default_forbids_dangerous_commands() {
        let p = Policy::default();
        assert!(p.command_violation("rm -rf / now").is_some());
        assert!(p.command_violation("ls -la").is_none());
    }

    #[test]
    fn network_allowlist_matches_suffix() {
        let p = Policy {
            network: NetworkPolicy::Allowlist(vec!["github.com".into()]),
            ..Default::default()
        };
        assert!(p.host_allowed("github.com"));
        assert!(p.host_allowed("api.github.com")); // suffix match
        assert!(!p.host_allowed("evil.com"));
    }
}
