//! Exercise the real production binary, not the archived library router.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Node {
    child: Child,
    root: std::path::PathBuf,
}
impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn active_agent_examples_stay_inside_the_cloud_boundary() {
    let doc = include_str!("../AGENTS.md");
    let mut checked = 0;
    for fragment in doc.split("$NODE").skip(1) {
        let path = fragment.split(['\"', '\'', ' ', '\n', '`']).next().unwrap();
        if !path.starts_with('/') {
            continue;
        }
        assert!(
            gap::cloud_surface::allowed_api(path) || path.starts_with("/sites/"),
            "{path}"
        );
        checked += 1;
    }
    assert!(checked >= 25, "Cloud examples missing: {checked}");
}

fn request(port: u16, method: &str, path: &str, headers: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    (
        status,
        response.split_once("\r\n\r\n").unwrap().1.to_string(),
    )
}

#[test]
fn cloud_binary_preserves_project_api_and_closes_commerce_surface() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let root = std::env::temp_dir().join(format!(
        "gap-cloud-surface-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir(&root).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_gap"))
        .env_clear()
        .env("GAP_ADDR", format!("127.0.0.1:{port}"))
        .env("GAP_STORAGE", "sqlite")
        .env("GAP_SQLITE_PATH", ":memory:")
        .env("GAP_CLOUD_ROOT", &root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _node = Node { child, root };
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "node did not start");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(request(port, "GET", "/health", "", "").0, 200);
    let home = request(port, "GET", "/", "", "");
    assert_eq!(home.0, 200);
    assert!(home.1.contains("Your agent builds."));
    for path in [
        "/docs",
        "/agents.md",
        "/llms.txt",
        "/.well-known/gap-agent.json",
    ] {
        assert_eq!(request(port, "GET", path, "", "").0, 200, "{path}");
    }
    for path in [
        "/v1/contract/propose",
        "/v1/escrow/park",
        "/v1/balance",
        "/v1/announce",
        "/v1/discover",
        "/v1/events",
        "/v1/activity/stream",
        "/x402/test",
        "/admin",
        "/activity",
    ] {
        assert_eq!(
            request(port, "GET", path, "Accept: text/event-stream\r\n", "").0,
            410,
            "{path}"
        );
        assert_eq!(
            request(port, "POST", path, "X-GAP-Custom-Domain: 1\r\n", "{}").0,
            410,
            "custom {path}"
        );
    }
    assert_eq!(
        request(port, "GET", "/", "X-GAP-Custom-Domain: 1\r\n", "").0,
        404
    );
    let identity = request(port, "POST", "/v1/identity", "", "{}");
    assert_eq!(identity.0, 200);
    let identity: serde_json::Value = serde_json::from_str(&identity.1).unwrap();
    let auth = format!(
        "Authorization: Bearer {}\r\n",
        identity["token"].as_str().unwrap()
    );
    let project = request(port, "POST", "/v1/cloud/projects", &auth, "{}");
    assert_eq!(project.0, 200);
    let project: serde_json::Value = serde_json::from_str(&project.1).unwrap();
    let id = project["project_id"].as_str().unwrap();
    let path = format!("/v1/cloud/projects/{id}/kv/pivot");
    assert_eq!(
        request(port, "PUT", &path, &auth, r#"{"value_base64":"Y2xvdWQ="}"#).0,
        200
    );
    let read = request(port, "GET", &path, &auth, "");
    assert_eq!(read.0, 200);
    assert!(
        read.1.contains("Y2xvdWQ="),
        "unexpected KV result: {}",
        read.1
    );
    assert_ne!(request(port, "GET", &path, "", "").0, 200);
}
