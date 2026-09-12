//! Read-only finance exchange between explicitly configured operator services.
use std::sync::{Arc,Mutex};
use serde_json::{json,Value};
use crate::server::NodeState;
fn denied(code:&str)->(u16,Value){(403,json!({"error":{"code":code}}))}
fn param<'a>(path:&'a str,key:&str)->Option<&'a str>{path.split_once('?')?.1.split('&').find_map(|p|p.split_once('=').filter(|(k,_)|*k==key).map(|(_,v)|v))}
fn id(s:&str,prefix:&str,len:usize)->bool{s.strip_prefix(prefix).is_some_and(|v|v.len()==len&&v.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b)))}
fn window(start:u64,end:u64)->bool{start%3600==0&&end%3600==0&&end>start&&end-start<=31*86400&&end<=crate::message::now_unix()/3600*3600}
pub fn node_report(state:&Arc<Mutex<NodeState>>,method:&str,path:&str,auth:Option<&str>)->Option<(u16,Value)>{
    if path.split('?').next()!=Some("/v1/fleet/node-finance"){return None}
    let expected=std::env::var("GAP_FLEET_REPORT_TOKEN").unwrap_or_default();
    let supplied=auth.and_then(|s|s.strip_prefix("Bearer ")).unwrap_or("");
    if expected.len()<43||crate::sha256_hex(expected.as_bytes())!=crate::sha256_hex(supplied.as_bytes()){return Some(denied("finance_service_required"))}
    if method!="GET"{return Some(denied("finance_read_only"))}
    let start=param(path,"start").and_then(|s|s.parse::<u64>().ok()).unwrap_or(0);
    let end=param(path,"end").and_then(|s|s.parse::<u64>().ok()).unwrap_or(0);
    let project=param(path,"project_id");
    if !window(start,end)||project.is_some_and(|p|!id(p,"prj_",24)){return Some((400,json!({"error":{"code":"invalid_finance_window_or_project"}})))}
    let runner=state.lock().ok().and_then(|s|s.private_node.as_ref().and_then(|p|p.runner.clone()));
    Some(match runner {Some(r)=>crate::private_node::forward(&r,"","","GET","admin/finance",json!({"start":start,"end":end,"project_id":project,"include_projects":true})),None=>(503,json!({"available":false}))})
}
pub fn fleet_report(start:u64,end:u64,project:Option<&str>,customer:Option<&str>,node:Option<&str>)->Option<(u16,Value)>{
    let target=std::env::var("GAP_FLEET_FINANCE_URL").ok().filter(|s|!s.is_empty())?;
    let token=std::env::var("GAP_FLEET_FINANCE_TOKEN").unwrap_or_default();
    let unavailable=||(503,json!({"available":false,"error":{"code":"fleet_finance_unavailable"}}));
    if !window(start,end)||project.is_some_and(|p|!id(p,"prj_",24))||customer.is_some_and(|p|!id(p,"cus_",32))||node.is_some_and(|p|p.is_empty()||p.len()>100||!p.bytes().all(|b|b.is_ascii_alphanumeric()||b"_.-".contains(&b))){return Some((400,json!({"error":{"code":"invalid_finance_filter"}})))}
    let parsed=target.parse::<ureq::http::Uri>().ok();
    if !parsed.is_some_and(|u|u.path()=="/v1/finance"&&u.query().is_none()&&!u.authority().is_some_and(|a|a.as_str().contains('@'))&&(u.scheme_str()==Some("https")||(u.scheme_str()==Some("http")&&matches!(u.host(),Some("127.0.0.1"|"localhost"|"172.17.0.1")))))||token.len()<43{return Some(unavailable())}
    let mut url=format!("{target}?start={start}&end={end}");
    for (key,value) in [("project_id",project),("customer_id",customer),("node_id",node)]{if let Some(v)=value{url.push_str(&format!("&{key}={v}"))}}
    let result=(||{
        let client=ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(12))).max_redirects(0).http_status_as_error(false).build().new_agent();
        let mut response=client.get(&url).header("Authorization",&format!("Bearer {token}")).header("User-Agent","GAP-Finance/1.0").call().ok()?;
        let status=response.status().as_u16();let bytes=response.body_mut().with_config().limit(4*1024*1024).read_to_vec().ok()?;
        let value:Value=serde_json::from_slice(&bytes).ok()?;Some((status,value))
    })();
    Some(result.unwrap_or_else(unavailable))
}
