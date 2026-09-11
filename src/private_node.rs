//! Independent private management admission and preapproved microVM transport (with optional Compose).
//! The runner, never this process, talks to operator-provisioned guests.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, io::Read, path::PathBuf};

#[derive(Clone)]
pub struct PrivateNode {
    pub private: bool,
    pub approvals: PathBuf,
    pub compose_approvals: Option<PathBuf>,
    pub runner: Option<(String, String)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approvals {
    agents: Vec<String>,
    #[serde(default)]
    quotas: BTreeMap<String, MicroVMQuota>,
    #[serde(default)]
    always_on_agents: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MicroVMQuota {
    pub vcpus: u32,
    pub memory_mib: u32,
    #[serde(default = "default_vm_limit")]
    pub max_vms: u32,
}

fn default_vm_limit() -> u32 { 1 }

impl Default for MicroVMQuota {
    fn default() -> Self {
        Self {
            vcpus: 2,
            memory_mib: 4096,
            max_vms: 1,
        }
    }
}

impl PrivateNode {
    pub fn from_env() -> Result<Option<Self>> {
        let flag = |key: &str| -> Result<bool> {
            match std::env::var(key).as_deref() {
                Ok("1") => Ok(true),
                Err(_) | Ok("") | Ok("0") => Ok(false),
                _ => Err(Error::Other(format!("{key} must be 0 or 1"))),
            }
        };
        let private = flag("GAP_PRIVATE_NODE")?;
        let compose = flag("GAP_COMPOSE_ENABLED")?;
        if !private && !compose {
            return Ok(None);
        }
        if private && std::env::var("GAP_ADMIN_TOKEN").unwrap_or_default().len() < 32 {
            return Err(Error::Other(
                "private mode requires GAP_ADMIN_TOKEN (32+ bytes)".into(),
            ));
        }
        let approvals = if private {
            std::env::var("GAP_PRIVATE_APPROVALS_FILE")
                .map_err(|_| Error::Other("GAP_PRIVATE_APPROVALS_FILE is required".into()))?
        } else {
            String::new()
        };
        let compose_approvals = if compose {
            Some(PathBuf::from(
                std::env::var("GAP_COMPOSE_APPROVALS_FILE")
                    .map_err(|_| Error::Other("GAP_COMPOSE_APPROVALS_FILE is required".into()))?,
            ))
        } else {
            None
        };
        let runner = if compose {
            let url = std::env::var("GAP_COMPOSE_RUNNER_URL").unwrap_or_default();
            let token = std::env::var("GAP_COMPOSE_RUNNER_TOKEN").unwrap_or_default();
            if !(url.starts_with("http://") || url.starts_with("https://")) || token.len() < 32 {
                return Err(Error::Other(
                    "configure microVM runner URL and token (32+ bytes)".into(),
                ));
            }
            Some((url.trim_end_matches('/').to_owned(), token))
        } else {
            None
        };
        let policy = Self {
            private,
            approvals: approvals.into(),
            compose_approvals,
            runner,
        };
        if private {
            Self::read_approvals(&policy.approvals)?;
        }
        if let Some(path) = &policy.compose_approvals {
            Self::read_approvals(path)?;
        }
        Ok(Some(policy))
    }

    fn read_approvals(path: &std::path::Path) -> Result<Approvals> {
        // Reload on every management authorization. Missing/corrupt files deny,
        // and atomic file replacement takes effect without restarting GAP.
        let file = std::fs::File::open(path)
            .map_err(|_| Error::Unauthorized("private approval file unavailable".into()))?;
        let mut bytes = Vec::new();
        file.take(65_537)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Unauthorized("private approval file unreadable".into()))?;
        if bytes.len() > 65_536 {
            return Err(Error::Unauthorized(
                "private approval file too large".into(),
            ));
        }
        let approvals: Approvals = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Unauthorized("invalid private approval file".into()))?;
        if approvals.agents.iter().any(|did| !valid_did(did)) {
            return Err(Error::Unauthorized(
                "invalid approved agent identity".into(),
            ));
        }
        if approvals.quotas.iter().any(|(did, quota)| {
            !approvals.agents.contains(did)
                || quota.vcpus == 0
                || quota.memory_mib == 0
                || quota.max_vms == 0
                || quota.max_vms >= 2_u32.pow(31)
                || quota.vcpus >= 2_u32.pow(31)
                || quota.memory_mib >= 2_u32.pow(31)
        }) {
            return Err(Error::Unauthorized("invalid microVM quota".into()));
        }
        if approvals
            .always_on_agents
            .iter()
            .any(|did| !approvals.agents.contains(did))
        {
            return Err(Error::Unauthorized("invalid always-on approval".into()));
        }
        Ok(approvals)
    }

    pub fn authorize(&self, did: &str) -> Result<()> {
        if !self.private {
            return Ok(());
        }
        Self::authorize_file(&self.approvals, did)
    }

    pub fn always_on_allowed(&self, did: &str) -> bool {
        self.microvm_quota(did).is_ok()
            && self
                .compose_approvals
                .as_ref()
                .and_then(|path| Self::read_approvals(path).ok())
                .is_some_and(|approval| approval.always_on_agents.iter().any(|value| value == did))
    }

    pub fn authorize_compose(&self, did: &str) -> Result<()> {
        self.microvm_quota(did).map(|_| ())
    }

    pub fn microvm_quota(&self, did: &str) -> Result<MicroVMQuota> {
        self.authorize(did)?;
        let path = self
            .compose_approvals
            .as_ref()
            .ok_or_else(|| Error::Unauthorized("MicroVM approval is not configured".into()))?;
        let approvals = Self::read_approvals(path)?;
        if !approvals.agents.iter().any(|candidate| candidate == did) {
            return Err(Error::Unauthorized(
                "agent is not preapproved for this operation".into(),
            ));
        }
        Ok(approvals.quotas.get(did).cloned().unwrap_or_default())
    }

    fn authorize_file(path: &std::path::Path, did: &str) -> Result<()> {
        if !Self::read_approvals(path)?
            .agents
            .iter()
            .any(|candidate| candidate == did)
        {
            return Err(Error::Unauthorized(
                "agent is not preapproved for this operation".into(),
            ));
        }
        Ok(())
    }
}

fn valid_did(did: &str) -> bool {
    did.strip_prefix("did:gap:")
        .is_some_and(|key| key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A VM collection per project; legacy /vm routes address its default guest.
pub fn runtime_route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/v1/cloud/projects/")?;
    let (project, tail) = rest.split_once('/')?;
    if !project
        .strip_prefix("prj_")
        .is_some_and(|id| id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    match tail {
        "vm" => return Some((project, "vm")),
        "vms" => return Some((project, "vms")),
        "vm/start" => return Some((project, "vm/start")),
        "vm/stop" => return Some((project, "vm/stop")),
        "vm/hibernate" => return Some((project, "vm/hibernate")),
        "vm/resume" => return Some((project, "vm/resume")),
        "vm/runtime" => return Some((project, "runtime")),
        "vm/credits" => return Some((project, "credits")),
        "vm/budget" => return Some((project, "budget")),
        "vm/ports" => return Some((project, "ports")),
        "vm/ssh" => return Some((project, "ssh")),
        "vm/ingress" => return Some((project, "ingress")),
        _ => {}
    }
    if let Some(job) = tail.strip_prefix("vm/jobs/") {
        if job
            .strip_prefix("job_")
            .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Some((project, tail.strip_prefix("vm/").unwrap()));
        }
        return None;
    }
    if tail == "stack" {
        Some((project, ""))
    } else {
        tail.strip_prefix("stack/").map(|tail| (project, tail))
    }
}

/// Compatibility name for callers of the previous transport helper.
pub fn stack_route(path: &str) -> Option<(&str, &str)> {
    runtime_route(path)
}

pub fn forward(
    runner: &(String, String),
    project: &str,
    owner: &str,
    method: &str,
    action: &str,
    body: Value,
) -> (u16, Value) {
    let payload =
        json!({"project_id":project,"owner_did":owner,"method":method,"action":action,"body":body});
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .new_agent();
    let result = (|| {
        let mut response = agent
            .post(&format!("{}/rpc", runner.0))
            .header("Authorization", &format!("Bearer {}", runner.1))
            .header("Content-Type", "application/json")
            .send(payload.to_string().as_bytes())
            .ok()?;
        let status = response.status().as_u16();
        let bytes = response
            .body_mut()
            .with_config()
            .limit(2 * 1024 * 1024)
            .read_to_vec()
            .ok()?;
        let value: Value = serde_json::from_slice(&bytes).ok()?;
        Some((status, value))
    })();
    result.unwrap_or_else(|| (502, json!({"error":{"code":"compose_runner_unavailable","message":"MicroVM runner unavailable; retry with the same request_id"}})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_project_routes() {
        let id = "prj_0123456789abcdef01234567";
        assert_eq!(
            stack_route(&format!("/v1/cloud/projects/{id}/stack/releases")),
            Some((id, "releases"))
        );
        assert!(stack_route("/v1/cloud/projects/../stack/releases").is_none());
        assert!(stack_route(&format!("/v1/cloud/projects/{id}/stacking")).is_none());
    }
    #[test]
    fn microvm_routes_are_independent_of_compose_and_share_legacy_actions() {
        let project = "prj_0123456789abcdef01234567";
        for (canonical, legacy, action) in [
            ("vm", "stack/vm", "vm"),
            ("vm/start", "stack/vm/start", "vm/start"),
            ("vm/stop", "stack/vm/stop", "vm/stop"),
            ("vm/ports", "stack/ports", "ports"),
            ("vm/ssh", "stack/ssh", "ssh"),
            ("vm/ingress", "stack/ingress", "ingress"),
        ] {
            for suffix in [canonical, legacy] {
                assert_eq!(
                    runtime_route(&format!("/v1/cloud/projects/{project}/{suffix}")),
                    Some((project, action))
                );
            }
        }
        for (suffix, action) in [
            ("hibernate", "vm/hibernate"),
            ("resume", "vm/resume"),
            ("runtime", "runtime"),
            ("credits", "credits"),
            ("budget", "budget"),
        ] {
            assert_eq!(
                runtime_route(&format!("/v1/cloud/projects/{project}/vm/{suffix}")),
                Some((project, action))
            );
        }
        assert_eq!(runtime_route(&format!("/v1/cloud/projects/{project}/vms")), Some((project, "vms")));
        let action = "jobs/job_0123456789abcdef0123456789abcdef";
        assert_eq!(
            runtime_route(&format!("/v1/cloud/projects/{project}/vm/{action}")),
            Some((project, action))
        );
        for suffix in [
            "vms/invalid",
            "vm/releases",
            "vm/../stack/start",
            "vm/jobs/invalid",
            "vm/start/extra",
        ] {
            assert!(runtime_route(&format!("/v1/cloud/projects/{project}/{suffix}")).is_none());
        }
    }

    #[test]
    fn microvm_quotas_default_reload_and_validate() {
        let path = std::env::temp_dir().join(format!("gap-quota-{}.json", std::process::id()));
        let did = format!("did:gap:{}", "a".repeat(64));
        let policy = PrivateNode {
            private: false,
            approvals: path.clone(),
            compose_approvals: Some(path.clone()),
            runner: None,
        };
        std::fs::write(&path, json!({"agents": [&did]}).to_string()).unwrap();
        assert_eq!(policy.microvm_quota(&did).unwrap().vcpus, 2);
        assert_eq!(policy.microvm_quota(&did).unwrap().memory_mib, 4096);
        assert_eq!(policy.microvm_quota(&did).unwrap().max_vms, 1);
        assert!(!policy.always_on_allowed(&did));
        std::fs::write(
            &path,
            json!({"agents": [&did], "always_on_agents": [&did]}).to_string(),
        )
        .unwrap();
        assert!(policy.always_on_allowed(&did));
        std::fs::write(
            &path,
            json!({"agents": [], "always_on_agents": [&did]}).to_string(),
        )
        .unwrap();
        assert!(!policy.always_on_allowed(&did));

        std::fs::write(
            &path,
            json!({"agents": [&did], "quotas": {&did: {"vcpus": 4, "memory_mib": 8192}}})
                .to_string(),
        )
        .unwrap();
        assert_eq!(policy.microvm_quota(&did).unwrap().vcpus, 4);
        for quota in [
            json!({"vcpus": 0, "memory_mib": 4096}),
            json!({"vcpus": 2, "memory_mib": 4096, "max_vms": 0}),
            json!({"vcpus": 2, "memory_mib": 4096, "max_vms": 2147483648_u32}),
            json!({"vcpus": 2}),
            json!({"vcpus": 2, "memory_mib": 4096, "extra": 1}),
        ] {
            std::fs::write(
                &path,
                json!({"agents": [&did], "quotas": {&did: quota}}).to_string(),
            )
            .unwrap();
            assert!(policy.authorize_compose(&did).is_err());
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn approvals_reload_and_fail_closed() {
        let path =
            std::env::temp_dir().join(format!("gap-approval-test-{}", rand::random::<u64>()));
        let policy = PrivateNode {
            private: true,
            approvals: path.clone(),
            compose_approvals: None,
            runner: None,
        };
        let did = format!("did:gap:{}", "a".repeat(64));
        assert!(policy.authorize(&did).is_err());
        std::fs::write(&path, json!({"agents":[did]}).to_string()).unwrap();
        assert!(policy.authorize(&did).is_ok());
        std::fs::write(&path, r#"{"agents":[]}"#).unwrap();
        assert!(policy.authorize(&did).is_err());
        std::fs::write(&path, r#"{"agents":["*"]}"#).unwrap();
        assert!(policy.authorize(&did).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn public_cloud_access_does_not_grant_compose_access() {
        let path =
            std::env::temp_dir().join(format!("gap-compose-approval-{}", rand::random::<u64>()));
        let did = format!("did:gap:{}", "a".repeat(64));
        let mut policy = PrivateNode {
            private: false,
            approvals: path.with_extension("missing"),
            compose_approvals: Some(path.clone()),
            runner: None,
        };
        assert!(policy.authorize(&did).is_ok());
        assert!(policy.authorize_compose(&did).is_err());
        std::fs::write(&path, json!({"agents":[did]}).to_string()).unwrap();
        assert!(policy.authorize_compose(&did).is_ok());
        policy.private = true;
        assert!(policy.authorize_compose(&did).is_err()); // private approval still required
        policy.private = false;
        std::fs::write(&path, r#"{"agents":[]}"#).unwrap();
        assert!(policy.authorize(&did).is_ok());
        assert!(policy.authorize_compose(&did).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
