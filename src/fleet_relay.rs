//! HTTPS edge relay to the private operator authority's node-only endpoint.
//! The destination is fixed by the server, never selected by caller input.
use serde_json::{json,Value};

pub fn forward(target:&str,method:&str,auth:Option<&str>,body:&[u8])->(u16,Value) {
    if method!="POST" {return (405,json!({"error":{"code":"method_not_allowed"}}))}
    forward_client(target,method,auth,body)
}

pub fn forward_client(target:&str,method:&str,auth:Option<&str>,body:&[u8])->(u16,Value) {
    if !matches!(method,"GET"|"POST") {return (405,json!({"error":{"code":"method_not_allowed"}}))}
    if body.len()>65536 {return (413,json!({"error":{"code":"request_too_large"}}))}
    let Some(token)=auth.and_then(|s|s.strip_prefix("Bearer ")).filter(|s|
        (43..=128).contains(&s.len()) && s.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'_' || b==b'-'))
    else {return (401,json!({"error":{"code":"node_credentials_required"}}))};
    let agent=ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(2)))
        .max_redirects(0).http_status_as_error(false).build().new_agent();
    let response=(|| {
        let mut response=if method=="GET" {
            agent.get(target).header("Authorization",&format!("Bearer {token}")).header("User-Agent","GAP-Identity/1.0").call().ok()?
        } else {
            agent.post(target).header("Authorization",&format!("Bearer {token}"))
                .header("User-Agent","GAP-Identity/1.0").header("Content-Type","application/json").send(body).ok()?
        };
        let status=response.status().as_u16();
        if !(200..300).contains(&status) && ![400,401,402,403,404,409,413,429,503].contains(&status) {return None}
        let bytes=response.body_mut().with_config().limit(65536).read_to_vec().ok()?;
        let value:Value=serde_json::from_slice(&bytes).ok()?;
        if !value.is_object(){return None}
        Some((status,value))
    })();
    response.unwrap_or_else(||(503,json!({"error":{"code":"fleet_authority_unavailable"}})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn missing_credentials_and_oversized_requests_never_contact_upstream() {
        assert_eq!(forward("http://127.0.0.1:1/node","GET",None,b"").0,405);
        assert_eq!(forward("http://127.0.0.1:1/node","POST",None,b"{}").0,401);
        assert_eq!(forward("http://127.0.0.1:1/node","POST",Some("Bearer bad\r\nHeader:x"),b"{}").0,401);
        assert_eq!(forward("http://127.0.0.1:1/node","POST",None,&vec![0;65537]).0,413);
    }
    #[test] fn preserves_node_auth_and_application_denial_without_following_redirects() {
        for (status,payload) in [(403,"{\"error\":{\"code\":\"project_node_mismatch\"}}"),(302,"{}"),(200,"not-json")] {
            let server=tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url=format!("http://{}/node",server.server_addr());
            let token="n".repeat(64);
            let expected=format!("Bearer {token}");
            let thread=std::thread::spawn(move|| {
                let mut request=server.recv().unwrap();
                assert_eq!(request.url(),"/node");
                assert!(request.headers().iter().any(|h|h.field.equiv("Authorization") && h.value.as_str()==expected));
                let mut body=String::new();request.as_reader().read_to_string(&mut body).unwrap();
                assert_eq!(body,"{\"action\":\"project\"}");
                request.respond(tiny_http::Response::from_string(payload).with_status_code(status)
                    .with_header(tiny_http::Header::from_bytes("Location","http://127.0.0.1:1/never").unwrap())).unwrap();
            });
            let result=forward(&url,"POST",Some(&format!("Bearer {token}")),b"{\"action\":\"project\"}");
            assert_eq!(result.0,if status==403 {403}else{503});
            thread.join().unwrap();
        }
    }
}
