//! Public discovery is informational. Never forwards a visitor credential.
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() }
fn field(key: &str) -> String { std::env::var(key).unwrap_or_default().chars().take(200).collect() }

fn assurance(origin: &str, listed: &str) -> Value {
    let provider = listed.len() <= 4096 && listed.split(',').take(8)
        .any(|v| !v.is_empty() && v.trim_end_matches('/') == origin.trim_end_matches('/'));
    json!({"identity_status":"not_independently_verified", "controls_status":"not_reviewed",
        "provider":if provider {json!({"name":"Elestio","relationship":"directory_operator_declared",
            "claims":["SOC 2 Type II","ISO 27001"],"evidence_status":"provider_statement",
            "source_url":"https://elest.io/security-and-compliance",
            "scope":"Elestio management services; applicability requires review of the reports.",
            "reports_reviewed":false})} else {Value::Null}})
}

pub fn local(runner: Option<(String,String)>) -> Value {
    static CACHE: OnceLock<Mutex<Option<(Instant,Value)>>> = OnceLock::new();
    let mut cache=CACHE.get_or_init(||Mutex::new(None)).lock().unwrap();
    if let Some((at,value))=&*cache { if at.elapsed()<Duration::from_secs(30) { return value.clone() } }
    let capacity=runner.map(|r|crate::private_node::forward(&r,"","","GET","admin/public-capacity",json!({})))
        .filter(|r|r.0==200).map(|r|r.1).unwrap_or(json!({"available":false}));
    let value=json!({"protocol":1,"node_id":field("GAP_FLEET_NODE_ID"),"operator":field("GAP_PUBLIC_OPERATOR"),
        "country":field("GAP_PUBLIC_COUNTRY"),"region":field("GAP_PUBLIC_REGION"),
        "version":env!("CARGO_PKG_VERSION"),"checked_at":now(),"max_age_seconds":30,
        "services":["kv","objects","sqlite","functions","sites","realtime"],"microvm":capacity,
        "trust":"Operator-declared metadata; discovery does not grant access or establish trust."});
    *cache=Some((Instant::now(),value.clone()));value
}

pub fn directory() -> Value {
    static CACHE: OnceLock<Mutex<Option<(Instant,Value)>>> = OnceLock::new();
    let mut cache=CACHE.get_or_init(||Mutex::new(None)).lock().unwrap();
    if let Some((at,value))=&*cache { if at.elapsed()<Duration::from_secs(30) { return value.clone() } }
    // A bounded operator-managed list, never a URL supplied by the request.
    let sources=std::env::var("GAP_PUBLIC_EXPLORER_NODES").unwrap_or_default();
    if sources.len()>4096 {return json!({"nodes":[],"error":"invalid_directory_configuration"})}
    let agent=ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(3)))
        .max_redirects(0).build().new_agent();
    let mut nodes=Vec::new();
    for origin in sources.split(',').filter(|s|!s.is_empty()).take(8) {
        let Ok(uri)=origin.parse::<ureq::http::Uri>() else {continue};
        if uri.scheme_str()!=Some("https") || uri.authority().is_none_or(|a|a.as_str().contains('@'))
            || uri.query().is_some() || !matches!(uri.path(),""|"/") {continue}
        let started=Instant::now();
        let value=(|| {
            let mut r=agent.get(&format!("{}/v1/public-node",origin.trim_end_matches('/'))).call().ok()?;
            let bytes=r.body_mut().with_config().limit(16384).read_to_vec().ok()?;
            let v:Value=serde_json::from_slice(&bytes).ok()?;
            if v["protocol"]!=1 || v["checked_at"].as_u64().is_none_or(|t|t>now()+5 || now().saturating_sub(t)>60) {return None}
            Some(v)
        })();
        nodes.push(json!({"url":origin,"reachable":value.is_some(),"node":value,
            "assurance":assurance(origin,&std::env::var("GAP_PUBLIC_ELESTIO_NODES").unwrap_or_default()),
            "latency_ms":started.elapsed().as_millis() as u64,"measurement_origin":field("GAP_FLEET_NODE_ID"),
            "measured_at":now()}));
    }
    let value=json!({"nodes":nodes,"checked_at":now(),"max_age_seconds":30});
    *cache=Some((Instant::now(),value.clone()));value
}

#[cfg(test)]
mod tests {
    #[test]
    fn provider_claims_require_explicit_origin_listing() {
        let known="https://node.example";
        assert!(super::assurance(known, "")["provider"].is_null());
        assert!(super::assurance("https://node.example.attacker.test", known)["provider"].is_null());
        let value=super::assurance(known, "https://node.example/");
        assert_eq!(value["provider"]["name"], "Elestio");
        assert_eq!(value["provider"]["reports_reviewed"], false);
        assert_eq!(value["identity_status"], "not_independently_verified");
    }
}
