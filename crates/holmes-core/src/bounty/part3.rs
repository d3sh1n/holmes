pub fn is_private_or_metadata(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.octets() == [169, 254, 169, 254]
            || ip.is_unspecified();
    }
    false
}

pub fn url_matches_any_prefix(url: &str, prefixes: &[String]) -> bool {
    let url = normalize_url_prefix(url);
    prefixes.iter().any(|prefix| {
        let p = prefix.trim_end_matches('/');
        url == *prefix || url == p || url.starts_with(&format!("{p}/")) || url.starts_with(&format!("{p}?"))
    })
}

fn normalize_entry(value: &str) -> String {
    value.trim().trim_end_matches('.').to_lowercase()
}

fn normalize_url_prefix(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Findings eligible for a bounty report: in-scope, and backed by a verified
/// (Confirmed) Ledger resolution. Rejected/candidate findings are excluded.
pub fn reportable_findings<'a>(
    program: Option<&ProgramScope>,
    findings: impl IntoIterator<Item = &'a Finding>,
) -> Vec<&'a Finding> {
    findings
        .into_iter()
        .filter(|f| f.confidence == FindingConfidence::Confirmed)
        .filter(|f| !f.resolution_ids.is_empty())
        .filter(|f| match program {
            None => true,
            Some(program) => {
                let asset = f
                    .affected_asset
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(f.location.as_str());
                !asset.is_empty() && identifier_in_scope(program, asset)
            }
        })
        .collect()
}

pub struct BountyReportInput<'a> {
    pub program: Option<&'a ProgramScope>,
    pub assets: &'a [DiscoveredAsset],
    pub findings: Vec<&'a Finding>,
    pub allow_no_findings: bool,
    pub generated_at: DateTime<Utc>,
}

/// Render a bounty-ready markdown report. Reproduction is high-level (what was
/// observed + evidence IDs). Never includes payloads or attack steps.
pub fn render_bounty_report(input: BountyReportInput<'_>) -> Result<String, String> {
    let findings = input.findings;
    if findings.is_empty() && !input.allow_no_findings {
        return Err(
            "no in-scope ledger-verified findings; pass allow_no_findings=true for an explicit empty report"
                .into(),
        );
    }

    let mut out = String::new();
    out.push_str("# Bug bounty research report\n\n");
    out.push_str(&format!(
        "_Generated at {}. This is a research workflow report, not an exploit writeup._\n\n",
        input.generated_at.to_rfc3339()
    ));

    out.push_str("## Summary\n\n");
    if findings.is_empty() {
        out.push_str("No in-scope, ledger-verified findings were recorded for this engagement.\n\n");
    } else {
        out.push_str(&format!(
            "{} in-scope finding(s) backed by verified Hypothesis Ledger Resolution IDs.\n\n",
            findings.len()
        ));
    }

    out.push_str("## Program scope\n\n");
    match input.program {
        Some(program) => {
            out.push_str(&format!("- **Program:** {}\n", program.name));
            if !program.policy_notes.trim().is_empty() {
                out.push_str(&format!("- **Policy notes:** {}\n", program.policy_notes.trim()));
            }
            out.push_str("- **In scope:**\n");
            for entry in &program.in_scope {
                out.push_str(&format!("  - {:?} `{}`\n", entry.kind, entry.value));
            }
            if program.out_of_scope.is_empty() {
                out.push_str("- **Out of scope:** (none declared)\n");
            } else {
                out.push_str("- **Out of scope:**\n");
                for entry in &program.out_of_scope {
                    out.push_str(&format!("  - {:?} `{}`\n", entry.kind, entry.value));
                }
            }
            out.push_str(
                "\nScope enforcement for `http_request` / `web_fetch` / `browser` navigate and \
                 hostnames extracted from `execute_command` / `execute_python` is the same \
                 **heuristic** as `guards.scope`: exact host, domain suffix, IPv4 CIDR, plus \
                 URL prefixes when declared. Redirects, DNS changes, unknown/MCP tools, and \
                 dynamically constructed commands can bypass it. The operator remains \
                 responsible for staying inside the authorized program.\n",
            );
        }
        None => out.push_str("No case-level program was attached.\n"),
    }
    out.push('\n');

    out.push_str("## Affected assets\n\n");
    let mut listed = Vec::new();
    for f in &findings {
        let asset = f
            .affected_asset
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(f.location.as_str());
        if !asset.is_empty() && !listed.iter().any(|s: &String| s == asset) {
            listed.push(asset.to_string());
        }
    }
    if listed.is_empty() && input.assets.is_empty() {
        out.push_str("(none recorded)\n\n");
    } else {
        for asset in &listed {
            out.push_str(&format!("- `{asset}` (cited by a verified finding)\n"));
        }
        for asset in input.assets {
            if listed.iter().any(|s| s == &asset.identifier) {
                continue;
            }
            out.push_str(&format!(
                "- `{}` ({:?}, provenance: {})\n",
                asset.identifier,
                asset.kind,
                asset.how_found.label()
            ));
        }
        out.push('\n');
    }

    out.push_str("## Findings\n\n");
    if findings.is_empty() {
        out.push_str("No findings.\n\n");
    } else {
        for (i, f) in findings.iter().enumerate() {
            out.push_str(&format!("### {}. {}\n\n", i + 1, f.id));
            out.push_str(&format!("- **Type:** {}\n", f.finding_type));
            out.push_str(&format!("- **Severity:** {:?}\n", f.severity));
            out.push_str(&format!("- **Confidence:** {:?}\n", f.confidence));
            let loc = f
                .affected_asset
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or(f.location.as_str());
            if !loc.is_empty() {
                out.push_str(&format!("- **Affected asset:** `{loc}`\n"));
            }
            out.push_str(&format!(
                "- **Ledger Resolution IDs:** {}\n",
                f.resolution_ids.join(", ")
            ));
            out.push_str("\n#### Impact\n\n");
            let impact = if f.details.trim().is_empty() {
                "Business/security impact as recorded in the validated finding (not an exploit writeup)."
            } else {
                f.details.trim()
            };
            out.push_str(impact);
            out.push_str("\n\n#### What was observed\n\n");
            out.push_str(f.evidence.trim());
            out.push_str("\n\nHigh-level reproduction: the observation above is bound to the cited Resolution IDs and evidence artifacts. Do not treat this section as step-by-step attack instructions.\n\n");
            out.push_str("#### High-level remediation\n\n");
            out.push_str(
                "Review the affected asset, confirm the observation against the cited evidence, \
                 and apply the control appropriate to the finding type (authorization, input \
                 handling, exposure reduction). No payload or exploit procedure is provided.\n\n",
            );
        }
    }

    out.push_str("## Evidence appendix\n\n");
    if findings.is_empty() {
        out.push_str("No evidence artifacts (no findings).\n");
    } else {
        for f in &findings {
            out.push_str(&format!("### {}\n\n", f.id));
            out.push_str(&format!(
                "- Resolution IDs: {}\n",
                f.resolution_ids.join(", ")
            ));
            if let Some(src) = &f.evidence_source {
                out.push_str(&format!("- Evidence source (tool call): `{src}`\n"));
            }
            if !f.evidence_artifacts.ledger_evidence_ids.is_empty() {
                out.push_str(&format!(
                    "- Ledger evidence IDs: {}\n",
                    f.evidence_artifacts.ledger_evidence_ids.join(", ")
                ));
            }
            if !f.evidence_artifacts.screenshot_paths.is_empty() {
                out.push_str(&format!(
                    "- Screenshots: {}\n",
                    f.evidence_artifacts.screenshot_paths.join(", ")
                ));
            }
            if !f.evidence_artifacts.request_response_hashes.is_empty() {
                out.push_str(&format!(
                    "- Request/response hashes: {}\n",
                    f.evidence_artifacts.request_response_hashes.join(", ")
                ));
            }
            if !f.evidence_artifacts.log_excerpts.is_empty() {
                out.push_str("- Log excerpts:\n");
                for excerpt in &f.evidence_artifacts.log_excerpts {
                    out.push_str(&format!("  - {}\n", excerpt.trim()));
                }
            }
            out.push('\n');
        }
    }
    out.push_str(
        "\n---\nAuthorized research only. This report contains no exploit recipes, payloads, \
         or weaponized reproduction.\n",
    );
    Ok(out)
}
