//! Browser sessions for the VM console. The bearer stays server-side.
use std::{collections::HashMap, sync::{Mutex, OnceLock}, time::{Duration, Instant}};
use serde_json::{json, Value};
const COOKIE: &str = "__Secure-GAPVM";
struct Session { project: String, token: String, csrf: String, expires: Instant }
static SESSIONS: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
fn sessions() -> &'static Mutex<HashMap<String,Session>> { SESSIONS.get_or_init(||Mutex::new(HashMap::new())) }
fn cookie_id(cookies: &str) -> Option<&str> {
    let mut values=cookies.split(';').filter_map(|c|c.trim().strip_prefix("__Secure-GAPVM="));
    let id=values.next()?; if values.next().is_some() {return None} Some(id)
}
fn cookie(value: &str, clear: bool) -> String {
    format!("{COOKIE}={value}; Path=/v1/cloud/projects; Secure; HttpOnly; SameSite=Strict{}",if clear {"; Max-Age=0"} else {""})
}
pub fn issue(project: &str, token: &str) -> Result<(Value,String), ()> {
    let mut all=sessions().lock().map_err(|_|())?;
    all.retain(|_,s|s.expires>Instant::now());
    if all.len()>=10000 {return Err(())}
    let id=crate::new_id("session"); let csrf=crate::new_id("csrf");
    all.insert(id.clone(),Session{project:project.into(),token:token.into(),csrf:csrf.clone(),expires:Instant::now()+Duration::from_secs(8*3600)});
    Ok((json!({"project_id":project,"csrf":csrf}),cookie(&id,false)))
}
pub fn console_path(path: &str) -> bool {
    let path=path.split('?').next().unwrap_or(path);
    if path=="/microvms" {return true}
    let Some(rest)=path.strip_prefix("/v1/cloud/projects/") else {return false};
    let Some((project,action))=rest.split_once('/') else {return false};
    project.len()==28 && project.starts_with("prj_") && project[4..].bytes().all(|c|c.is_ascii_hexdigit()) &&
        (action=="vms" || action=="vm" || action.starts_with("vm/") || action=="access-requests" || action=="browser-session")
}
pub fn authorization(path: &str, cookies: &str, csrf: &str) -> Option<String> {
    let all=sessions().lock().ok()?;
    let session=all.get(cookie_id(cookies)?)?;
    let prefix=format!("/v1/cloud/projects/{}/",session.project);
    if session.expires<=Instant::now() || csrf.is_empty() || session.csrf!=csrf || !path.starts_with(&prefix) {return None}
    // Never let this cookie authorize arbitrary Cloud application data APIs.
    let action=path[prefix.len()..].split('?').next()?;
    if !(action=="vms" || action=="vm" || action.starts_with("vm/") || action=="access-requests" || action=="browser-session") {return None}
    Some(session.token.clone())
}
pub fn revoke(cookies: &str) -> String {
    if let Some(id)=cookie_id(cookies) {if let Ok(mut all)=sessions().lock() {all.remove(id);}}
    cookie("",true)
}
#[cfg(test)] mod tests {
    use super::*;
    #[test] fn scoped_csrf_and_revocation() {
        let (body,set)=issue("prj_test","Bearer test").unwrap(); let c=set.split(';').next().unwrap(); let csrf=body["csrf"].as_str().unwrap();
        assert_eq!(authorization("/v1/cloud/projects/prj_test/vms",c,csrf).as_deref(),Some("Bearer test"));
        for path in ["/v1/cloud/projects/prj_other/vms","/v1/cloud/projects/prj_test/kv","/v1/cloud/projects/prj_test/vm-evil"] {assert!(authorization(path,c,csrf).is_none());}
        assert!(authorization("/v1/cloud/projects/prj_test/vms",c,"").is_none());
        assert!(authorization("/v1/cloud/projects/prj_test/vms",&format!("{c}; {c}"),csrf).is_none());
        assert!(set.contains("HttpOnly")); assert!(!set.contains("Max-Age"));
        revoke(c); assert!(authorization("/v1/cloud/projects/prj_test/vms",c,csrf).is_none());
    }
}
