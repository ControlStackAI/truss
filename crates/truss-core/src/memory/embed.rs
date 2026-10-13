//! Embedding seam for vector-backed memory recall.
//!
//! The core crate owns only the trait and hermetic deterministic implementation.
//! Real provider-backed embedders live outside core so the runtime boundary stays
//! vendor-neutral.

use anyhow::Result;
use sha2::{Digest, Sha256};

/// Synchronous text embedder used by the SQLite memory backend.
///
/// Store writes are synchronous today because the memory substrate is backed by
/// rusqlite. API-backed implementations should return configuration/network
/// errors cleanly rather than panicking.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn dims(&self) -> usize;
    fn id(&self) -> &str;
}

/// Hermetic feature-hashing embedder for tests and offline development.
#[derive(Debug, Clone)]
pub struct DeterministicEmbedder {
    dims: usize,
    id: String,
}

impl DeterministicEmbedder {
    pub fn new(dims: usize) -> Self {
        let dims = dims.max(1);
        Self {
            dims,
            id: format!("deterministic-hash-v1:{dims}"),
        }
    }
}

impl Default for DeterministicEmbedder {
    fn default() -> Self {
        Self::new(64)
    }
}

impl Embedder for DeterministicEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut vector = vec![0.0; self.dims];
        for token in normalized_tokens(text) {
            let digest = Sha256::digest(token.as_bytes());
            let mut bucket_bytes = [0u8; 8];
            bucket_bytes.copy_from_slice(&digest[..8]);
            let bucket = (u64::from_le_bytes(bucket_bytes) as usize) % self.dims;
            let sign = if digest[8] & 1 == 0 { 1.0 } else { -1.0 };
            vector[bucket] += sign;
        }
        normalize(&mut vector);
        Ok(vector)
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn id(&self) -> &str {
        &self.id
    }
}

fn normalized_tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
}

fn normalize(vector: &mut [f32]) {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector {
            *value /= norm;
        }
    }
}
