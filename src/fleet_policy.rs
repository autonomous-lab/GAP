//! Short execution leases for centrally suspended customers. Local bans remain independent.
use std::{collections::HashSet, sync::{Arc, Mutex}, time::{Duration, Instant}};
use serde::Deserialize;
use crate::server::NodeState;

#[derive(Deserialize)]
struct Snapshot {
    protocol: u64, operator_id: String, node_id: String, sequence: u64,
    all_blocked: bool, agents: HashSet<String>, email_hashes: HashSet<String>, projects: HashSet<String>,
}
#[derive(Default)]
pub struct Policy { value: Mutex<Option<(Snapshot, Instant)>> }
impl Policy {
    fn accept(&self, raw: serde_json::Value, operator: &str, node: &str, started: Instant) -> bool {
        let Ok(next) = serde_json::from_value::<Snapshot>(raw) else { return false };
        if next.protocol != 1 || next.operator_id != operator || next.node_id != node { return false }
        let Ok(mut current) = self.value.lock() else { return false };
        if current.as_ref().is_some_and(|(old,_)| next.sequence < old.sequence) { return false }
        *current = Some((next, started)); true
    }
    pub fn blocked(&self, agent: Option<&str>, email_hash: Option<&str>, project: Option<&str>) -> bool {
        let Ok(value) = self.value.lock() else { return true };
        let Some((s, at)) = value.as_ref() else { return true };
        at.elapsed() >= Duration::from_secs(5) || s.all_blocked
            || agent.is_some_and(|v| s.agents.contains(v))
            || email_hash.is_some_and(|v| s.email_hashes.contains(v))
            || project.is_some_and(|v| s.projects.contains(v))
    }
    pub fn sequence(&self) -> u64 {
        self.value.lock().ok().and_then(|v| v.as_ref().map(|(s,_)|s.sequence)).unwrap_or(0)
    }
}

pub fn start(state: &Arc<Mutex<NodeState>>) -> Result<(), String> {
    match std::env::var("GAP_FLEET_POLICY_ENABLED").as_deref() {
        Err(_) | Ok("") | Ok("0") => return Ok(()),
        Ok("1") => (), _ => return Err("invalid GAP_FLEET_POLICY_ENABLED".into()),
    }
    let policy = Arc::new(Policy::default());
    let (runner, operator, node) = {
        let mut s = state.lock().map_err(|_|"fleet policy state unavailable")?;
        let access = s.fleet_access.as_ref().ok_or("fleet policy requires fleet access")?;
        let operator=access.operator.clone(); let node=access.node.clone();
        let runner=s.private_node.as_ref().and_then(|p|p.runner.clone()).ok_or("fleet policy requires worker")?;
        s.fleet_policy=Some(policy.clone()); (runner,operator,node)
    };
    let weak=Arc::downgrade(state);
    std::thread::spawn(move || {
        while weak.upgrade().is_some() {
            let started=Instant::now();
            let (status,raw)=crate::private_node::forward(&runner,"","","GET","admin/customer-policy",serde_json::json!({}));
            if status==200 { policy.accept(raw,&operator,&node,started); }
            // Never extend a prior lease on failure or a stale response.
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_wrong_node_and_older_policy_cannot_restore_access() {
        let p=Policy::default(); assert!(p.blocked(None,None,None));
        let mut s=serde_json::json!({"protocol":1,"operator_id":"op","node_id":"one","sequence":2,
            "all_blocked":false,"agents":["did"],"email_hashes":["email"],"projects":["project"]});
        assert!(p.accept(s.clone(),"op","one",Instant::now()));
        assert!(p.blocked(Some("did"),None,None)); assert!(p.blocked(None,Some("email"),None));
        assert!(p.blocked(None,None,Some("project"))); assert!(!p.blocked(Some("other"),None,None));
        s["sequence"]=serde_json::json!(1); assert!(!p.accept(s.clone(),"op","one",Instant::now()));
        s["sequence"]=serde_json::json!(3); assert!(!p.accept(s.clone(),"op","two",Instant::now()));
        assert!(p.accept(s,"op","one",Instant::now()-Duration::from_secs(6)));
        assert!(p.blocked(Some("other"),None,None));
    }
}
