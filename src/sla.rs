//! SLAs & incident reporting (RFC-0012).
//!
//! Service level commitments on capabilities (uptime, latency,
//! incident disclosure), signed incident reports, and declared-vs-
//! measured divergence tracking with conformance downgrade.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};

/// Declared SLA targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sla {
    /// Target uptime fraction, e.g. 0.999.
    pub uptime_target: f64,
    /// p95 latency in ms.
    pub latency_p95_ms: u64,
    #[serde(default)]
    pub max_call_duration_ms: Option<u64>,
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_async: bool,
    #[serde(default)]
    pub regions: Vec<String>,
    /// Incident disclosure window in hours.
    pub incident_disclosure_within_hours: u64,
    #[serde(default)]
    pub scheduled_maintenance_notice_hours: u64,
}

impl Sla {
    /// Validate the declaration is sane.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.uptime_target) {
            return Err(Error::Other("uptime target must be in [0,1]".into()));
        }
        Ok(())
    }
}

/// A measured value (from a probe or counterparty attestation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurement {
    pub measured_at: u64,
    pub uptime_observed: f64,
    pub latency_p95_ms_observed: u64,
    /// DID of the measuring party (probe service or counterparty).
    pub measured_by: Did,
    #[serde(default)]
    pub sig: Option<String>,
}

impl Measurement {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig = None;
        let v = serde_json::to_value(&clone).expect("measurement serializes");
        serde_json::to_vec(&v).expect("measurement serializes")
    }

    pub fn sign(mut self, measurer: &AgentIdentity) -> Self {
        self.sig = Some(measurer.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.measured_by,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// A signed incident report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentReport {
    pub incident_id: String,
    pub provider: Did,
    pub capabilities: Vec<String>,
    /// "minor" | "major" | "critical"
    pub severity: String,
    pub scope: String,
    pub root_cause: String,
    pub mitigation: String,
    pub affected_principals: u64,
    pub reported_at: u64,
    #[serde(default)]
    pub provider_sig: Option<String>,
    #[serde(skip)]
    sig: Option<String>,
}

impl IncidentReport {
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.provider_sig = None;
        let v = serde_json::to_value(&clone).expect("incident serializes");
        serde_json::to_vec(&v).expect("incident serializes")
    }

    pub fn sign(mut self, provider: &AgentIdentity) -> Self {
        self.sig = Some(provider.sign(&self.canonical_bytes()).to_hex());
        self
    }

    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.provider,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )
    }
}

/// Tracks declared vs measured SLA and detects divergence.
#[derive(Debug, Clone)]
pub struct SlaTracker {
    declared: Sla,
    measurements: Vec<Measurement>,
}

/// The tolerance window for divergence detection.
pub const DIVERGENCE_WINDOW_SECS: u64 = 30 * 86400; // 30 days
/// Uptime divergence threshold: measured < declared - 0.005.
pub const UPTIME_DIVERGENCE: f64 = 0.005;

impl SlaTracker {
    pub fn new(declared: Sla) -> Self {
        Self {
            declared,
            measurements: vec![],
        }
    }

    pub fn declared(&self) -> &Sla {
        &self.declared
    }

    /// Record a verified measurement.
    pub fn record(&mut self, measurement: Measurement) -> Result<()> {
        measurement.verify()?;
        self.measurements.push(measurement);
        Ok(())
    }

    /// Average observed uptime over the divergence window.
    pub fn observed_uptime(&self, now: u64) -> Option<f64> {
        let window_start = now.saturating_sub(DIVERGENCE_WINDOW_SECS);
        let relevant: Vec<&Measurement> = self
            .measurements
            .iter()
            .filter(|m| m.measured_at >= window_start)
            .collect();
        if relevant.is_empty() {
            return None;
        }
        Some(
            relevant.iter().map(|m| m.uptime_observed).sum::<f64>()
                / relevant.len() as f64,
        )
    }

    /// Whether the declared SLA materially diverges from measurements
    /// over the window.
    pub fn diverges(&self, now: u64) -> bool {
        match self.observed_uptime(now) {
            Some(observed) => observed < self.declared.uptime_target - UPTIME_DIVERGENCE,
            None => false,
        }
    }

    /// Whether an incident was disclosed within the declared window.
    pub fn disclosed_on_time(
        &self,
        incident: &IncidentReport,
        occurred_at: u64,
    ) -> bool {
        incident.reported_at.saturating_sub(occurred_at)
            <= self.declared.incident_disclosure_within_hours * 3600
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::now_unix;

    fn sla() -> Sla {
        Sla {
            uptime_target: 0.999,
            latency_p95_ms: 300,
            max_call_duration_ms: Some(30000),
            supports_streaming: true,
            supports_async: false,
            regions: vec!["eu-west".into()],
            incident_disclosure_within_hours: 72,
            scheduled_maintenance_notice_hours: 168,
        }
    }

    fn measurement(agent: &AgentIdentity, uptime: f64, at: u64) -> Measurement {
        Measurement {
            measured_at: at,
            uptime_observed: uptime,
            latency_p95_ms_observed: 250,
            measured_by: agent.did().clone(),
            sig: None,
        }
        .sign(agent)
    }

    #[test]
    fn sla_validation() {
        assert!(sla().validate().is_ok());
        let mut bad = sla();
        bad.uptime_target = 1.5;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn divergence_detection() {
        let measurer = AgentIdentity::generate();
        let now = now_unix();
        let mut tracker = SlaTracker::new(sla());
        // No measurements -> no divergence.
        assert!(!tracker.diverges(now));
        // Good measurements -> no divergence.
        tracker
            .record(measurement(&measurer, 0.9995, now))
            .unwrap();
        tracker
            .record(measurement(&measurer, 0.9990, now - 100))
            .unwrap();
        assert!(!tracker.diverges(now));
        // Bad measurements -> divergence.
        let mut tracker2 = SlaTracker::new(sla());
        tracker2
            .record(measurement(&measurer, 0.98, now))
            .unwrap();
        tracker2
            .record(measurement(&measurer, 0.97, now - 100))
            .unwrap();
        assert!(tracker2.diverges(now));
        assert!(tracker2.observed_uptime(now).unwrap() < 0.994);
    }

    #[test]
    fn measurement_forgery_rejected() {
        let measurer = AgentIdentity::generate();
        let mut m = measurement(&measurer, 0.99, now_unix());
        m.uptime_observed = 1.0; // tamper
        let mut tracker = SlaTracker::new(sla());
        assert!(tracker.record(m).is_err());
    }

    #[test]
    fn incident_disclosure_window() {
        let provider = AgentIdentity::generate();
        let now = now_unix();
        let incident = IncidentReport {
            incident_id: crate::new_id("inc"),
            provider: provider.did().clone(),
            capabilities: vec!["cap:data:api".into()],
            severity: "major".into(),
            scope: "Elevated latency".into(),
            root_cause: "DNS misconfig".into(),
            mitigation: "Traffic rebalanced".into(),
            affected_principals: 182,
            reported_at: now,
            provider_sig: None,
            sig: None,
        }
        .sign(&provider);
        assert!(incident.verify().is_ok());

        let tracker = SlaTracker::new(sla());
        // Reported 1h after occurrence: within 72h.
        assert!(tracker.disclosed_on_time(&incident, now - 3600));
        // Reported 4 days later: too late.
        assert!(!tracker.disclosed_on_time(&incident, now - 4 * 86400));
    }

    #[test]
    fn incident_tamper_detected() {
        let provider = AgentIdentity::generate();
        let mut incident = IncidentReport {
            incident_id: crate::new_id("inc"),
            provider: provider.did().clone(),
            capabilities: vec!["cap:data:api".into()],
            severity: "minor".into(),
            scope: "x".into(),
            root_cause: "y".into(),
            mitigation: "z".into(),
            affected_principals: 1,
            reported_at: now_unix(),
            provider_sig: None,
            sig: None,
        }
        .sign(&provider);
        incident.severity = "critical".into(); // tamper
        assert!(incident.verify().is_err());
    }
}
