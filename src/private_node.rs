//! Opt-in private management admission and Compose transport.
//! The runner, never this process, talks to operator-provisioned guests.
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf};

#[derive(Clone)]
pub struct PrivateNode {
    pub approvals: PathBuf,
    pub runner: Option<(String, String)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approvals {
    agents: Vec<String>,
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
        if !private {
            if compose {
                return Err(Error::Other("Compose requires GAP_PRIVATE_NODE=1".into()));
            }
            return Ok(None);
        }
        if std::env::var("GAP_ADMIN_TOKEN").unwrap_or_default().len() < 32 {
            return Err(Error::Other(
                "private mode requires GAP_ADMIN_TOKEN (32+ bytes)".into(),
            ));
        }
        let approvals = std::env::var("GAP_PRIVATE_APPROVALS_FILE")
            .map_err(|_| Error::Other("GAP_PRIVATE_APPROVALS_FILE is required".into()))?;
        let runner = if compose {
            let url = std::env::var("GAP_COMPOSE_RUNNER_URL").unwrap_or_default();
            let token = std::env::var("GAP_COMPOSE_RUNNER_TOKEN").unwrap_or_default();
            if !(url.starts_with("http://") || url.starts_with("https://")) || token.len() < 32 {
                return Err(Error::Other(
                    "configure Compose runner URL and token (32+ bytes)".into(),
                ));
            }
            Some((url.trim_end_matches('/').to_owned(), token))
        } else {
            None
        };
        let policy = Self {
            approvals: approvals.into(),
            runner,
        };
        policy.read_approvals()?;
        Ok(Some(policy))
    }

    fn read_approvals(&self) -> Result<Approvals> {
        // Reload on every management authorization. Missing/corrupt files deny,
        // and atomic file replacement takes effect without restarting GAP.
        let file = std::fs::File::open(&self.approvals)
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
        Ok(approvals)
    }

    pub fn authorize(&self, did: &str) -> Result<()> {
        if !self
            .read_approvals()?
            .agents
            .iter()
            .any(|candidate| candidate == did)
        {
            return Err(Error::Unauthorized(
                "agent is not preapproved on this private node".into(),
            ));
        }
        Ok(())
    }
}

fn valid_did(did: &str) -> bool {
    did.strip_prefix("did:gap:")
        .is_some_and(|key| key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// One stack per project in the MVP. No user-chosen upstream addresses.
pub fn stack_route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/v1/cloud/projects/")?;
    let (project, tail) = rest.split_once('/')?;
    if !project
        .strip_prefix("prj_")
        .is_some_and(|id| id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    if tail == "stack" {
        Some((project, ""))
    } else {
        tail.strip_prefix("stack/").map(|tail| (project, tail))
    }
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
    result.unwrap_or_else(|| (502, json!({"error":{"code":"compose_runner_unavailable","message":"Compose runner unavailable; retry with the same request_id"}})))
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
    fn approvals_reload_and_fail_closed() {
        let path =
            std::env::temp_dir().join(format!("gap-approval-test-{}", rand::random::<u64>()));
        let policy = PrivateNode {
            approvals: path.clone(),
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
}
