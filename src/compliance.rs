//! Compliance context (RFC-0006).
//!
//! A signed, versioned set of obligations on a principal/scope: NDAs,
//! embargo lists, Chinese walls, sanctions screening, professional
//! codes. Evaluated before every data-sharing action via the
//! pre-action gate.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// A non-disclosure agreement covering data classes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nda {
    pub id: String,
    pub counterparties: Vec<Did>,
    pub covered_classes: Vec<String>,
    pub valid_until: u64,
    pub jurisdiction: String,
    #[serde(default)]
    pub document_hash: Option<String>,
}

/// A Chinese wall isolating two scopes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChineseWall {
    pub between: Vec<String>,
    pub reason: String,
}

/// The signed compliance context of a scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplianceContext {
    pub context_id: String,
    pub scope_id: String,
    pub subject: Did,
    #[serde(default)]
    pub active_ndas: Vec<Nda>,
    #[serde(default)]
    pub embargo_list: Vec<Did>,
    #[serde(default)]
    pub chinese_walls: Vec<ChineseWall>,
    #[serde(default)]
    pub sanctions_screening: Option<String>,
    #[serde(default)]
    pub professional_codes: Vec<String>,
    #[serde(default)]
    pub export_control: Option<String>,
    pub signed_at: u64,
    #[serde(default)]
    pub sig: Option<String>,
}

/// The outcome of a pre-action gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    Allow,
    Deny,
    Escalate,
}

/// A signed gate verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateVerdict {
    pub verdict_id: String,
    pub decision: GateDecision,
    pub reasons: Vec<String>,
    pub evaluated_at: u64,
    pub evaluated_by: Did,
    #[serde(default)]
    pub sig: Option<String>,
}

impl GateVerdict {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("verdict serializes");
        serde_json::to_vec(&v).expect("verdict serializes")
    }

    pub fn sign(mut self, evaluator: &AgentIdentity) -> Self {
        self.sig = Some(evaluator.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.evaluated_by,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

impl ComplianceContext {
    /// Create and sign a context.
    pub fn create(
        subject: &AgentIdentity,
        scope_id: &str,
        ndas: Vec<Nda>,
        embargo_list: Vec<Did>,
        chinese_walls: Vec<ChineseWall>,
    ) -> Self {
        let mut c = Self {
            context_id: crate::new_id("ccc"),
            scope_id: scope_id.into(),
            subject: subject.did().clone(),
            active_ndas: ndas,
            embargo_list,
            chinese_walls,
            sanctions_screening: Some("ofac_eu_un".into()),
            professional_codes: vec![],
            export_control: Some("none".into()),
            signed_at: crate::message::now_unix(),
            sig: None,
        };
        c.resign(subject);
        c
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, subject: &AgentIdentity) {
        self.sig = None;
        let canonical = self.canonical_bytes();
        self.sig = Some(subject.sign(&canonical).to_hex());
    }

    /// Verify the subject signature.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.subject,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("context serializes");
        serde_json::to_vec(&v).expect("context serializes")
    }

    /// Is this context currently valid at `now`?
    pub fn is_valid_at(&self, now: u64) -> bool {
        self.active_ndas.iter().any(|n| n.valid_until > now)
            || !self.embargo_list.is_empty()
            || !self.chinese_walls.is_empty()
    }

    /// Is the destination embargoed?
    pub fn is_embargoed(&self, destination: &Did) -> bool {
        self.embargo_list.contains(destination)
    }

    /// Does an active NDA cover these data classes with this
    /// counterparty at `now`?
    pub fn nda_covers(&self, counterparty: &Did, classes: &[String], now: u64) -> bool {
        self.active_ndas.iter().any(|nda| {
            nda.valid_until > now
                && nda.counterparties.contains(counterparty)
                && classes.iter().all(|c| nda.covered_classes.contains(c))
        })
    }

    /// Is the destination scope isolated from the source scope by a
    /// Chinese wall?
    pub fn is_walled(&self, source_scope: &str, dest_scope: &str) -> bool {
        self.chinese_walls.iter().any(|w| {
            w.between.contains(&source_scope.to_string())
                && w.between.contains(&dest_scope.to_string())
        })
    }
}

/// The pre-action gate: evaluates a data-sharing action against the
/// context, in normative order. Returns a signed verdict.
pub fn gate(
    context: &ComplianceContext,
    destination: &Did,
    data_classes: &[String],
    source_scope: &str,
    dest_scope: &str,
    now: u64,
    evaluator: &AgentIdentity,
) -> GateVerdict {
    let mut reasons: Vec<String> = vec![];
    let decision: GateDecision;

    // 1. Embargo
    if context.is_embargoed(destination) {
        reasons.push("destination on embargo list".into());
        decision = GateDecision::Deny;
    // 2. Chinese wall
    } else if context.is_walled(source_scope, dest_scope) {
        reasons.push(format!("Chinese wall between {source_scope} and {dest_scope}"));
        decision = GateDecision::Deny;
    // 3. NDA coverage
    } else if !context.nda_covers(destination, data_classes, now) {
        if data_classes.is_empty() {
            reasons.push("no data classes declared".into());
            decision = GateDecision::Allow;
        } else {
            reasons.push("no active NDA covers the data classes".into());
            decision = GateDecision::Deny;
        }
    } else {
        reasons.push("all gates passed".into());
        decision = GateDecision::Allow;
    }

    GateVerdict {
        verdict_id: crate::new_id("ver"),
        decision,
        reasons,
        evaluated_at: now,
        evaluated_by: evaluator.did().clone(),
        sig: None,
    }
    .sign(evaluator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::now_unix;

    fn context(ndas: Vec<Nda>, embargo: Vec<Did>, walls: Vec<ChineseWall>) -> ComplianceContext {
        ComplianceContext::create(
            &AgentIdentity::generate(),
            "scope:consulting:clientA",
            ndas,
            embargo,
            walls,
        )
    }

    fn nda(counterparty: Did, classes: Vec<&str>) -> Nda {
        Nda {
            id: "nda:2026:clientA".into(),
            counterparties: vec![counterparty],
            covered_classes: classes.into_iter().map(String::from).collect(),
            valid_until: now_unix() + 86400,
            jurisdiction: "FR".into(),
            document_hash: None,
        }
    }

    #[test]
    fn embargo_denies() {
        let evaluator = AgentIdentity::generate();
        let evil = Did::parse(&AgentIdentity::generate().did().to_string()).unwrap();
        let ctx = context(vec![], vec![evil.clone()], vec![]);
        let verdict = gate(&ctx, &evil, &[], "a", "b", now_unix(), &evaluator);
        assert_eq!(verdict.decision, GateDecision::Deny);
        assert!(verdict.verify().is_ok());
    }

    #[test]
    fn chinese_wall_denies() {
        let evaluator = AgentIdentity::generate();
        let dest = Did::parse(&AgentIdentity::generate().did().to_string()).unwrap();
        let ctx = context(
            vec![],
            vec![],
            vec![ChineseWall {
                between: vec!["scope:consulting:clientA".into(), "scope:consulting:clientB".into()],
                reason: "competing_clients".into(),
            }],
        );
        let verdict = gate(
            &ctx,
            &dest,
            &[],
            "scope:consulting:clientA",
            "scope:consulting:clientB",
            now_unix(),
            &evaluator,
        );
        assert_eq!(verdict.decision, GateDecision::Deny);
    }

    #[test]
    fn nda_coverage_allows_and_denies() {
        let evaluator = AgentIdentity::generate();
        let counterparty = Did::parse(&AgentIdentity::generate().did().to_string()).unwrap();
        let ctx = context(
            vec![nda(counterparty.clone(), vec!["financials", "strategy"])],
            vec![],
            vec![],
        );
        let classes: Vec<String> = vec!["financials".into(), "strategy".into()];
        // Covered -> allow.
        let verdict = gate(&ctx, &counterparty, &classes, "a", "b", now_unix(), &evaluator);
        assert_eq!(verdict.decision, GateDecision::Allow);
        // Uncovered class -> deny.
        let uncovered: Vec<String> = vec!["personnel".into()];
        let verdict2 = gate(&ctx, &counterparty, &uncovered, "a", "b", now_unix(), &evaluator);
        assert_eq!(verdict2.decision, GateDecision::Deny);
    }

    #[test]
    fn expired_nda_does_not_cover() {
        let evaluator = AgentIdentity::generate();
        let counterparty = Did::parse(&AgentIdentity::generate().did().to_string()).unwrap();
        let mut n = nda(counterparty.clone(), vec!["financials"]);
        n.valid_until = now_unix() - 100; // expired
        let ctx = context(vec![n], vec![], vec![]);
        let classes: Vec<String> = vec!["financials".into()];
        let verdict = gate(&ctx, &counterparty, &classes, "a", "b", now_unix(), &evaluator);
        assert_eq!(verdict.decision, GateDecision::Deny);
    }

    #[test]
    fn context_signature_detects_tampering() {
        let subject = AgentIdentity::generate();
        let mut ctx = ComplianceContext::create(
            &subject,
            "scope:x",
            vec![],
            vec![],
            vec![],
        );
        assert!(ctx.verify().is_ok());
        ctx.scope_id = "scope:evil".into();
        assert!(ctx.verify().is_err());
    }

    #[test]
    fn empty_context_validity() {
        let subject = AgentIdentity::generate();
        let ctx = ComplianceContext::create(&subject, "scope:x", vec![], vec![], vec![]);
        // No NDA, no embargo, no walls -> validity depends on content.
        assert!(!ctx.is_valid_at(now_unix()));
        assert!(!ctx.is_embargoed(&subject.did().clone()));
    }
}
