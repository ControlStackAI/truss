//! External anchoring of the ledger head (M4).
//!
//! Per `docs/durable-execution.md`, anchoring publishes a session's head hash to
//! an **external, append-only transparency log** so the audit trail can be checked
//! by a third party — without revealing any private data (only a hash leaves the
//! box). This is the seam where a real Sigstore/Rekor or on-chain anchor slots in
//! later; the default local implementation is itself a hash-chained log, so it is
//! tamper-evident in the same spirit.
//!
//! Rule: anchor a hash, never evidence data.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use sha2::{Digest, Sha256};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A proof that a head hash was published to a log at a position and time.
#[derive(Debug, Clone)]
pub struct AnchorReceipt {
    pub log: String,
    pub index: u64,
    pub head_hash_hex: String,
    pub entry_hash_hex: String,
    pub anchored_at_ms: i64,
}

/// Publishes a session head hash to some external log.
pub trait Anchor {
    fn anchor(&self, session_id: &str, head_hash: &[u8], pubkey: &str) -> Result<AnchorReceipt>;
}

/// No-op anchor (default): records nothing externally. Useful as a stand-in and
/// in tests.
pub struct NoopAnchor;

impl Anchor for NoopAnchor {
    fn anchor(&self, _session_id: &str, head_hash: &[u8], _pubkey: &str) -> Result<AnchorReceipt> {
        Ok(AnchorReceipt {
            log: "noop".into(),
            index: 0,
            head_hash_hex: hex::encode(head_hash),
            entry_hash_hex: String::new(),
            anchored_at_ms: now_ms(),
        })
    }
}

/// A local append-only transparency log (JSONL). Each line links to the previous
/// (hash chain), so the log itself is tamper-evident — the same property a real
/// transparency-log service provides via a Merkle tree. A `RekorAnchor` /
/// `ChainAnchor` implementing the same trait can replace this with no caller
/// changes.
pub struct FileTransparencyLog {
    path: String,
}

impl FileTransparencyLog {
    pub fn new(path: impl Into<String>) -> Self {
        FileTransparencyLog { path: path.into() }
    }

    fn read_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .map(|s| s.lines().map(|l| l.to_string()).collect())
            .unwrap_or_default()
    }

    /// Verify the log's internal hash chain. Returns the number of entries, or the
    /// index where the chain breaks.
    pub fn verify(&self) -> Result<std::result::Result<usize, u64>> {
        let lines = self.read_lines();
        let mut prev = String::new();
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line)?;
            let rec_prev = v["prev_hash"].as_str().unwrap_or("");
            if rec_prev != prev {
                return Ok(Err(i as u64));
            }
            let expect = chain_hash(
                &prev,
                v["session_id"].as_str().unwrap_or(""),
                v["head_hash"].as_str().unwrap_or(""),
                v["anchored_at_ms"].as_i64().unwrap_or(0),
            );
            if expect != v["entry_hash"].as_str().unwrap_or("") {
                return Ok(Err(i as u64));
            }
            prev = expect;
        }
        Ok(Ok(lines.len()))
    }
}

fn chain_hash(prev: &str, session_id: &str, head_hash_hex: &str, ts: i64) -> String {
    let mut h = Sha256::new();
    h.update(prev.as_bytes());
    h.update(session_id.as_bytes());
    h.update(head_hash_hex.as_bytes());
    h.update(ts.to_le_bytes());
    hex::encode(h.finalize())
}

impl Anchor for FileTransparencyLog {
    fn anchor(&self, session_id: &str, head_hash: &[u8], pubkey: &str) -> Result<AnchorReceipt> {
        use std::io::Write;

        let lines = self.read_lines();
        let index = lines.len() as u64;
        let prev = lines
            .last()
            .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .and_then(|v| v["entry_hash"].as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let head_hash_hex = hex::encode(head_hash);
        let anchored_at_ms = now_ms();
        let entry_hash = chain_hash(&prev, session_id, &head_hash_hex, anchored_at_ms);

        let record = serde_json::json!({
            "index": index,
            "session_id": session_id,
            "head_hash": head_hash_hex,
            "pubkey": pubkey,
            "anchored_at_ms": anchored_at_ms,
            "prev_hash": prev,
            "entry_hash": entry_hash,
        });

        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{record}")?;

        Ok(AnchorReceipt {
            log: self.path.clone(),
            index,
            head_hash_hex,
            entry_hash_hex: entry_hash,
            anchored_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_log_chains_and_verifies() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("ah-anchor-test-{}.jsonl", std::process::id()));
        let p = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&p);

        let log = FileTransparencyLog::new(p.clone());
        let r0 = log.anchor("s1", &[1u8; 32], "pk").unwrap();
        let r1 = log.anchor("s2", &[2u8; 32], "pk").unwrap();
        assert_eq!(r0.index, 0);
        assert_eq!(r1.index, 1);
        assert!(matches!(log.verify().unwrap(), Ok(2)));

        std::fs::remove_file(&p).ok();
    }
}
