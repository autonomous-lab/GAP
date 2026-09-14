//! Public Cloud product boundary. Historical protocol code is not exposed.

pub fn allowed_api(path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    if matches!(path,"/v1/fleet/browser-session" | "/v1/fleet/connect" | "/v1/fleet/login" | "/v1/fleet/login/verify" | "/v1/fleet/migration-worker") || crate::fleet_access::relay_path("GET",path).is_some() || crate::fleet_access::relay_path("POST",path).is_some() {return true}
    matches!(path, "/health" | "/v1/registration" | "/v1/identity" | "/v1/identity/verify" | "/v1/identity/email" | "/v1/identity/email/verify" | "/v1/cloud/projects" | "/v1/fleet/node" | "/v1/fleet/node-finance" | "/v1/pricing" | "/v1/public-node" | "/v1/explorer")
        || path.starts_with("/v1/cloud/projects/")
        || path.starts_with("/v1/admin/cloud/projects/")
        || path.starts_with("/functions/")
        || matches!(
            path,
            "/internal/tls/ask"
                | "/internal/workload-policy"
                | "/internal/compose/authorize"
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
        "/" => Some(("text/html; charset=utf-8", product_page(HOME, path))),
        "/explorer" => Some(("text/html; charset=utf-8", product_page(include_str!("ui/cloud_explorer.html"), path))),
        "/pricing" => Some(("text/html; charset=utf-8", product_page(include_str!("ui/cloud_pricing.html"), path))),
        "/account" => Some(("text/html; charset=utf-8", product_page(include_str!("ui/cloud_account.html"), path))),
        "/signup" => Some(("text/html; charset=utf-8", product_page(include_str!("ui/cloud_signup.html"), path))),
        "/microvms/assets/xterm.js" => Some(("text/javascript; charset=utf-8", include_str!("ui/vendor/xterm.js").into())),
        "/microvms/assets/xterm-fit.js" => Some(("text/javascript; charset=utf-8", include_str!("ui/vendor/xterm-fit.js").into())),
        "/microvms/assets/xterm.css" => Some(("text/css; charset=utf-8", include_str!("ui/vendor/xterm.css").into())),
        "/microvms" => Some(("text/html; charset=utf-8", product_page(include_str!("ui/cloud_microvms.html"), path))),
        "/docs" | "/for-agents" | "/for-humans" | "/how-it-works" => Some(("text/html; charset=utf-8", product_page(documentation(), "/docs"))),
        "/.well-known/gap-agent.json" => Some(("application/json", "{\"name\":\"GAP Cloud\",\"description\":\"Application infrastructure for AI agents\",\"documentation\":\"/agents.md\",\"projects\":\"/v1/cloud/projects\"}".into())),
        _ => None,
    }
}

fn product_page(source: &str, active: &str) -> String {
    // The public /microvms route embeds this console from the admin origin.
    // Navigation must replace the outer page; otherwise CSP correctly blocks
    // pages such as /account from being rendered inside that iframe.
    let navigation_target = if active == "/microvms" {
        " target=\"_top\""
    } else {
        ""
    };
    let links = [("/", "Home"), ("/explorer", "Explore"), ("/pricing", "Pricing"),
        ("/microvms", "MicroVMs"), ("/docs", "Docs"), ("/account", "Account")];
    let links = links.iter().map(|(href, label)| format!(
        "<a href=\"{href}\"{navigation_target}{}>{label}</a>",
        if *href == active { " aria-current=\"page\"" } else { "" }
    )).collect::<String>();
    let wordmark = include_str!("ui/gap_wordmark.svg");
    let navigation = format!(r#"<header class="gap-header"><div class="gap-header-inner">
<a class="gap-brand" href="/"{navigation_target} aria-label="GAP Cloud home">{wordmark}</a>
<nav class="gap-desktop" aria-label="Main navigation">{links}</nav>
<div class="gap-mobile"><details><summary>Menu</summary><nav aria-label="Mobile navigation">{links}</nav></details></div>
</div></header>"#);
    let variant = if active == "/" { "gap-home" } else if active == "/microvms" { "gap-compute" } else { "gap-product" };
    source.replace("<!-- GAP-WORDMARK -->", wordmark).replace("<!-- GAP-NAV -->", &navigation)
        .replace("<!-- GAP-DESIGN -->", &format!("<style>{}</style>", include_str!("ui/cloud_design.css")))
        .replace("<html lang=\"en\">", &format!("<html lang=\"en\" class=\"{variant}\">"))
}

const HOME: &str = include_str!("ui/cloud_home.html");

fn render_markdown(source: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut headings = Vec::new();
    let mut heading = None;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => heading = Some((level, String::new())),
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, title)) = &mut heading {
                    title.push_str(&text);
                }
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(value) = heading.take() {
                    headings.push(value);
                }
            }
            _ => {}
        }
    }
    let parser = Parser::new_ext(
        source,
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .map(|event| match event {
        // The input is repository-owned, but keep raw HTML inert anyway.
        Event::Html(text) | Event::InlineHtml(text) => Event::Text(text),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => {
            let url = if let Some(path) = dest_url.strip_prefix("./") {
                format!("https://github.com/autonomous-lab/GAP/blob/main/{path}").into()
            } else {
                dest_url
            };
            Event::Start(Tag::Link {
                link_type,
                dest_url: url,
                title,
                id,
            })
        }
        other => other,
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    let mut seen = std::collections::HashMap::<String, usize>::new();
    let mut old_index = 0;
    for (level, title) in headings {
        let base = title
            .split(" — ")
            .next()
            .unwrap_or(&title)
            .to_ascii_lowercase();
        let slug = base
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug = if slug.is_empty() {
            "heading".to_string()
        } else {
            slug
        };
        let count = seen.entry(slug.clone()).or_default();
        *count += 1;
        let id = if *count == 1 {
            slug
        } else {
            format!("{slug}-{count}")
        };
        // Keep existing positional bookmarks working, but use named anchors
        // for new links. Both are emitted server-side, including without JS.
        let alias = if matches!(
            level,
            pulldown_cmark::HeadingLevel::H2 | pulldown_cmark::HeadingLevel::H3
        ) {
            let value = format!("<span id=\"section-{old_index}\" aria-hidden=\"true\"></span>");
            old_index += 1;
            value
        } else {
            String::new()
        };
        html = html.replacen(
            &format!("<{level}>"),
            &format!("{alias}<{level} id=\"{id}\">"),
            1,
        );
    }
    html
}

fn documentation() -> &'static str {
    static PAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PAGE.get_or_init(|| {
        include_str!("ui/cloud_docs.html").replace(
            "<!-- CONTENT -->",
            &render_markdown(include_str!("../AGENTS.md")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn documentation_renders_markdown_and_preserves_raw_agent_instructions() {
        let html = page("/docs").unwrap().1;
        assert!(html.contains("id=\"gap-cloud\">GAP Cloud"));
        assert!(html.contains("id=\"projects\">Projects"));
        assert!(html.contains("<pre><code class=\"language-bash\">"));
        assert!(html.contains("https://github.com/autonomous-lab/GAP/blob/main/sdk/realtime.js"));
        assert_eq!(page("/agents.md").unwrap().1, include_str!("../AGENTS.md"));
        let sample = render_markdown(
            "# Title\n\n**bold**\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n<script>alert(1)</script>",
        );
        assert!(sample.contains("<strong>bold</strong>"));
        assert!(sample.contains("<table>"));
        assert!(!sample.contains("<script>"));
    }
    #[test]
    fn feature_links_have_server_rendered_documentation_targets() {
        let docs = page("/docs").unwrap().1;
        for fragment in HOME.split("href=\"/docs#").skip(1) {
            let id = fragment.split('"').next().unwrap();
            assert!(
                docs.contains(&format!("id=\"{id}\"")),
                "missing anchor {id}"
            );
        }
        for id in [
            "sqlite",
            "functions",
            "websocket",
            "private-static-site",
            "custom-site-domains",
            "section-0",
        ] {
            assert!(docs.contains(&format!("id=\"{id}\"")), "{id}");
        }
        let duplicate = render_markdown("## Example\n\n## Example\n");
        assert!(duplicate.contains("id=\"example-2\""));
    }

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
            "/v1/fleet/node-finance-evil",
        ] {
            assert!(!allowed_api(path), "{path}");
        }
        for path in [
            "/health",
            "/v1/identity",
            "/v1/fleet/node-finance?start=0&end=3600",
            "/v1/cloud/projects",
            "/v1/cloud/projects/prj_a/realtime/tokens",
            "/functions/prj_a/api",
        ] {
            assert!(allowed_api(path), "{path}");
        }
        assert!(page("/activity").is_none());
        assert!(page("/").unwrap().1.contains("GAP Cloud"));
    }

    #[test]
    fn account_opens_microvm_management_as_a_dedicated_route() {
        let (_, account) = page("/account").unwrap();
        assert!(!account.contains("machineFrame"));
        assert!(!account.contains("<iframe"));
        assert!(account.contains("location.assign(path+'?workspace=1')"));
        assert!(account.contains("revokeMachineSessions"));

        let (_, console) = page("/microvms").unwrap();
        assert!(console.contains("id=\"workspace-back\""));
        assert!(console.contains("renewWorkspaceSession"));
        assert!(console.contains("'/v1'+'/fleet/project-token'"));
    }

    #[test]
    fn microvm_navigation_escapes_the_public_wrapper_iframe() {
        let (_, console) = page("/microvms").unwrap();
        assert!(console.contains("<a href=\"/account\" target=\"_top\">Account</a>"));
        assert!(console.contains("class=\"gap-brand\" href=\"/\" target=\"_top\""));

        let (_, account) = page("/account").unwrap();
        assert!(account.contains("<a href=\"/account\" aria-current=\"page\">Account</a>"));
        assert!(!account.contains("target=\"_top\""));
    }
}
