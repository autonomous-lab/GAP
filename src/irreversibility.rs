//! Irreversibility & cooling-off (RFC-0009).
//!
//! Classifies action irreversibility, mandates cooling-off windows for
//! consequential classes, and defines the Withdrawal Receipt that
//! proves no irreversible execution occurred.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// Irreversibility classes with default cooling-off windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IrreversibilityClass {
    /// Reversible: draft, query, analysis.
    Reversible,
    /// External side effect: send email, publish post.
    External,
    /// Financial transfer above threshold.
    Financial,
    /// Legal filings, registrations.
    Legal,
    /// Medical / health data.
    Medical,
}

impl IrreversibilityClass {
    /// Default cooling-off window in seconds.
    pub fn default_cooling_off(&self) -> u64 {
        match self {
            IrreversibilityClass::Reversible => 0,
            IrreversibilityClass::External => 0,
            IrreversibilityClass::Financial => 3600, // 1h
            IrreversibilityClass::Legal => 86400,    // 24h
            IrreversibilityClass::Medical => 86400,  // 24h
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            IrreversibilityClass::Reversible => "reversible",
            IrreversibilityClass::External => "external",
            IrreversibilityClass::Financial => "financial",
            IrreversibilityClass::Legal => "legal",
            IrreversibilityClass::Medical => "medical",
        }
    }
}

/// A cooling-off timer for a pending irreversible action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoolingOffTimer {
    pub pending_id: String,
    pub class: IrreversibilityClass,
    pub started_at: u64,
    /// Duration in seconds (≥ class default).
    pub duration_secs: u64,
}

impl CoolingOffTimer {
    /// Create a timer for an action; enforces the class minimum.
    pub fn start(
        class: IrreversibilityClass,
        started_at: u64,
        declared_duration: Option<u64>,
    ) -> Self {
        let minimum = class.default_cooling_off();
        let duration = declared_duration.map(|d| d.max(minimum)).unwrap_or(minimum);
        Self {
            pending_id: crate::new_id("pend"),
            class,
            started_at,
            duration_secs: duration,
        }
    }

    /// When the window elapses.
    pub fn deadline(&self) -> u64 {
        self.started_at.saturating_add(self.duration_secs)
    }

    /// Whether the window has elapsed at `now`.
    pub fn elapsed(&self, now: u64) -> bool {
        now >= self.deadline()
    }

    /// Remaining seconds at `now` (0 if elapsed).
    pub fn remaining(&self, now: u64) -> u64 {
        self.deadline().saturating_sub(now)
    }
}

/// A signed receipt type for the cooling-off flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoolingOffReceipt {
    pub receipt_id: String,
    /// "irreversible_pending" | "irreversible_withdrawn" | "irreversible_executed"
    pub kind: String,
    pub pending_id: String,
    pub class: IrreversibilityClass,
    pub action_ref: String,
    pub at: u64,
    pub signed_by: Did,
    #[serde(default)]
    pub sig: Option<String>,
}

impl CoolingOffReceipt {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("receipt serializes");
        serde_json::to_vec(&v).expect("receipt serializes")
    }

    pub fn sign(mut self, signer: &AgentIdentity) -> Self {
        self.sig = Some(signer.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.signed_by,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// A manager for one pending irreversible action.
#[derive(Debug, Clone)]
pub struct IrreversibilityGuard {
    timer: CoolingOffTimer,
    /// Whether the principal withdrew consent.
    withdrawn: bool,
    /// Whether execution already happened.
    executed: bool,
}

impl IrreversibilityGuard {
    /// Begin the cooling-off flow for an irreversible action.
    pub fn begin(
        class: IrreversibilityClass,
        action_ref: &str,
        now: u64,
        declared_duration: Option<u64>,
        signer: &AgentIdentity,
    ) -> (Self, CoolingOffReceipt) {
        let timer = CoolingOffTimer::start(class, now, declared_duration);
        let receipt = CoolingOffReceipt {
            receipt_id: crate::new_id("rcpt"),
            kind: "irreversible_pending".into(),
            pending_id: timer.pending_id.clone(),
            class,
            action_ref: action_ref.into(),
            at: now,
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(signer);
        (
            Self {
                timer,
                withdrawn: false,
                executed: false,
            },
            receipt,
        )
    }

    /// The principal withdraws within the window.
    /// Returns a Withdrawal Receipt proving nothing executed.
    pub fn withdraw(&mut self, now: u64, signer: &AgentIdentity) -> Result<CoolingOffReceipt> {
        if self.executed {
            return Err(Error::Other(
                "cannot withdraw: action already executed".into(),
            ));
        }
        if self.timer.elapsed(now) {
            return Err(Error::Other(
                "cannot withdraw: cooling-off window elapsed".into(),
            ));
        }
        self.withdrawn = true;
        Ok(CoolingOffReceipt {
            receipt_id: crate::new_id("rcpt"),
            kind: "irreversible_withdrawn".into(),
            pending_id: self.timer.pending_id.clone(),
            class: self.timer.class,
            action_ref: String::new(),
            at: now,
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(signer))
    }

    /// Execute the action — only allowed after the window elapses.
    pub fn execute(&mut self, now: u64, signer: &AgentIdentity) -> Result<CoolingOffReceipt> {
        if self.withdrawn {
            return Err(Error::Other("cannot execute: consent was withdrawn".into()));
        }
        if !self.timer.elapsed(now) {
            return Err(Error::Other(format!(
                "cannot execute: cooling-off window not elapsed ({}s remaining)",
                self.timer.remaining(now)
            )));
        }
        self.executed = true;
        Ok(CoolingOffReceipt {
            receipt_id: crate::new_id("rcpt"),
            kind: "irreversible_executed".into(),
            pending_id: self.timer.pending_id.clone(),
            class: self.timer.class,
            action_ref: String::new(),
            at: now,
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(signer))
    }

    pub fn withdrawn(&self) -> bool {
        self.withdrawn
    }

    pub fn executed(&self) -> bool {
        self.executed
    }

    pub fn timer(&self) -> &CoolingOffTimer {
        &self.timer
    }
}

/// A bounded waiver of the cooling-off window (per class, per provider,
/// expires ≤ 30 days).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoolingOffWaiver {
    pub waiver_id: String,
    pub class: IrreversibilityClass,
    pub provider: Did,
    pub granted_by: Did,
    pub expires_at: u64,
    #[serde(default)]
    pub sig: Option<String>,
}

impl CoolingOffWaiver {
    /// Grant a waiver. The expiry MUST be ≤ 30 days from now.
    pub fn grant(
        principal: &AgentIdentity,
        class: IrreversibilityClass,
        provider: Did,
        now: u64,
        duration_secs: u64,
    ) -> Result<Self> {
        const MAX_WAIVER: u64 = 30 * 86400;
        if duration_secs > MAX_WAIVER {
            return Err(Error::Other(format!(
                "waiver exceeds maximum of {}s",
                MAX_WAIVER
            )));
        }
        let mut w = Self {
            waiver_id: crate::new_id("wvr"),
            class,
            provider,
            granted_by: principal.did().clone(),
            expires_at: now + duration_secs,
            sig: None,
        };
        let canonical = {
            let mut clone = w.clone();
            clone.sig = None;
            let v = serde_json::to_value(&clone).expect("waiver serializes");
            serde_json::to_vec(&v).expect("waiver serializes")
        };
        w.sig = Some(principal.sign(&canonical).to_hex());
        Ok(w)
    }

    pub fn is_valid_at(&self, now: u64) -> bool {
        now < self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::now_unix;

    #[test]
    fn class_defaults_are_sane() {
        assert_eq!(IrreversibilityClass::Reversible.default_cooling_off(), 0);
        assert_eq!(IrreversibilityClass::External.default_cooling_off(), 0);
        assert_eq!(IrreversibilityClass::Financial.default_cooling_off(), 3600);
        assert_eq!(IrreversibilityClass::Legal.default_cooling_off(), 86400);
        assert_eq!(IrreversibilityClass::Medical.default_cooling_off(), 86400);
    }

    #[test]
    fn window_enforces_class_minimum() {
        // Declared 10s but financial requires 3600s -> 3600s.
        let t = CoolingOffTimer::start(IrreversibilityClass::Financial, 1000, Some(10));
        assert_eq!(t.duration_secs, 3600);
        // Reversible allows 0.
        let t2 = CoolingOffTimer::start(IrreversibilityClass::Reversible, 1000, None);
        assert_eq!(t2.duration_secs, 0);
        assert!(t2.elapsed(1000));
    }

    #[test]
    fn guard_blocks_early_execution() {
        let signer = AgentIdentity::generate();
        let now = now_unix();
        let (mut guard, pending) = IrreversibilityGuard::begin(
            IrreversibilityClass::Financial,
            "urn:gap:action:transfer-1",
            now,
            None,
            &signer,
        );
        assert_eq!(pending.kind, "irreversible_pending");
        assert!(pending.verify().is_ok());
        // Early execution fails.
        assert!(guard.execute(now + 60, &signer).is_err());
        // Withdrawal within window works.
        let withdrawn = guard.withdraw(now + 60, &signer).unwrap();
        assert_eq!(withdrawn.kind, "irreversible_withdrawn");
        assert!(withdrawn.verify().is_ok());
        assert!(guard.withdrawn());
        // Execute after withdrawal fails.
        assert!(guard.execute(now + 99999, &signer).is_err());
    }

    #[test]
    fn guard_executes_after_window() {
        let signer = AgentIdentity::generate();
        let now = now_unix();
        let (mut guard, _) = IrreversibilityGuard::begin(
            IrreversibilityClass::Financial,
            "urn:gap:action:transfer-2",
            now,
            None,
            &signer,
        );
        // After the 1h window.
        let executed = guard.execute(now + 4000, &signer).unwrap();
        assert_eq!(executed.kind, "irreversible_executed");
        assert!(guard.executed());
        // Cannot withdraw after execution.
        assert!(guard.withdraw(now + 5000, &signer).is_err());
    }

    #[test]
    fn waiver_is_bounded_and_expires() {
        let principal = AgentIdentity::generate();
        let provider = AgentIdentity::generate();
        let now = now_unix();
        // 10 days: OK.
        let w = CoolingOffWaiver::grant(
            &principal,
            IrreversibilityClass::Financial,
            provider.did().clone(),
            now,
            10 * 86400,
        )
        .unwrap();
        assert!(w.is_valid_at(now + 5 * 86400));
        assert!(!w.is_valid_at(now + 11 * 86400));
        // 31 days: rejected.
        assert!(CoolingOffWaiver::grant(
            &principal,
            IrreversibilityClass::Financial,
            provider.did().clone(),
            now,
            31 * 86400,
        )
        .is_err());
    }

    #[test]
    fn receipt_signature_detects_tampering() {
        let signer = AgentIdentity::generate();
        let mut r = CoolingOffReceipt {
            receipt_id: crate::new_id("rcpt"),
            kind: "irreversible_pending".into(),
            pending_id: crate::new_id("pend"),
            class: IrreversibilityClass::Legal,
            action_ref: "urn:gap:action:legal-1".into(),
            at: now_unix(),
            signed_by: signer.did().clone(),
            sig: None,
        }
        .sign(&signer);
        assert!(r.verify().is_ok());
        r.kind = "irreversible_executed".into(); // tamper
        assert!(r.verify().is_err());
    }
}
