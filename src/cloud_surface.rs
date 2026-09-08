//! Public Cloud product boundary. Historical protocol code is not exposed.

pub fn allowed_api(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    matches!(path, "/health" | "/v1/identity" | "/v1/cloud/projects")
        || path.starts_with("/v1/cloud/projects/")
        || path.starts_with("/v1/admin/cloud/projects/")
        || path.starts_with("/functions/")
        || matches!(
            path,
            "/internal/tls/ask"
                | "/internal/functions/capability"
                | "/internal/realtime/custom-domain"
                | "/internal/realtime/credits/spend"
        )
}

pub fn page(path: &str) -> Option<(&'static str, String)> {
    let path = path.split('?').next().unwrap_or(path);
    match path {
        "/sitemap.xml" => {
            let origin = std::env::var("GAP_PUBLIC_URL").unwrap_or_else(|_| "http://localhost:8080".into());
            let origin = origin.trim_end_matches('/').replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            Some(("application/xml", format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\"><url><loc>{origin}/</loc></url><url><loc>{origin}/docs</loc></url></urlset>")))
        }
        "/agents.md" | "/AGENTS.md" | "/llms.txt" => Some(("text/plain; charset=utf-8", include_str!("../AGENTS.md").into())),
        "/robots.txt" => Some(("text/plain", "User-agent: *\nAllow: /\nDisallow: /v1/\nDisallow: /internal/\nDisallow: /sites/\n".into())),
        "/" => Some(("text/html; charset=utf-8", HOME.into())),
        "/docs" | "/for-agents" | "/for-humans" | "/how-it-works" => Some(("text/html; charset=utf-8", format!(
            "<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>GAP Cloud documentation</title><style>body{{background:#080e1a;color:#e5edf9;font:16px/1.6 system-ui;max-width:1000px;margin:auto;padding:32px}}a{{color:#69e2cd}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:14px/1.7 monospace}}</style><nav><a href='/'>GAP Cloud</a> · <a href='/agents.md'>Download agent instructions</a></nav><h1>Build with GAP Cloud</h1><pre>{}</pre></html>",
            include_str!("../AGENTS.md").replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        ))),
        "/.well-known/gap-agent.json" => Some(("application/json", "{\"name\":\"GAP Cloud\",\"description\":\"Application infrastructure for AI agents\",\"documentation\":\"/agents.md\",\"projects\":\"/v1/cloud/projects\"}".into())),
        _ => None,
    }
}

const HOME: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>GAP Cloud — Your agent builds. GAP runs it.</title>
<meta name="description" content="Deploy agent-built apps with databases, functions, static sites, custom domains and realtime. One project. One API.">
<meta property="og:title" content="GAP Cloud — Your agent builds. GAP runs it.">
<meta property="og:description" content="Data, functions, sites, custom domains and realtime. Application infrastructure for AI agents.">
<meta property="og:type" content="website">
<style>
*{box-sizing:border-box}body{margin:0;background:#070d19;color:#eef4ff;font:17px/1.6 system-ui,sans-serif}a{color:inherit;text-decoration:none}main,nav,footer{max-width:1180px;margin:auto;padding:24px}nav{display:flex;align-items:center;justify-content:space-between}.brand{font-size:24px;font-weight:800}.brand span,.eyebrow{color:#69e2cd}.links{display:flex;gap:24px;color:#b5c4dc}.hero{padding:70px 0 40px;max-width:850px}.eyebrow{font-size:13px;letter-spacing:2px;text-transform:uppercase}h1{font-size:clamp(44px,7vw,86px);line-height:1.05;letter-spacing:-3px;margin:20px 0}h1 span{color:#69e2cd}h2{font-size:34px;line-height:1.2}p{color:#b5c4dc}.lead{font-size:22px;max-width:710px}.actions{display:flex;gap:14px;flex-wrap:wrap;margin:30px 0}.button{border:1px solid #33455f;padding:12px 22px;border-radius:9px}.primary{background:#69e2cd;color:#07121b;border:0;font-weight:700}.art{display:block;width:100%;height:auto;border-radius:20px;border:1px solid #24324a}.grid{display:grid;grid-template-columns:repeat(3,1fr);gap:18px;margin:30px 0 60px}.card{background:#101a2c;border:1px solid #24324a;border-radius:14px;padding:24px}.card h3{margin:0;font-size:22px}.card p{margin-bottom:0}.workflow{display:grid;grid-template-columns:1fr 1fr;gap:32px;align-items:center;margin:50px 0}pre{background:#101a2c;padding:24px;border-radius:14px;overflow:auto;color:#9ef0d8;font-size:14px}footer{border-top:1px solid #24324a;color:#94a7c4;margin-top:60px}section{scroll-margin-top:20px}@media(max-width:760px){.grid,.workflow{grid-template-columns:1fr}.hero{padding-top:35px}.links{gap:12px;font-size:14px}h1{letter-spacing:-2px}}
</style></head><body>
<nav><a class="brand" href="/">GAP <span>Cloud</span></a><div class="links"><a href="#services">Services</a><a href="/docs">Documentation</a><a href="/agents.md">For agents ↗</a></div></nav>
<main><section class="hero"><div class="eyebrow">Application infrastructure for AI agents</div><h1>Your agent builds.<br><span>GAP runs it.</span></h1><p class="lead">From an idea to a working application. Give your agent data, compute, hosting and realtime through one project-scoped API.</p><div class="actions"><a class="button primary" href="/docs">Build your first app</a><a class="button" href="/agents.md">Give these instructions to your agent</a></div></section>
<img class="art" src="/agent-cloud-565f3ea9fc57.webp" alt="A compute core connecting the services of an agent cloud project" width="1600" height="759">
<section id="services"><h2>A complete backend.<br>Provisioned by your agent.</h2><div class="grid">
<article class="card"><h3>Persistent data</h3><p>KV for state, objects for files and an isolated SQLite database for structured application data.</p></article>
<article class="card"><h3>Secure functions</h3><p>Version JavaScript, pass publication review and run in a constrained sandbox. Connect to approved external APIs.</p></article>
<article class="card"><h3>Sites & domains</h3><p>Publish an atomic site release. Keep it private or attach a verified custom domain with automatic HTTPS.</p></article>
<article class="card"><h3>Realtime</h3><p>Build chat, live dashboards and collaborative apps with scoped WebSocket tokens and message replay.</p></article>
<article class="card"><h3>Scheduled work</h3><p>Refresh application data and run recurring functions with project-managed schedules.</p></article>
<article class="card"><h3>Controlled growth</h3><p>Start with free resource allowances. Operator-funded realtime credits unlock higher usage within documented limits.</p></article>
</div></section><section class="workflow"><div><div class="eyebrow">One identity. One project. One API.</div><h2>Built for agents<br>that ship applications.</h2><p>Create a project, store data and deploy your application. Every operation is available over HTTP, with copyable examples and explicit limits.</p><a class="button primary" href="/docs">Explore the API</a></div><pre># Create your agent identity
POST /v1/identity

# Provision your application backend
POST /v1/cloud/projects
Authorization: Bearer gat_...

# Store application state
PUT /v1/cloud/projects/{id}/kv/config

# Deploy a function
POST /v1/cloud/projects/{id}/functions/api</pre></section>
<section><h2>Your project has boundaries.</h2><p>Owner-scoped data. Short-lived browser tokens. Reviewed function releases. Controlled outbound HTTP. Explicit resource limits.</p><p>Self-host GAP Cloud or connect your agents to this node. Read the documentation for the exact security model and available services.</p></section></main>
<footer>GAP Cloud · Built for agent-created applications. <a href="/docs">Documentation</a> · <a href="/health">Health</a></footer></body></html>"##;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_boundary_excludes_legacy_and_prefix_lookalikes() {
        for path in [
            "/v1/contract/propose",
            "/v1/balance/withdraw",
            "/v1/events",
            "/v1/discover",
            "/x402/a",
            "/v1/cloud/projects-evil",
        ] {
            assert!(!allowed_api(path), "{path}");
        }
        for path in [
            "/health",
            "/v1/identity",
            "/v1/cloud/projects",
            "/v1/cloud/projects/prj_a/realtime/tokens",
            "/functions/prj_a/api",
        ] {
            assert!(allowed_api(path), "{path}");
        }
        assert!(page("/activity").is_none());
        assert!(page("/").unwrap().1.contains("GAP Cloud"));
    }
}
