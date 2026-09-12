//! Cookie admission for ttyd resources on the isolated management origin.
use super::*;
pub fn admit_terminal(state:&Arc<Mutex<NodeState>>,secret:&str,host:&str,path:&str,cookies:&str,origin:&str)->u16 {
    let expected=std::env::var("GAP_VM_EDGE_TOKEN").unwrap_or_default();
    let admin=std::env::var("GAP_ADMIN_ORIGIN").unwrap_or_default();
    let admin=admin.trim_end_matches('/');
    let admin_host=admin.strip_prefix("https://").unwrap_or("");
    if expected.len()<32 || crate::sha256_hex(secret.as_bytes())!=crate::sha256_hex(expected.as_bytes()) || admin_host.is_empty() || !host.eq_ignore_ascii_case(admin_host) || (!origin.is_empty() && origin!=admin) {return 403}
    let Some((project,_))=crate::cloud_vm_session::terminal_view(path) else{return 403};
    let Some(token)=crate::cloud_vm_session::authorization(path,cookies,"") else{return 403};
    let Some(token)=token.strip_prefix("Bearer ") else{return 403};
    let guard=match state.lock(){Ok(g)=>g,Err(_)=>return 503};
    let project=match guard.cloud_owned_project(token,project){Ok(p)=>p,Err(_)=>return 403};
    if !guard.private_node.as_ref().is_some_and(|p|p.runner.is_some() && p.authorize_compose(&project.owner_did).is_ok()) {return 403}
    // Worker checks this ticket's project, live VM, policy and credits itself.
    // No synchronous callback cycle between the node and its worker.
    204
}
