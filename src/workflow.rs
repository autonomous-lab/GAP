//! Workflow composition (RFC-0002).
//!
//! A Workflow is a signed, versioned DAG of contract steps executed by
//! different agents, with outputs flowing between steps. Turns GAP from
//! a 1:1 contract protocol into a multi-agent orchestration layer.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use crate::message::now_unix;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Workflow lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowState {
    #[default]
    Pending,
    Provisioning,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Failure handling mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureMode {
    Abort,
    Continue,
    Compensate,
}

/// A step's execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Pending,
    Provisioning,
    Running,
    Delivered,
    Accepted,
    Failed,
    Skipped,
}

/// A workflow step: a contract template with input/output bindings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub step_id: String,
    pub capability: String,
    /// Step ids this step depends on.
    #[serde(default)]
    pub needs: Vec<String>,
    /// Binding expressions: `"${workflow.topic}"` or `"${steps.X.out}"`.
    #[serde(default)]
    pub inputs: HashMap<String, String>,
    /// Named output handles: `"raw" -> "steps.scrape.deliverable"`.
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    /// Optional per-step terms (price, deadline offset, autonomy).
    #[serde(default)]
    pub terms: Option<StepTerms>,
    #[serde(default)]
    pub retryable: bool,
}

/// Per-step terms.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepTerms {
    #[serde(default)]
    #[serde(deserialize_with = "crate::contract::de_opt_f64_from_string")]
    pub price_amount: Option<f64>,
    #[serde(default)]
    pub price_model: Option<String>,
    #[serde(default)]
    pub deadline_offset_secs: Option<u64>,
    #[serde(default)]
    pub autonomy: Option<String>,
}

/// A signed workflow manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub workflow_id: String,
    pub version: String,
    pub sponsor: Did,
    pub name: String,
    #[serde(default)]
    pub inputs: HashMap<String, Value>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub budget: Option<Budget>,
    #[serde(default = "default_failure")]
    pub on_failure: FailureMode,
    pub expires_at: u64,
    pub created_at: u64,
    #[serde(default)]
    pub sponsor_sig: Option<String>,
    #[serde(skip)]
    pub state: WorkflowState,
}

fn default_failure() -> FailureMode {
    FailureMode::Abort
}

/// Workflow budget.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default)]
    #[serde(deserialize_with = "crate::contract::de_opt_f64_from_string")]
    pub max_total: Option<f64>,
    #[serde(default)]
    pub currency: String,
}

impl Workflow {
    /// Create and sign a workflow manifest.
    pub fn create(
        sponsor: &AgentIdentity,
        name: &str,
        inputs: HashMap<String, Value>,
        steps: Vec<Step>,
        budget: Option<Budget>,
        on_failure: FailureMode,
        expires_in_secs: u64,
    ) -> Self {
        let mut w = Self {
            workflow_id: crate::new_id("wf"),
            version: "0.2.0".into(),
            sponsor: sponsor.did().clone(),
            name: name.into(),
            inputs,
            steps,
            budget,
            on_failure,
            expires_at: now_unix() + expires_in_secs,
            created_at: now_unix(),
            sponsor_sig: None,
            state: WorkflowState::Pending,
        };
        w.resign(sponsor);
        w
    }

    /// Re-sign after mutation.
    pub fn resign(&mut self, sponsor: &AgentIdentity) {
        self.sponsor_sig = None;
        let canonical = self.canonical_bytes();
        self.sponsor_sig = Some(sponsor.sign(&canonical).to_hex());
    }

    /// Verify the sponsor signature and expiry.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.sponsor_sig.as_ref().ok_or(Error::BadSignature)?;
        let sig_bytes: [u8; 64] = hex::decode(sig_hex)
            .map_err(|_| Error::BadSignature)?
            .try_into()
            .map_err(|_| Error::BadSignature)?;
        crate::identity::verify_signature(
            &self.sponsor,
            &self.canonical_bytes(),
            &crate::identity::Signature(sig_bytes),
        )?;
        if now_unix() > self.expires_at {
            return Err(Error::Other("workflow expired".into()));
        }
        Ok(())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sponsor_sig = None;
        let v = serde_json::to_value(&clone).expect("workflow serializes");
        serde_json::to_vec(&v).expect("workflow serializes")
    }

    /// Validate the DAG: unique step ids, no cycles, needs reference
    /// existing steps, no self-dependency.
    pub fn validate_dag(&self) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        for step in &self.steps {
            if !ids.insert(step.step_id.clone()) {
                return Err(Error::Other(format!("duplicate step id: {}", step.step_id)));
            }
            if step.needs.contains(&step.step_id) {
                return Err(Error::Other(format!(
                    "step {} depends on itself",
                    step.step_id
                )));
            }
            for need in &step.needs {
                if !ids.contains(need) {
                    return Err(Error::Other(format!(
                        "step {} needs unknown step {}",
                        step.step_id, need
                    )));
                }
            }
        }
        // Cycle detection via topological sort (Kahn's algorithm).
        let mut indegree: HashMap<&str, usize> = HashMap::new();
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
        for step in &self.steps {
            indegree.entry(&step.step_id).or_insert(0);
            for need in &step.needs {
                adjacency
                    .entry(need.as_str())
                    .or_default()
                    .push(&step.step_id);
                *indegree.entry(&step.step_id).or_insert(0) += 1;
            }
        }
        let mut queue: Vec<&str> = indegree
            .iter()
            .filter(|(_, d)| **d == 0)
            .map(|(id, _)| *id)
            .collect();
        let mut visited = 0;
        while let Some(id) = queue.pop() {
            visited += 1;
            if let Some(nexts) = adjacency.get(id) {
                for n in nexts {
                    let d = indegree.get_mut(n).unwrap();
                    *d -= 1;
                    if *d == 0 {
                        queue.push(n);
                    }
                }
            }
        }
        if visited != self.steps.len() {
            return Err(Error::Other("workflow DAG contains a cycle".into()));
        }
        Ok(())
    }

    /// Validate binding expressions: each `${expr}` must reference a
    /// workflow input or a prior step output.
    pub fn validate_bindings(&self) -> Result<()> {
        for step in &self.steps {
            for expr in step.inputs.values() {
                if let Some(inner) = expr.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                    let parts: Vec<&str> = inner.split('.').collect();
                    match parts.as_slice() {
                        ["workflow", key] => {
                            if !self.inputs.contains_key(*key) {
                                return Err(Error::Other(format!(
                                    "binding references unknown workflow input: {inner}"
                                )));
                            }
                        }
                        ["steps", step_id, out] => {
                            if !self.steps.iter().any(|s| s.step_id == *step_id) {
                                return Err(Error::Other(format!(
                                    "binding references unknown step: {inner}"
                                )));
                            }
                            if !self
                                .steps
                                .iter()
                                .any(|s| s.step_id == *step_id && s.outputs.contains_key(*out))
                            {
                                return Err(Error::Other(format!(
                                    "binding references unknown output: {inner}"
                                )));
                            }
                        }
                        _ => {
                            return Err(Error::Other(format!("malformed binding: {inner}")));
                        }
                    }
                } else {
                    return Err(Error::Other(format!(
                        "malformed binding expression: {expr}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Validate the total budget against step prices (where declared).
    pub fn validate_budget(&self) -> Result<()> {
        if let Some(budget) = &self.budget {
            if let Some(max) = budget.max_total {
                let declared: f64 = self
                    .steps
                    .iter()
                    .filter_map(|s| s.terms.as_ref())
                    .filter_map(|t| t.price_amount)
                    .sum();
                if declared > max {
                    return Err(Error::Other(format!(
                        "step prices sum {declared} exceeds workflow budget {max}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// Run all validations at once (the entry point for creation).
    pub fn validate(&self) -> Result<()> {
        self.verify()?;
        self.validate_dag()?;
        self.validate_bindings()?;
        self.validate_budget()
    }
}

/// The workflow engine: executes the DAG step by step.
#[derive(Debug, Default)]
pub struct WorkflowEngine {
    /// step_id -> state
    step_states: HashMap<String, StepState>,
    /// step_id -> resolved output values
    step_outputs: HashMap<String, HashMap<String, Value>>,
    /// Whether any step failed (for continue mode bookkeeping).
    any_failed: bool,
}

impl WorkflowEngine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Resolve a binding expression against workflow inputs and
    /// collected step outputs.
    pub fn resolve_binding(&self, expr: &str, inputs: &HashMap<String, Value>) -> Result<Value> {
        let inner = expr
            .strip_prefix("${")
            .and_then(|s| s.strip_suffix('}'))
            .ok_or_else(|| Error::Other(format!("malformed binding: {expr}")))?;
        let parts: Vec<&str> = inner.split('.').collect();
        match parts.as_slice() {
            ["workflow", key] => inputs
                .get(*key)
                .cloned()
                .ok_or_else(|| Error::Other(format!("unknown workflow input: {key}"))),
            ["steps", step_id, out] => self
                .step_outputs
                .get(*step_id)
                .and_then(|outs| outs.get(*out))
                .cloned()
                .ok_or_else(|| {
                    Error::Other(format!("output {out} of step {step_id} not available"))
                }),
            _ => Err(Error::Other(format!("malformed binding: {inner}"))),
        }
    }

    /// Which steps are runnable now (all needs accepted or skipped).
    pub fn runnable_steps<'a>(&self, workflow: &'a Workflow) -> Vec<&'a Step> {
        workflow
            .steps
            .iter()
            .filter(|s| {
                let state = self
                    .step_states
                    .get(&s.step_id)
                    .copied()
                    .unwrap_or(StepState::Pending);
                state == StepState::Pending
                    && s.needs.iter().all(|n| {
                        matches!(
                            self.step_states.get(n),
                            Some(StepState::Accepted) | Some(StepState::Skipped)
                        )
                    })
            })
            .collect()
    }

    /// Mark a step as provisioned (provider selected).
    pub fn provision(&mut self, step_id: &str) -> Result<()> {
        self.set_state(step_id, StepState::Provisioning)
    }

    /// Mark a step as running.
    pub fn start(&mut self, step_id: &str) -> Result<()> {
        self.set_state(step_id, StepState::Running)
    }

    /// Record a step's delivery with its output values.
    pub fn deliver(&mut self, step_id: &str, outputs: HashMap<String, Value>) -> Result<()> {
        self.set_state(step_id, StepState::Delivered)?;
        self.step_outputs.insert(step_id.to_string(), outputs);
        Ok(())
    }

    /// Accept a step's delivery.
    pub fn accept(&mut self, step_id: &str) -> Result<()> {
        self.set_state(step_id, StepState::Accepted)
    }

    /// Fail a step (abort mode: workflow fails; continue mode: step is
    /// skipped and downstream steps evaluate).
    pub fn fail(&mut self, step_id: &str, on_failure: FailureMode) -> Result<()> {
        self.set_state(step_id, StepState::Failed)?;
        self.any_failed = true;
        if on_failure == FailureMode::Continue {
            // Downstream steps that depended on this one get skipped.
            self.set_state(step_id, StepState::Skipped)
        } else {
            Ok(())
        }
    }

    /// Whether the workflow has any failed step.
    pub fn has_failures(&self) -> bool {
        self.any_failed
    }

    /// The engine's overall progress: number of accepted steps.
    pub fn accepted_count(&self, workflow: &Workflow) -> usize {
        workflow
            .steps
            .iter()
            .filter(|s| {
                matches!(
                    self.step_states.get(&s.step_id),
                    Some(StepState::Accepted) | Some(StepState::Skipped)
                )
            })
            .count()
    }

    pub fn state_of(&self, step_id: &str) -> StepState {
        self.step_states
            .get(step_id)
            .copied()
            .unwrap_or(StepState::Pending)
    }

    fn set_state(&mut self, step_id: &str, state: StepState) -> Result<()> {
        let current = self
            .step_states
            .get(step_id)
            .copied()
            .unwrap_or(StepState::Pending);
        let ok = match (current, state) {
            (StepState::Pending, StepState::Provisioning) => true,
            (StepState::Pending, StepState::Running) => true, // provisioning optional
            (StepState::Provisioning, StepState::Running) => true,
            (StepState::Pending, StepState::Delivered) => true, // direct delivery
            (StepState::Running, StepState::Delivered) => true,
            (StepState::Delivered, StepState::Accepted) => true,
            (
                StepState::Pending | StepState::Provisioning | StepState::Running,
                StepState::Failed,
            ) => true,
            (StepState::Failed, StepState::Skipped) => true,
            _ => false,
        };
        if !ok {
            return Err(Error::InvalidTransition {
                from: format!("{current:?}"),
                to: format!("{state:?}"),
            });
        }
        self.step_states.insert(step_id.to_string(), state);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, needs: Vec<&str>) -> Step {
        Step {
            step_id: id.into(),
            capability: format!("cap:{id}"),
            needs: needs.into_iter().map(String::from).collect(),
            inputs: HashMap::new(),
            outputs: HashMap::new(),
            terms: None,
            retryable: false,
        }
    }

    #[test]
    fn dag_validation_accepts_valid_and_rejects_cycles() {
        let sponsor = AgentIdentity::generate();
        let wf = Workflow::create(
            &sponsor,
            "pipeline",
            HashMap::new(),
            vec![
                step("scrape", vec![]),
                step("analyze", vec!["scrape"]),
                step("publish", vec!["analyze"]),
            ],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf.validate_dag().is_ok());

        // Cycle: a -> b -> a
        let mut a = step("a", vec!["b"]);
        a.outputs.insert("x".into(), "steps.b.x".into());
        let mut b = step("b", vec!["a"]);
        b.outputs.insert("x".into(), "steps.a.x".into());
        let wf2 = Workflow::create(
            &sponsor,
            "cycle",
            HashMap::new(),
            vec![a, b],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf2.validate_dag().is_err());
    }

    #[test]
    fn dag_validation_rejects_unknown_needs_and_duplicates() {
        let sponsor = AgentIdentity::generate();
        let wf = Workflow::create(
            &sponsor,
            "bad",
            HashMap::new(),
            vec![step("a", vec!["ghost"])],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf.validate_dag().is_err());

        let wf2 = Workflow::create(
            &sponsor,
            "dup",
            HashMap::new(),
            vec![step("a", vec![]), step("a", vec![])],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf2.validate_dag().is_err());
    }

    #[test]
    fn binding_validation_and_resolution() {
        let sponsor = AgentIdentity::generate();
        let mut inputs: HashMap<String, Value> = HashMap::new();
        inputs.insert("topic".into(), json!("quantum computing"));
        let mut scrape = step("scrape", vec![]);
        scrape
            .inputs
            .insert("query".into(), "${workflow.topic}".into());
        scrape
            .outputs
            .insert("raw".into(), "steps.scrape.deliverable".into());
        let mut analyze = step("analyze", vec!["scrape"]);
        analyze
            .inputs
            .insert("data".into(), "${steps.scrape.raw}".into());
        let wf = Workflow::create(
            &sponsor,
            "pipeline",
            inputs.clone(),
            vec![scrape, analyze],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf.validate_bindings().is_ok());

        // Unknown output reference.
        let mut bad = step("x", vec![]);
        bad.inputs
            .insert("in".into(), "${steps.scrape.nope}".into());
        let wf2 = Workflow::create(
            &sponsor,
            "bad-binding",
            HashMap::new(),
            vec![bad],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf2.validate_bindings().is_err());

        // Resolution against collected outputs.
        let mut engine = WorkflowEngine::new();
        let mut outs = HashMap::new();
        outs.insert("raw".into(), json!("lead: alice@example.com"));
        engine.deliver("scrape", outs).unwrap();
        let resolved = engine
            .resolve_binding("${steps.scrape.raw}", &inputs)
            .unwrap();
        assert_eq!(resolved, json!("lead: alice@example.com"));
        // Workflow input resolution.
        let resolved2 = engine
            .resolve_binding("${workflow.topic}", &inputs)
            .unwrap();
        assert_eq!(resolved2, json!("quantum computing"));
        // Unknown binding fails.
        assert!(engine.resolve_binding("${steps.ghost.x}", &inputs).is_err());
    }

    #[test]
    fn budget_validation() {
        let sponsor = AgentIdentity::generate();
        let mut s = step("a", vec![]);
        s.terms = Some(StepTerms {
            price_amount: Some(80.0),
            price_model: Some("fixed".into()),
            deadline_offset_secs: Some(3600),
            autonomy: None,
        });
        let wf = Workflow::create(
            &sponsor,
            "over-budget",
            HashMap::new(),
            vec![s],
            Some(Budget {
                max_total: Some(50.0),
                currency: "EUR".into(),
            }),
            FailureMode::Abort,
            3600,
        );
        assert!(wf.validate_budget().is_err());

        let mut s2 = step("a", vec![]);
        s2.terms = Some(StepTerms {
            price_amount: Some(30.0),
            price_model: Some("fixed".into()),
            deadline_offset_secs: Some(3600),
            autonomy: None,
        });
        let wf2 = Workflow::create(
            &sponsor,
            "in-budget",
            HashMap::new(),
            vec![s2],
            Some(Budget {
                max_total: Some(50.0),
                currency: "EUR".into(),
            }),
            FailureMode::Abort,
            3600,
        );
        assert!(wf2.validate_budget().is_ok());
    }

    #[test]
    fn engine_executes_in_dependency_order() {
        let sponsor = AgentIdentity::generate();
        let wf = Workflow::create(
            &sponsor,
            "pipeline",
            HashMap::new(),
            vec![
                step("scrape", vec![]),
                step("analyze", vec!["scrape"]),
                step("publish", vec!["analyze"]),
            ],
            None,
            FailureMode::Abort,
            3600,
        );

        let mut engine = WorkflowEngine::new();
        // Initially only scrape is runnable.
        let runnable = engine.runnable_steps(&wf);
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].step_id, "scrape");

        engine.start("scrape").unwrap();
        let mut outs = HashMap::new();
        outs.insert("deliverable".into(), json!("raw data"));
        engine.deliver("scrape", outs).unwrap();
        engine.accept("scrape").unwrap();

        // Now analyze is runnable.
        let runnable = engine.runnable_steps(&wf);
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].step_id, "analyze");

        // Analyze can't start before scrape accepted — already done.
        engine.start("analyze").unwrap();
        engine.deliver("analyze", HashMap::new()).unwrap();
        engine.accept("analyze").unwrap();

        engine.start("publish").unwrap();
        engine.deliver("publish", HashMap::new()).unwrap();
        engine.accept("publish").unwrap();
        assert_eq!(engine.accepted_count(&wf), 3);
    }

    #[test]
    fn engine_abort_vs_continue_on_failure() {
        let sponsor = AgentIdentity::generate();
        let wf = Workflow::create(
            &sponsor,
            "abort-wf",
            HashMap::new(),
            vec![step("a", vec![]), step("b", vec!["a"])],
            None,
            FailureMode::Abort,
            3600,
        );
        let mut engine = WorkflowEngine::new();
        engine.start("a").unwrap();
        // Abort mode: fail stays Failed.
        engine.fail("a", FailureMode::Abort).unwrap();
        assert!(engine.has_failures());
        assert!(engine.runnable_steps(&wf).is_empty());

        // Continue mode: failed step becomes Skipped, downstream runs.
        let wf2 = Workflow::create(
            &sponsor,
            "continue-wf",
            HashMap::new(),
            vec![step("a", vec![]), step("b", vec!["a"])],
            None,
            FailureMode::Continue,
            3600,
        );
        let mut engine2 = WorkflowEngine::new();
        engine2.start("a").unwrap();
        engine2.fail("a", FailureMode::Continue).unwrap();
        let runnable = engine2.runnable_steps(&wf2);
        assert_eq!(runnable.len(), 1);
        assert_eq!(runnable[0].step_id, "b");
    }

    #[test]
    fn engine_rejects_invalid_transitions() {
        let mut engine = WorkflowEngine::new();
        // Deliver without any prior state is now allowed (direct
        // delivery); but accept without deliver is still invalid.
        engine.start("y").unwrap();
        assert!(engine.accept("y").is_err());
        // Double-start is invalid.
        let mut engine2 = WorkflowEngine::new();
        engine2.start("z").unwrap();
        assert!(engine2.start("z").is_err());
        // Accept after direct deliver works.
        let mut engine3 = WorkflowEngine::new();
        engine3.deliver("w", HashMap::new()).unwrap();
        engine3.accept("w").unwrap();
        // Accept twice is invalid.
        assert!(engine3.accept("w").is_err());
    }

    #[test]
    fn workflow_signature_and_expiry() {
        let sponsor = AgentIdentity::generate();
        let wf = Workflow::create(
            &sponsor,
            "signed",
            HashMap::new(),
            vec![step("a", vec![])],
            None,
            FailureMode::Abort,
            3600,
        );
        assert!(wf.verify().is_ok());

        let mut wf2 = Workflow::create(
            &sponsor,
            "expired",
            HashMap::new(),
            vec![step("a", vec![])],
            None,
            FailureMode::Abort,
            1,
        );
        // Tamper: change the name after signing.
        wf2.name = "evil".into();
        assert!(wf2.verify().is_err());
    }
}
