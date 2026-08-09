//! Layered policy engine (RFC-0004).
//!
//! Every consequential action is evaluated against four policy layers
//! (platform → legal → organizational → personal), producing a signed
//! Decision Record. First `deny` terminates evaluation. L1 universal
//! prohibitions are non-overridable.

use crate::error::{Error, Result};
use crate::identity::{AgentIdentity, Did};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The four policy layers, in evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    Platform,
    Legal,
    Organizational,
    Personal,
}

impl Layer {
    pub fn as_str(&self) -> &'static str {
        match self {
            Layer::Platform => "L1",
            Layer::Legal => "L2",
            Layer::Organizational => "L3",
            Layer::Personal => "L4",
        }
    }
}

/// A rule effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Deny,
    /// Allow only if the condition (e.g. human review) is satisfied.
    AllowWithConditions,
}

/// A single rule: `if <field> <op> <value>` then `effect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub rule_id: String,
    pub effect: Effect,
    #[serde(rename = "if")]
    pub condition: Condition,
}

/// A condition over the action context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    /// Dot path into the context, e.g. "action.amount".
    pub field: String,
    /// One of: eq, ne, gt, gte, lt, lte, in, not_in, contains.
    pub op: String,
    /// Literal value or {"ref": "principal.budget.per_day"}.
    pub value: Value,
}

/// A policy: an ordered set of rules at one layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub policy_id: String,
    pub layer: Layer,
    pub rules: Vec<Rule>,
}

/// The outcome of a single rule evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleOutcome {
    NotMatched,
    Matched(Effect),
}

/// A signed decision record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub decision_id: String,
    pub evaluated_at: u64,
    pub layers_evaluated: Vec<String>,
    pub applied_rules: Vec<String>,
    pub outcome: String,
    #[serde(default)]
    pub conditions: Vec<String>,
    pub explanation: String,
    #[serde(default)]
    pub explanation_for_principal: String,
    pub action_hash: String,
    pub evaluated_by: Did,
    #[serde(default)]
    pub evaluator_sig: Option<String>,
}

impl DecisionRecord {
    /// The canonical bytes for signing.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.evaluator_sig = None;
        let v = serde_json::to_value(&clone).expect("record serializes");
        serde_json::to_vec(&v).expect("record serializes")
    }

    /// Sign with the evaluating runtime's identity.
    pub fn sign(mut self, evaluator: &AgentIdentity) -> Self {
        self.evaluator_sig = Some(evaluator.sign(&self.canonical_bytes()).to_hex());
        self
    }

    /// Verify the evaluator signature.
    pub fn verify(&self) -> Result<()> {
        let sig_hex = self.evaluator_sig.as_ref().ok_or(Error::BadSignature)?;
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

/// The policy engine: evaluates policies layer by layer.
#[derive(Debug, Default)]
pub struct Engine {
    /// L1 policies (non-overridable).
    platform: Vec<Policy>,
    legal: Vec<Policy>,
    organizational: Vec<Policy>,
    personal: Vec<Policy>,
}

/// The action context: a typed JSON map with dot-path access.
#[derive(Debug, Clone, Default)]
pub struct ActionContext {
    data: Value,
}

impl ActionContext {
    pub fn new() -> Self {
        Self {
            data: serde_json::json!({}),
        }
    }

    /// Set a nested value by dot path, e.g. "action.amount" -> 45.
    /// Intermediate objects are created automatically.
    pub fn set(&mut self, path: &str, value: Value) {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = &mut self.data;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                current[part] = value.clone();
            } else {
                // Create intermediate objects on demand.
                if current.get(*part).is_none() {
                    current[*part] = serde_json::json!({});
                }
                current = current
                    .get_mut(*part)
                    .expect("intermediate path just ensured");
            }
        }
    }

    /// Ensure a nested object path exists (so set() can write into it).
    pub fn ensure(&mut self, path: &str) {
        let parts: Vec<&str> = path.split('.').collect();
        let mut current = &mut self.data;
        for part in parts {
            if current.get(part).is_none() {
                current[part] = serde_json::json!({});
            }
            current = current.get_mut(part).expect("just ensured");
        }
    }

    /// Read a value by dot path.
    pub fn get(&self, path: &str) -> Option<&Value> {
        let mut current = &self.data;
        for part in path.split('.') {
            current = current.get(part)?;
        }
        Some(current)
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a policy at its layer.
    pub fn add_policy(&mut self, policy: Policy) {
        match policy.layer {
            Layer::Platform => self.platform.push(policy),
            Layer::Legal => self.legal.push(policy),
            Layer::Organizational => self.organizational.push(policy),
            Layer::Personal => self.personal.push(policy),
        }
    }

    /// Evaluate the context against all layers. Returns a signed
    /// decision record. First deny terminates.
    pub fn evaluate(
        &self,
        context: &ActionContext,
        evaluator: &AgentIdentity,
        locale_explanation: Option<&str>,
    ) -> DecisionRecord {
        let mut applied: Vec<String> = vec![];
        let mut conditions: Vec<String> = vec![];
        let mut outcome = "allow".to_string();
        let mut explanation = "allowed by policy".to_string();

        'outer: for (layer, policies) in [
            (Layer::Platform, &self.platform),
            (Layer::Legal, &self.legal),
            (Layer::Organizational, &self.organizational),
            (Layer::Personal, &self.personal),
        ] {
            for policy in policies {
                for rule in &policy.rules {
                    if let Some(matched) = evaluate_rule(rule, context) {
                        if matched != RuleOutcome::NotMatched {
                            applied.push(format!(
                                "{}.{}.{}",
                                layer.as_str(),
                                policy.policy_id,
                                rule.rule_id
                            ));
                        }
                        match matched {
                            RuleOutcome::Matched(Effect::Deny) => {
                                outcome = "deny".into();
                                explanation = format!(
                                    "denied by rule {} at {}",
                                    rule.rule_id,
                                    layer.as_str()
                                );
                                break 'outer;
                            }
                            RuleOutcome::Matched(Effect::AllowWithConditions) => {
                                conditions.push(rule.rule_id.clone());
                                outcome = "allow_with_conditions".into();
                            }
                            RuleOutcome::Matched(Effect::Allow) => {
                                // allow: keep evaluating (later rules may deny)
                            }
                            RuleOutcome::NotMatched => {}
                        }
                    }
                }
            }
        }

        let explanation_for_principal = locale_explanation
            .map(|s| s.to_string())
            .unwrap_or_else(|| explanation.clone());

        DecisionRecord {
            decision_id: crate::new_id("pol"),
            evaluated_at: crate::message::now_unix(),
            layers_evaluated: vec!["L1".into(), "L2".into(), "L3".into(), "L4".into()],
            applied_rules: applied,
            outcome,
            conditions,
            explanation,
            explanation_for_principal,
            action_hash: crate::sha256_hex(
                serde_json::to_vec(&context.data)
                    .expect("ctx serializes")
                    .as_slice(),
            ),
            evaluated_by: evaluator.did().clone(),
            evaluator_sig: None,
        }
        .sign(evaluator)
    }
}

/// Evaluate one rule against the context.
fn evaluate_rule(rule: &Rule, context: &ActionContext) -> Option<RuleOutcome> {
    let field_value = context.get(&rule.condition.field)?;
    let target = resolve_value(&rule.condition.value, context);

    let matched = match rule.condition.op.as_str() {
        "eq" => json_eq(field_value, &target),
        "ne" => !json_eq(field_value, &target),
        "gt" => json_cmp(field_value, &target)
            .map(|o| o == std::cmp::Ordering::Greater)
            .unwrap_or(false),
        "gte" => json_cmp(field_value, &target)
            .map(|o| o != std::cmp::Ordering::Less)
            .unwrap_or(false),
        "lt" => json_cmp(field_value, &target)
            .map(|o| o == std::cmp::Ordering::Less)
            .unwrap_or(false),
        "lte" => json_cmp(field_value, &target)
            .map(|o| o != std::cmp::Ordering::Greater)
            .unwrap_or(false),
        "in" => json_in(field_value, &target),
        "not_in" => !json_in(field_value, &target),
        "contains" => json_contains(field_value, &target),
        _ => false,
    };

    if matched {
        Some(RuleOutcome::Matched(rule.effect))
    } else {
        Some(RuleOutcome::NotMatched)
    }
}

/// Resolve a rule value: literal, or {"ref": "path"}.
fn resolve_value(value: &Value, context: &ActionContext) -> Value {
    if let Some(ref_path) = value.get("ref").and_then(|v| v.as_str()) {
        context.get(ref_path).cloned().unwrap_or(Value::Null)
    } else {
        value.clone()
    }
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => x == y,
        _ => a == b,
    }
}

fn json_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64().partial_cmp(&y.as_f64()),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn json_in(needle: &Value, haystack: &Value) -> bool {
    match haystack {
        Value::Array(items) => items.iter().any(|i| json_eq(i, needle)),
        _ => false,
    }
}

fn json_contains(haystack: &Value, needle: &Value) -> bool {
    match (haystack, needle) {
        (Value::Array(items), _) => items.iter().any(|i| json_eq(i, needle)),
        (Value::String(s), Value::String(n)) => s.contains(n),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> ActionContext {
        let mut ctx = ActionContext::new();
        ctx.ensure("action");
        ctx.ensure("principal");
        ctx.set("action.amount", serde_json::json!(45));
        ctx.set("action.to", serde_json::json!("did:gap:9b1e…"));
        ctx.set("principal.budget.per_day", serde_json::json!(100));
        ctx.set(
            "principal.embargo_list",
            serde_json::json!(["did:gap:evil"]),
        );
        ctx
    }

    fn policy(layer: Layer, rules: Vec<Rule>) -> Policy {
        Policy {
            policy_id: "test".into(),
            layer,
            rules,
        }
    }

    fn rule(id: &str, effect: Effect, field: &str, op: &str, value: Value) -> Rule {
        Rule {
            rule_id: id.into(),
            effect,
            condition: Condition {
                field: field.into(),
                op: op.into(),
                value,
            },
        }
    }

    #[test]
    fn personal_spend_cap_denies() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "spend_cap",
                Effect::Deny,
                "action.amount",
                "gt",
                serde_json::json!({ "ref": "principal.budget.per_day" }),
            )],
        ));

        let mut ctx = context();
        ctx.set("action.amount", serde_json::json!(150)); // over 100
        let record = engine.evaluate(&ctx, &evaluator, None);
        assert_eq!(record.outcome, "deny");
        assert!(record.verify().is_ok());
        assert!(record.applied_rules.iter().any(|r| r.contains("spend_cap")));
    }

    #[test]
    fn within_budget_allows() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "spend_cap",
                Effect::Deny,
                "action.amount",
                "gt",
                serde_json::json!({ "ref": "principal.budget.per_day" }),
            )],
        ));
        let ctx = context(); // amount = 45, budget = 100
        let record = engine.evaluate(&ctx, &evaluator, None);
        assert_eq!(record.outcome, "allow");
    }

    #[test]
    fn embargo_list_denies() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "embargo",
                Effect::Deny,
                "action.to",
                "in",
                serde_json::json!({ "ref": "principal.embargo_list" }),
            )],
        ));
        let mut ctx = context();
        ctx.set("action.to", serde_json::json!("did:gap:evil"));
        let record = engine.evaluate(&ctx, &evaluator, None);
        assert_eq!(record.outcome, "deny");
    }

    #[test]
    fn platform_layer_deny_is_terminal() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        // L1 prohibition
        engine.add_policy(policy(
            Layer::Platform,
            vec![rule(
                "no_csam",
                Effect::Deny,
                "action.category",
                "eq",
                serde_json::json!("csam"),
            )],
        ));
        // L4 would allow — but L1 fires first.
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "allow_all",
                Effect::Allow,
                "action.category",
                "eq",
                serde_json::json!("csam"),
            )],
        ));
        let mut ctx = context();
        ctx.ensure("action");
        ctx.set("action.category", serde_json::json!("csam"));
        let record = engine.evaluate(&ctx, &evaluator, None);
        assert_eq!(record.outcome, "deny");
        assert!(record.applied_rules.iter().any(|r| r.starts_with("L1.")));
    }

    #[test]
    fn allow_with_conditions_sets_conditions() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Legal,
            vec![rule(
                "high_risk",
                Effect::AllowWithConditions,
                "action.risk_class",
                "eq",
                serde_json::json!("high"),
            )],
        ));
        let mut ctx = context();
        ctx.ensure("action");
        ctx.set("action.risk_class", serde_json::json!("high"));
        let record = engine.evaluate(
            &ctx,
            &evaluator,
            Some("Action à risque élevé : confirmation humaine requise."),
        );
        assert_eq!(record.outcome, "allow_with_conditions");
        assert!(record.conditions.contains(&"high_risk".to_string()));
        assert!(record
            .explanation_for_principal
            .contains("confirmation humaine"));
    }

    #[test]
    fn explanation_localized() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "cap",
                Effect::Deny,
                "action.amount",
                "gt",
                serde_json::json!(50),
            )],
        ));
        let mut ctx = context();
        ctx.set("action.amount", serde_json::json!(500));
        let record = engine.evaluate(
            &ctx,
            &evaluator,
            Some("Dépense refusée : au-delà du plafond."),
        );
        assert!(record.explanation_for_principal.contains("plafond"));
    }

    #[test]
    fn decision_record_tamper_detected() {
        let evaluator = AgentIdentity::generate();
        let engine = Engine::new();
        let ctx = context();
        let mut record = engine.evaluate(&ctx, &evaluator, None);
        assert!(record.verify().is_ok());
        record.outcome = "deny".into(); // tamper
        assert!(record.verify().is_err());
    }

    #[test]
    fn ref_resolution_uses_context_value() {
        let evaluator = AgentIdentity::generate();
        let mut engine = Engine::new();
        engine.add_policy(policy(
            Layer::Personal,
            vec![rule(
                "cap",
                Effect::Deny,
                "action.amount",
                "gt",
                serde_json::json!({ "ref": "principal.budget.per_day" }),
            )],
        ));
        // Amount 45 ≤ budget 100 → allow; raise budget instead.
        let mut ctx = context();
        ctx.set("principal.budget.per_day", serde_json::json!(10));
        let record = engine.evaluate(&ctx, &evaluator, None);
        assert_eq!(record.outcome, "deny");
    }
}
