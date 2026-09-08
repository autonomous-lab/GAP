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

const HOME: &str = include_str!("ui/cloud_home.html");

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn landing_has_accessible_navigation_and_local_artwork() {
        let (_, home) = page("/").unwrap();
        for expected in [
            "mobile-menu",
            "Skip to content",
            "prefers-reduced-motion",
            "aria-live=\"polite\"",
            "role=\"tablist\"",
            "ILLUSTRATIVE WORKFLOW",
            "cloud-core-v2.webp",
        ] {
            assert!(home.contains(expected), "missing {expected}");
        }
        let (media_type, image) = crate::server::static_asset("/cloud-core-v2.webp").unwrap();
        assert_eq!(media_type, "image/webp");
        assert!(
            image.len() < 200_000,
            "hero image should remain lightweight"
        );
    }
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
