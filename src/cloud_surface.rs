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
        "/" => Some(("text/html; charset=utf-8", HOME.into())),
        "/microvms" => Some(("text/html; charset=utf-8", include_str!("ui/cloud_microvms.html").into())),
        "/docs" | "/for-agents" | "/for-humans" | "/how-it-works" => Some(("text/html; charset=utf-8", documentation().to_string())),
        "/.well-known/gap-agent.json" => Some(("application/json", "{\"name\":\"GAP Cloud\",\"description\":\"Application infrastructure for AI agents\",\"documentation\":\"/agents.md\",\"projects\":\"/v1/cloud/projects\"}".into())),
        _ => None,
    }
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
