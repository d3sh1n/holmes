#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Severity;
    use crate::state::validated::Finding;

    fn example_program() -> ProgramScope {
        ProgramScope {
            name: "Example VDP".into(),
            in_scope: vec![
                ScopeEntry::new(ScopeAssetKind::DomainSuffix, "example.com"),
                ScopeEntry::new(ScopeAssetKind::UrlPrefix, "https://app.example.com/api"),
                ScopeEntry::new(ScopeAssetKind::Cidr, "203.0.113.0/24"),
            ],
            out_of_scope: vec![
                ScopeEntry::new(ScopeAssetKind::Host, "blog.example.com"),
                ScopeEntry::new(ScopeAssetKind::UrlPrefix, "https://app.example.com/api/admin"),
            ],
            policy_notes: "No testing of third-party services.".into(),
            allow_private: false,
        }
    }

    fn finding(
        id: &str,
        location: &str,
        confidence: FindingConfidence,
        resolution_ids: &[&str],
    ) -> Finding {
        Finding {
            id: id.into(),
            finding_type: "idor".into(),
            confidence,
            evidence: "Observed object identifier returned another user's record.".into(),
            details: "Authorization check missing on the object endpoint.".into(),
            attack_type: "idor".into(),
            severity: Severity::High,
            location: location.into(),
            evidence_source: Some("call-1".into()),
            resolution_ids: resolution_ids.iter().map(|s| (*s).to_string()).collect(),
            affected_asset: Some(location.into()),
            evidence_artifacts: EvidenceArtifacts {
                ledger_evidence_ids: vec!["ev-1".into()],
                request_response_hashes: vec!["abc123".into()],
                ..EvidenceArtifacts::default()
            },
        }
    }

    #[test]
    fn out_of_scope_host_is_rejected() {
        let program = example_program();
        assert!(identifier_in_scope(&program, "api.example.com"));
        assert!(identifier_in_scope(
            &program,
            "https://app.example.com/api/users"
        ));
        assert!(!identifier_in_scope(&program, "evil.com"));
        assert!(!identifier_in_scope(&program, "blog.example.com"));
        assert!(!identifier_in_scope(
            &program,
            "https://app.example.com/api/admin/users"
        ));
        assert!(record_asset_or_reject(Some(&program), "evil.com").is_err());
        assert!(record_asset_or_reject(Some(&program), "api.example.com").is_ok());
    }

    #[test]
    fn finding_without_resolution_id_is_rejected() {
        let program = example_program();
        let err = gate_finding(
            Some(&program),
            &[],
            |_| Some("confirmed".into()),
            Some("https://app.example.com/api/users"),
        )
        .unwrap_err();
        assert_eq!(err, FindingGateError::NoVerifiedResolution);
    }

    #[test]
    fn unverified_resolution_is_rejected() {
        let program = example_program();
        let err = gate_finding(
            Some(&program),
            &["res-open".into()],
            |_| Some("inconclusive".into()),
            Some("api.example.com"),
        )
        .unwrap_err();
        assert!(matches!(err, FindingGateError::UnverifiedResolution { .. }));
    }

    #[test]
    fn out_of_scope_finding_asset_is_rejected() {
        let program = example_program();
        let err = gate_finding(
            Some(&program),
            &["res-1".into()],
            |_| Some("confirmed".into()),
            Some("https://evil.com/login"),
        )
        .unwrap_err();
        assert!(matches!(err, FindingGateError::OutOfScopeAsset(_)));
    }

    #[test]
    fn report_includes_only_in_scope_verified_findings() {
        let program = example_program();
        let in_scope = finding(
            "IDOR on /api/users",
            "https://app.example.com/api/users",
            FindingConfidence::Confirmed,
            &["res-1"],
        );
        let oos = finding(
            "Issue on evil.com",
            "https://evil.com/x",
            FindingConfidence::Confirmed,
            &["res-2"],
        );
        let unverified = finding(
            "Candidate",
            "https://app.example.com/api/users",
            FindingConfidence::Candidate,
            &["res-3"],
        );
        let no_res = finding(
            "No resolution",
            "https://app.example.com/api/users",
            FindingConfidence::Confirmed,
            &[],
        );
        let selected = reportable_findings(
            Some(&program),
            [&in_scope, &oos, &unverified, &no_res],
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, "IDOR on /api/users");

        let md = render_bounty_report(BountyReportInput {
            program: Some(&program),
            assets: &[],
            findings: selected,
            allow_no_findings: false,
            generated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        })
        .unwrap();
        assert!(md.contains("IDOR on /api/users"));
        assert!(md.contains("res-1"));
        assert!(md.contains("Example VDP"));
        assert!(!md.contains("evil.com"));
        assert!(!md.contains("Candidate"));
        assert!(md.contains("not an exploit writeup") || md.contains("research workflow"));
        assert!(md.contains("no exploit recipes, payloads") || md.contains("No payload or exploit procedure"));
        assert!(md.contains("High-level reproduction"));
        assert!(!md.to_lowercase().contains("step-by-step attack instructions") || md.contains("Do not treat this section as step-by-step"));
    }

    #[test]
    fn asset_inventory_records_provenance() {
        let mut bounty = BountyCase::default();
        let asset = DiscoveredAsset {
            id: "asset-1".into(),
            identifier: "api.example.com".into(),
            kind: ScopeAssetKind::Host,
            how_found: AssetProvenance::ReportRecon,
            notes: "seen in recon endpoints".into(),
            recorded_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        };
        bounty.upsert_asset(asset.clone());
        bounty.upsert_asset(asset);
        assert_eq!(bounty.assets.len(), 1);
        assert_eq!(bounty.assets[0].how_found, AssetProvenance::ReportRecon);
        assert_eq!(bounty.assets[0].notes, "seen in recon endpoints");
    }

    #[test]
    fn empty_report_requires_explicit_flag() {
        let program = example_program();
        let err = render_bounty_report(BountyReportInput {
            program: Some(&program),
            assets: &[],
            findings: vec![],
            allow_no_findings: false,
            generated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        })
        .unwrap_err();
        assert!(err.contains("allow_no_findings"));
        let md = render_bounty_report(BountyReportInput {
            program: Some(&program),
            assets: &[],
            findings: vec![],
            allow_no_findings: true,
            generated_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        })
        .unwrap();
        assert!(md.contains("No in-scope, ledger-verified findings"));
    }

    #[test]
    fn cidr_and_private_heuristics() {
        let mut program = example_program();
        assert!(identifier_in_scope(&program, "203.0.113.10"));
        assert!(!identifier_in_scope(&program, "10.1.2.3"));
        program.allow_private = true;
        program
            .in_scope
            .push(ScopeEntry::new(ScopeAssetKind::Cidr, "10.0.0.0/8"));
        assert!(identifier_in_scope(&program, "10.1.2.3"));
    }

    #[test]
    fn gate_is_noop_without_program() {
        assert!(gate_finding(None, &[], |_| None, None).is_ok());
    }
}
