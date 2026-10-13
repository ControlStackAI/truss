//! L0 agent identity — split into operator-signed `core` and self-amendable `self`.
//!
//! This is the "agent identity, done better than markdown injection" layer. It
//! sits at the very front of the cached `system` prefix and **never churns
//! within a run** (amendments commit at run boundaries), so memory writes can't
//! invalidate the most expensive cache.
//!
//! The split is load-bearing: it's what makes `IdentityPolicy::SelfAmend` safe
//! to ship. The agent evolves `self` (voice, learned preferences, working
//! persona); it can never touch `core` (boundaries, safety, mandate). See
//! `docs/memory-identity-architecture.md` §2 + §3.5.
//!
//! Not to be confused with [`crate::identity`], which is the ed25519 *signing*
//! key. `core` is signed *by* that key (operator authority); `self` is not.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::policy::IdentityPolicy;
use super::types::LedgerRef;
use crate::identity::{verify_sig, Signer};

#[derive(Debug, Error)]
pub enum PersonaError {
    #[error("persona core signature mismatch")]
    CoreSignatureMismatch,
    #[error("identity amendment signature mismatch")]
    AmendmentSignatureMismatch,
    #[error("identity amendment targets immutable core")]
    CoreAmendmentDenied,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Operator-signed identity: boundaries, safety constraints, mandate, immutable
/// values. **Never self-amendable.** Editing requires the operator signing key
/// ([`crate::identity::Signer`]); the signature is recorded so any drift is
/// detectable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaCore {
    pub body: String,
    /// Hex ed25519 signature over `body` by the operator key (`pubkey_id`).
    pub signature: String,
    pub pubkey_id: String,
}

impl PersonaCore {
    pub fn signed(body: impl Into<String>, signer: &Signer) -> Self {
        let body = body.into();
        Self {
            signature: hex::encode(signer.sign(body.as_bytes())),
            pubkey_id: signer.public_hex(),
            body,
        }
    }

    pub fn verify(&self) -> bool {
        let Ok(sig) = hex::decode(&self.signature) else {
            return false;
        };
        verify_sig(&self.pubkey_id, self.body.as_bytes(), &sig)
    }
}

/// Self-amendable identity: voice, tone, learned preferences, working persona.
/// Mutated only at run boundaries, only via a deliberate signed amendment, only
/// under the active [`IdentityPolicy`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersonaSelf {
    pub body: String,
    /// Journal ref of the amendment that produced the current `body` (provenance).
    pub last_amended_by: Option<LedgerRef>,
    /// Monotonic version for diff/rollback.
    pub version: u32,
}

/// The assembled L0 layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub core: PersonaCore,
    #[serde(rename = "self")]
    pub self_: PersonaSelf,
}

impl Persona {
    /// Render the L0 system-prefix block. Core first (frozen, authoritative),
    /// then self. Stable across a run so the cache prefix holds.
    pub fn render(&self) -> String {
        format!("{}\n\n{}", self.core.body, self.self_.body)
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self, PersonaError> {
        let text = fs::read_to_string(path)?;
        let persona: Persona = serde_json::from_str(&text)?;
        if !persona.core.verify() {
            return Err(PersonaError::CoreSignatureMismatch);
        }
        Ok(persona)
    }

    pub fn persist(&self, path: impl AsRef<Path>) -> Result<(), PersonaError> {
        if !self.core.verify() {
            return Err(PersonaError::CoreSignatureMismatch);
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmendmentTarget {
    Core,
    SelfLayer,
}

/// A proposed change to `self` (never `core`). Buffered, not applied in-run:
/// [`Persona`] commits it at the next session boundary so the cached prefix is
/// never rewritten mid-run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Amendment {
    #[serde(default = "default_amendment_target")]
    pub target: AmendmentTarget,
    /// New `self` body the agent proposes.
    pub new_body: String,
    /// The deliberate signed tool call that requested it (provenance; also the
    /// guard against injection — only a real tool call can mint one).
    pub proposed_by: LedgerRef,
    /// Public key that signed the deliberate `amend_identity` call.
    pub signer_pubkey_id: String,
    /// Hex ed25519 signature over [`Amendment::signing_payload`].
    pub signature: String,
}

fn default_amendment_target() -> AmendmentTarget {
    AmendmentTarget::SelfLayer
}

impl Amendment {
    pub fn for_self(new_body: impl Into<String>, proposed_by: LedgerRef, signer: &Signer) -> Self {
        let mut amendment = Self {
            target: AmendmentTarget::SelfLayer,
            new_body: new_body.into(),
            proposed_by,
            signer_pubkey_id: signer.public_hex(),
            signature: String::new(),
        };
        amendment.signature = hex::encode(signer.sign(amendment.signing_payload().as_bytes()));
        amendment
    }

    pub fn signing_payload(&self) -> String {
        format!(
            "amend_identity:v1:{:?}:{}:{}:{}",
            self.target, self.proposed_by.session, self.proposed_by.seq, self.new_body
        )
    }

    pub fn verify_signature(&self) -> bool {
        let Ok(sig) = hex::decode(&self.signature) else {
            return false;
        };
        verify_sig(
            &self.signer_pubkey_id,
            self.signing_payload().as_bytes(),
            &sig,
        )
    }
}

/// Outcome of routing an [`Amendment`] through the active policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmendmentDecision {
    /// Commit at the next run boundary (buffered, not applied now).
    CommitAtBoundary,
    /// Needs operator approval or a passing verdict first.
    NeedsApproval,
    /// Rejected — e.g. the amendment tried to touch `core`, or verdict-check
    /// against `core` failed.
    Denied(&'static str),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityJournalEvent {
    pub prior_body: String,
    pub new_body: String,
    pub prior_version: u32,
    pub new_version: u32,
    pub committed_by: LedgerRef,
}

impl Persona {
    /// Decide what happens to an amendment under `policy`. This does **not**
    /// mutate — the loop buffers a `CommitAtBoundary` decision and applies it on
    /// the next session build, recording a reversible ledger event. Even under
    /// `SelfAmend`, the amendment should first be verdict-checked against `core`
    /// (an identity edit is a high-stakes memory write → same N-reviewer path).
    pub fn route_amendment(
        &self,
        amendment: &Amendment,
        policy: IdentityPolicy,
        core_verdict_passed: bool,
    ) -> AmendmentDecision {
        if amendment.target == AmendmentTarget::Core {
            return AmendmentDecision::Denied("core amendments are denied");
        }
        if !amendment.verify_signature() {
            return AmendmentDecision::Denied("amend_identity signature failed");
        }
        if !core_verdict_passed {
            return AmendmentDecision::Denied("amendment failed core verdict check");
        }
        match policy {
            IdentityPolicy::Locked => AmendmentDecision::NeedsApproval,
            IdentityPolicy::Propose => AmendmentDecision::NeedsApproval,
            IdentityPolicy::SelfAmend => AmendmentDecision::CommitAtBoundary,
        }
    }

    pub fn commit_amendment(
        &mut self,
        amendment: &Amendment,
        committed_by: LedgerRef,
    ) -> Result<IdentityJournalEvent, PersonaError> {
        if amendment.target == AmendmentTarget::Core {
            return Err(PersonaError::CoreAmendmentDenied);
        }
        if !amendment.verify_signature() {
            return Err(PersonaError::AmendmentSignatureMismatch);
        }
        let event = IdentityJournalEvent {
            prior_body: self.self_.body.clone(),
            new_body: amendment.new_body.clone(),
            prior_version: self.self_.version,
            new_version: self.self_.version + 1,
            committed_by: committed_by.clone(),
        };
        self.self_.body = amendment.new_body.clone();
        self.self_.version = event.new_version;
        self.self_.last_amended_by = Some(committed_by);
        Ok(event)
    }

    pub fn rollback(&mut self, event: &IdentityJournalEvent) {
        self.self_.body = event.prior_body.clone();
        self.self_.version = event.prior_version;
        self.self_.last_amended_by = None;
    }
}
