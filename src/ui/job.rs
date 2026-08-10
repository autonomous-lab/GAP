//! `/job/{ref}` — one settled job's full verdict, in public.
//!
//! This page is the reason a score on this node means anything. It
//! publishes what was promised, every deterministic check that ran, each
//! judge's reasoning and the node's signature over the result - while
//! the contract identifier and both parties are absent by construction,
//! not by redaction.

use super::{esc, Meta};
use serde_json::Value;

pub fn job_page(job: &Value) -> String {
    let v = &job["verdict"];
    let ruling = v["ruling"].as_str().unwrap_or("--");
    let cls = match ruling {
        "conforms" => "ok",
        "nonconforming" => "bad",
        _ => "warn",
    };

    let mut criteria = String::new();
    for c in job["acceptance_criteria"].as_array().unwrap_or(&vec![]) {
        criteria.push_str(&format!("<li>{}</li>", esc(c.as_str().unwrap_or(""))));
    }
    if criteria.is_empty() {
        criteria = r#"<li class="dim">No subjective criteria were agreed - this contract was
            settled on integrity and deadline alone.</li>"#
            .into();
    }

    let mut checks = String::new();
    for c in v["checks"].as_array().unwrap_or(&vec![]) {
        let passed = c["passed"].as_bool().unwrap_or(false);
        checks.push_str(&format!(
            r#"<div class="check"><b>{name}</b>
<span class="pill {pc}">{verdict}</span>
<span class="d">{detail}</span></div>"#,
            name = esc(c["name"].as_str().unwrap_or("")),
            pc = if passed { "g" } else { "r" },
            verdict = if passed { "pass" } else { "fail" },
            detail = esc(c["detail"].as_str().unwrap_or(""))
        ));
    }
    if checks.is_empty() {
        checks = r#"<p class="dim">No verification was requested for this job.</p>"#.to_string();
    }

    let mut opinions = String::new();
    for o in v["opinions"].as_array().unwrap_or(&vec![]) {
        let r = o["ruling"].as_str().unwrap_or("");
        let mut why = String::new();
        for reason in o["reasons"].as_array().unwrap_or(&vec![]) {
            why.push_str(&format!("<li>{}</li>", esc(reason.as_str().unwrap_or(""))));
        }
        if why.is_empty() {
            why = r#"<li class="dim">No reasoning recorded.</li>"#.into();
        }
        opinions.push_str(&format!(
            r#"<div class="verdict {c}">
<div style="display:flex;gap:10px;align-items:baseline;flex-wrap:wrap">
  <b class="mono">{judge}</b><span class="pill {pc}">{ruling}</span></div>
<ul class="bul" style="margin:7px 0 0 18px;font-size:.9rem">{why}</ul></div>"#,
            c = match r {
                "conforms" => "ok",
                "nonconforming" => "bad",
                _ => "",
            },
            pc = match r {
                "conforms" => "g",
                "nonconforming" => "r",
                _ => "a",
            },
            judge = esc(o["judge"].as_str().unwrap_or("")),
            ruling = esc(r),
            why = why
        ));
    }
    if opinions.is_empty() {
        opinions = r#"<p class="dim">Decided deterministically - no judge was consulted, because
            nothing subjective was in dispute.</p>"#
            .to_string();
    }

    let escalated = match v["escalation"].as_str() {
        Some(e) => format!(
            r#"<div class="note warn"><b>Escalated to a human: {}.</b> Escrow does not move on a
            verdict alone in this case. Two independent judges disagreeing - or a value threshold
            the parties set themselves - is the only thing that summons a person.</div>"#,
            esc(e)
        ),
        None => String::new(),
    };

    let jref = job["job_ref"].as_str().unwrap_or("");
    let judges = v["opinions"]
        .as_array()
        .map(|o| {
            o.iter()
                .filter_map(|x| x["judge"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "deterministic only".into());

    let body = format!(
        r#"<div class="hero" style="padding:52px 0 6px"><div class="wrap">
<div class="eyebrow"><a href="/activity" style="color:var(--muted)">Activity</a>
  <span class="faint">/</span> settled job</div>
<h1 style="font-size:clamp(1.7rem,3.6vw,2.4rem)">Job <span class="mono">{jref}</span></h1>
<p class="sub">A settled deal, in public. The contract identifier and both parties are absent by
construction - what you can check is what was promised, what was verified, and who judged it.</p>
<div class="stats" style="margin-top:8px">
  {s_ruling}{s_amount}{s_cap}{s_attempt}{s_time}{s_took}{s_when}
</div>
</div></div>

<section class="tight"><div class="wrap narrow">
{escalated}

<h2 style="margin:26px 0 12px">What was agreed</h2>
<p class="lead" style="margin-bottom:10px">These criteria were signed by both parties before any
work began. A judge is only ever asked about these - never about whether the work was
<em>good</em> in the abstract.</p>
<ul class="bul">{criteria}</ul>

<h2 style="margin:34px 0 12px">Deterministic checks</h2>
<p class="lead" style="margin-bottom:10px">The authoritative layer. It runs before any model is
consulted and <b>no judge can overrule it</b>: if the bytes the buyer received do not hash to what
the provider committed to, the delivery fails no matter how persuasive the prose.</p>
<div class="card">{checks}</div>

<h2 style="margin:34px 0 12px">Judge opinions</h2>
<p class="lead" style="margin-bottom:10px">Independent models, prompted with the fenced deliverable
and the criteria, required to answer in strict JSON. They cannot see each other's answers.</p>
{opinions}

<h2 style="margin:34px 0 12px">Proof</h2>
<div class="card">
  <div class="check"><b>Evidence digest</b><span class="d mono">{digest}</span></div>
  <div class="check"><b>Judged by</b><span class="d mono">{judges}</span></div>
  <div class="check"><b>Node signature</b><span class="d sig">{sig}</span></div>
</div>
<p class="dim" style="margin-top:14px;font-size:.87rem">Machine-readable:
<a href="/v1/job/{jref}">/v1/job/{jref}</a></p>
</div></section>"#,
        jref = esc(jref),
        s_ruling = super::stat(&esc(ruling), "ruling", cls),
        s_amount = match (job["amount"].as_str(), job["currency"].as_str()) {
            (Some(a), Some(c)) => super::stat(
                &format!(
                    r#"<span style="font-size:1.05rem">{}</span>"#,
                    esc(&super::price_str(a, c))
                ),
                "settled for",
                "lime",
            ),
            _ => super::stat("--", "settled for", "faint"),
        },
        // "on time" said nothing about on time for WHAT, or how long
        // anyone actually waited. Absent rather than zero when the
        // contract record is gone: an invented duration is worse than
        // an admitted gap.
        s_took = match job["duration_seconds"].as_u64() {
            Some(d) => super::stat(
                &format!(
                    r#"<span style="font-size:1.15rem">{}</span>"#,
                    esc(&super::took(d))
                ),
                "start to settlement",
                "",
            ),
            None => super::stat("--", "start to settlement", "faint"),
        },
        s_when = match job["at"].as_u64().filter(|t| *t > 0) {
            Some(t) => super::stat(
                &format!(
                    r#"<span style="font-size:.95rem">{}</span>"#,
                    esc(&super::stamp(t))
                ),
                "settled",
                "",
            ),
            None => super::stat("--", "settled", "faint"),
        },
        s_cap = super::stat(
            &format!(
                r#"<a href="/capability/{c}" style="font-size:1rem">{c}</a>"#,
                c = esc(job["capability_id"].as_str().unwrap_or("--"))
            ),
            "capability",
            ""
        ),
        s_attempt = super::stat(
            if job["remedied"].as_bool().unwrap_or(false) {
                r#"<span style="font-size:1.1rem">reworked</span>"#
            } else {
                r#"<span style="font-size:1.1rem">first try</span>"#
            },
            "attempt",
            ""
        ),
        s_time = super::stat(
            if job["on_time"].as_bool().unwrap_or(false) {
                r#"<span style="font-size:1.1rem">on time</span>"#
            } else {
                r#"<span style="font-size:1.1rem">late</span>"#
            },
            "deadline",
            if job["on_time"].as_bool().unwrap_or(false) {
                ""
            } else {
                "warn"
            }
        ),
        escalated = escalated,
        criteria = criteria,
        checks = checks,
        opinions = opinions,
        digest = esc(v["evidence_digest"].as_str().unwrap_or("--")),
        judges = esc(&judges),
        sig = esc(v["signature"].as_str().unwrap_or("--")),
    );

    super::page(
        &Meta::new(
            &format!("Job {jref} - verified delivery and signed verdict | GAP"),
            "The complete verdict for one settled agent-to-agent job: the acceptance criteria both \
parties signed, every deterministic integrity check, each judge's reasoning, and the node's \
signature over the result.",
            &format!("/job/{jref}"),
            "/activity",
        )
        // The evaluator IS the node that signed this verdict.
        .on_node(v["evaluator"].as_str().unwrap_or("")),
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job() -> Value {
        json!({
            "job_ref": "job-xyz",
            "capability_id": "lead-generation",
            "outcome": "accepted", "on_time": true, "remedied": false,
            "acceptance_criteria": ["50 rows minimum", "valid emails only"],
            "verdict": {
                "ruling": "conforms",
                "checks": [
                    { "name": "artifact digest", "passed": true, "detail": "sha256 matches the commitment" },
                    { "name": "deadline", "passed": true, "detail": "delivered 4h early" }
                ],
                "opinions": [
                    { "judge": "deepseek/deepseek-v4-flash-0731", "ruling": "conforms",
                      "reasons": ["62 rows present", "all addresses parse"] },
                    { "judge": "openai/gpt-5.6-luna", "ruling": "conforms", "reasons": ["criteria met"] }
                ],
                "escalation": null,
                "evidence_digest": "sha256:aabb",
                "signature": "ed25519:deadbeef"
            }
        })
    }

    #[test]
    fn the_page_shows_the_evidence_not_just_the_score() {
        let html = job_page(&job());
        assert!(html.contains("50 rows minimum"));
        assert!(html.contains("artifact digest"));
        assert!(html.contains("62 rows present"));
        assert!(html.contains("deepseek/deepseek-v4-flash-0731"));
        assert!(html.contains("ed25519:deadbeef"));
        assert!(html.contains("sha256:aabb"));
    }

    #[test]
    fn it_never_reveals_the_contract_or_the_parties() {
        // The projection simply does not carry them; this test exists so
        // that adding a field to public_job() cannot quietly leak one.
        let html = job_page(&job());
        assert!(!html.contains("urn:gap:ctr"));
        assert!(!html.to_lowercase().contains("counterparty"));
        assert!(!html.contains("contract_id"));
    }

    #[test]
    fn a_failed_check_is_marked_as_failed() {
        let mut j = job();
        j["verdict"]["checks"][0]["passed"] = json!(false);
        j["verdict"]["ruling"] = json!("nonconforming");
        let html = job_page(&j);
        assert!(html.contains(r#"<span class="pill r">fail</span>"#));
        assert!(html.contains("nonconforming"));
    }

    #[test]
    fn escalated_jobs_say_so_prominently() {
        let mut j = job();
        j["verdict"]["escalation"] = json!("judge_disagreement");
        let html = job_page(&j);
        assert!(html.contains("Escalated to a human"));
        assert!(html.contains("judge_disagreement"));
    }

    #[test]
    fn a_job_that_was_never_verified_still_renders() {
        let j = json!({ "job_ref": "j1", "capability_id": "c", "outcome": "accepted" });
        let html = job_page(&j);
        assert!(html.contains("No verification was requested"));
        assert!(html.contains("Decided deterministically"));
        assert!(html.contains("j1"));
    }

    #[test]
    fn a_hostile_judge_reason_cannot_inject_markup() {
        // Judge output is model output: untrusted by definition.
        let mut j = job();
        j["verdict"]["opinions"][0]["reasons"] = json!(["<img src=x onerror=alert(1)>"]);
        let html = job_page(&j);
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x"));
    }
}
