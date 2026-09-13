//! Narrow operator transport for cold migration. Tenant capabilities never
//! authorize this endpoint; the worker still checks the authority binding.
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use crate::server::NodeState;

fn admitted(method: &str, expected: &str, auth: Option<&str>, body: &Value) -> bool {
    let supplied = auth.and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    method == "POST" && (43..=128).contains(&expected.len())
        && expected.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        && crate::sha256_hex(expected.as_bytes()) == crate::sha256_hex(supplied.as_bytes())
        && body["migration_id"].as_str().is_some_and(|id| {
            id.strip_prefix("move_").is_some_and(|s| s.len() == 32
                && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
        })
        && body["operation"].as_str().is_some_and(|op| matches!(op,
            "status" | "upload-status" | "policy" | "export" | "import" | "read" | "write" | "settle" | "activate" | "discard" | "restore"))
}

pub fn worker(state: &Arc<Mutex<NodeState>>, method: &str, path: &str,
              auth: Option<&str>, body: &Value) -> Option<(u16, Value)> {
    if path.split('?').next() != Some("/v1/fleet/migration-worker") { return None; }
    let expected = std::env::var("GAP_FLEET_MIGRATION_TOKEN").unwrap_or_default();
    if !admitted(method, &expected, auth, body) {
        return Some((403, json!({"error":{"code":"migration_service_required"}})));
    }
    // Release the node lock before contacting the worker.
    let runner = state.lock().ok().and_then(|s|
        s.private_node.as_ref().and_then(|p| p.runner.clone()));
    Some(match runner {
        Some(r) => crate::private_node::forward(&r, "", "", "POST", "admin/migration", body.clone()),
        None => (503, json!({"error":{"code":"migration_worker_unavailable"}})),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dedicated_credential_and_fixed_operations_only() {
        let token = "a".repeat(43);
        let header = format!("Bearer {token}");
        let mut body = json!({"migration_id":format!("move_{}", "b".repeat(32)), "operation":"activate"});
        assert!(admitted("POST", &token, Some(&header), &body));
        assert!(!admitted("GET", &token, Some(&header), &body));
        assert!(!admitted("POST", "", Some("Bearer "), &body));
        assert!(!admitted("POST", &token, Some("Bearer gapf1.tenant"), &body));
        body["operation"] = json!("admin/credits");
        assert!(!admitted("POST", &token, Some(&header), &body));
        body["operation"] = json!("write");
        body["migration_id"] = json!("../other");
        assert!(!admitted("POST", &token, Some(&header), &body));
    }
}
