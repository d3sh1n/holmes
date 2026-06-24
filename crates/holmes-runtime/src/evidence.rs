use crate::context::RuntimeContext;
use crate::dialogue::DialogueEngine;
use crate::yield_stream::RuntimeYield;

#[derive(Debug, Clone, Default)]
pub struct EvidenceEngine;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EvidenceProjection {
    pub updates: Vec<String>,
    pub events: Vec<RuntimeYield>,
}

impl EvidenceEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn project(&self, context: &mut RuntimeContext) -> EvidenceProjection {
        let mut updates = Vec::new();

        project_attack_surface(context, &mut updates);
        project_evidence_bundle(context, &mut updates);
        project_findings(context, &mut updates);

        context.state.observations.extend(updates.iter().cloned());
        let events = updates
            .iter()
            .map(DialogueEngine::evidence_update)
            .collect::<Vec<_>>();

        EvidenceProjection { updates, events }
    }
}

fn project_attack_surface(context: &mut RuntimeContext, updates: &mut Vec<String>) {
    let surface = context.state.compatibility_state.attack_surface().clone();

    for port in surface.ports {
        let service = port.service.trim();
        let version = port.version.trim();
        let key = format!("{}:{}:{}", port.port, service, version);
        let version = if version.is_empty() {
            String::new()
        } else {
            format!(" {version}")
        };
        record_once(
            &mut context.state.evidence_projection.seen_ports,
            key,
            format!(
                "Discovered service on port {}: {}{}.",
                port.port, service, version
            ),
            updates,
        );
    }

    for tech in surface.tech_stack {
        let normalized = tech.trim();
        if normalized.is_empty() {
            continue;
        }
        record_once(
            &mut context.state.evidence_projection.seen_tech,
            normalized.to_ascii_lowercase(),
            format!("Identified technology: {normalized}."),
            updates,
        );
    }

    for link in surface.links {
        let normalized = link.trim();
        if normalized.is_empty() {
            continue;
        }
        record_once(
            &mut context.state.evidence_projection.seen_endpoints,
            format!("link:{normalized}"),
            format!("Observed endpoint/link: {normalized}."),
            updates,
        );
    }

    for form in surface.forms {
        let action = form.action.trim();
        if action.is_empty() {
            continue;
        }
        let method = form.method.trim();
        let method_label = if method.is_empty() { "form" } else { method };
        record_once(
            &mut context.state.evidence_projection.seen_endpoints,
            format!("form:{}:{}", method_label.to_ascii_uppercase(), action),
            format!("Observed {method_label} form at {action}."),
            updates,
        );
    }
}

fn project_evidence_bundle(context: &mut RuntimeContext, updates: &mut Vec<String>) {
    let bundle = context.state.compatibility_state.evidence_bundle().clone();

    for credential in bundle.credentials {
        let username = credential.username.trim();
        let source = credential.source.trim();
        if username.is_empty() {
            continue;
        }
        record_once(
            &mut context.state.evidence_projection.seen_credentials,
            format!("{username}:{source}"),
            if source.is_empty() {
                format!("Captured credential material for user {username}.")
            } else {
                format!("Captured credential material for user {username} from {source}.")
            },
            updates,
        );
    }

    for object_ref in bundle.object_refs {
        let path = object_ref.path.trim();
        let id_value = object_ref.id_value.trim();
        if path.is_empty() && id_value.is_empty() {
            continue;
        }
        let label = if id_value.is_empty() {
            path.to_string()
        } else if path.is_empty() {
            id_value.to_string()
        } else {
            format!("{path} -> {id_value}")
        };
        record_once(
            &mut context.state.evidence_projection.seen_endpoints,
            format!("object_ref:{path}:{id_value}"),
            format!("Observed object reference: {label}."),
            updates,
        );
    }

    for vuln in bundle.vulns {
        let vuln_type = vuln.vuln_type.trim();
        let endpoint = vuln.endpoint.trim();
        if vuln_type.is_empty() && endpoint.is_empty() {
            continue;
        }
        let label = match (vuln_type.is_empty(), endpoint.is_empty()) {
            (false, false) => format!("{vuln_type} at {endpoint}"),
            (false, true) => vuln_type.to_string(),
            (true, false) => endpoint.to_string(),
            (true, true) => String::new(),
        };
        record_once(
            &mut context.state.evidence_projection.seen_findings,
            format!("vuln:{vuln_type}:{endpoint}:{}", vuln.evidence.trim()),
            format!("Observed vulnerability evidence: {label}."),
            updates,
        );
    }
}

fn project_findings(context: &mut RuntimeContext, updates: &mut Vec<String>) {
    let mut findings = context
        .state
        .compatibility_state
        .findings()
        .iter()
        .map(|(id, finding)| (id.clone(), finding.clone()))
        .collect::<Vec<_>>();
    findings.sort_by(|(left, _), (right, _)| left.cmp(right));

    for (id, finding) in findings {
        let finding_type = finding.finding_type.trim();
        let label = if finding_type.is_empty() {
            id.as_str()
        } else {
            finding_type
        };
        let confidence = format!("{:?}", finding.confidence).to_ascii_lowercase();
        record_once(
            &mut context.state.evidence_projection.seen_findings,
            format!("finding:{id}"),
            format!("Observed {confidence} finding: {label}."),
            updates,
        );
    }
}

fn record_once(
    seen: &mut std::collections::HashSet<String>,
    key: String,
    update: String,
    updates: &mut Vec<String>,
) {
    if seen.insert(key) {
        updates.push(update);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::state::{
        Credential, Finding, FindingConfidence, FormInfo, ObjectRef, PortInfo, VulnEvidence,
    };
    use holmes_core::{LlmResponse, SessionMode};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, SessionDB};
    use holmes_tools::ToolRegistry;

    use crate::context::{RuntimeContext, RuntimeState};
    use crate::deliberation::StaticLlmBackend;

    use super::*;

    #[tokio::test]
    async fn projects_new_evidence_into_updates_events_and_observations() {
        let mut context = make_context().await;
        let state = &mut context.state.compatibility_state;
        state.attack_surface_mut().ports.push(PortInfo {
            port: 443,
            service: "https".into(),
            version: "nginx".into(),
        });
        state.attack_surface_mut().tech_stack.push("Django".into());
        state.attack_surface_mut().links.push("/admin".into());
        state.attack_surface_mut().forms.push(FormInfo {
            action: "/login".into(),
            method: "POST".into(),
            inputs: vec!["username".into(), "password".into()],
        });
        state.evidence_bundle_mut().add_credential(Credential {
            username: "admin".into(),
            password: "secret".into(),
            source: "html".into(),
        });
        state.evidence_bundle_mut().object_refs.push(ObjectRef {
            path: "/users".into(),
            id_value: "42".into(),
        });
        state.evidence_bundle_mut().vulns.push(VulnEvidence {
            vuln_type: "idor".into(),
            endpoint: "/users/42".into(),
            evidence: "object id accepted".into(),
        });
        state.findings_mut().insert(
            "finding-1".into(),
            Finding {
                id: "finding-1".into(),
                finding_type: "idor".into(),
                confidence: FindingConfidence::Candidate,
                evidence: "object id accepted".into(),
                details: "user object reference".into(),
                attack_type: "authorization".into(),
            },
        );

        let projection = EvidenceEngine::new().project(&mut context);

        assert_eq!(
            projection.updates,
            vec![
                "Discovered service on port 443: https nginx.",
                "Identified technology: Django.",
                "Observed endpoint/link: /admin.",
                "Observed POST form at /login.",
                "Captured credential material for user admin from html.",
                "Observed object reference: /users -> 42.",
                "Observed vulnerability evidence: idor at /users/42.",
                "Observed candidate finding: idor.",
            ]
        );
        assert_eq!(projection.events.len(), projection.updates.len());
        assert!(matches!(
            projection.events[0],
            RuntimeYield::EvidenceUpdate { .. }
        ));
        assert_eq!(context.state.observations, projection.updates);
    }

    #[tokio::test]
    async fn repeated_projection_does_not_emit_duplicates() {
        let mut context = make_context().await;
        context
            .state
            .compatibility_state
            .attack_surface_mut()
            .tech_stack
            .push("Django".into());

        let first = EvidenceEngine::new().project(&mut context);
        let second = EvidenceEngine::new().project(&mut context);

        assert_eq!(first.updates, vec!["Identified technology: Django."]);
        assert!(second.updates.is_empty());
        assert!(second.events.is_empty());
        assert_eq!(
            context.state.observations,
            vec!["Identified technology: Django."]
        );
    }

    #[tokio::test]
    async fn findings_are_projected_in_stable_order() {
        let mut context = make_context().await;
        context.state.compatibility_state.findings_mut().insert(
            "b".into(),
            Finding {
                id: "b".into(),
                finding_type: "xss".into(),
                confidence: FindingConfidence::Candidate,
                evidence: "script reflected".into(),
                details: String::new(),
                attack_type: "client-side".into(),
            },
        );
        context.state.compatibility_state.findings_mut().insert(
            "a".into(),
            Finding {
                id: "a".into(),
                finding_type: "sqli".into(),
                confidence: FindingConfidence::Confirmed,
                evidence: "sql error".into(),
                details: String::new(),
                attack_type: "injection".into(),
            },
        );

        let projection = EvidenceEngine::new().project(&mut context);

        assert_eq!(
            projection.updates,
            vec![
                "Observed confirmed finding: sqli.",
                "Observed candidate finding: xss."
            ]
        );
    }

    #[tokio::test]
    async fn dedupe_keys_do_not_depend_on_secret_or_untrimmed_port_fields() {
        let mut context = make_context().await;
        let state = &mut context.state.compatibility_state;
        state.attack_surface_mut().ports.push(PortInfo {
            port: 80,
            service: " http ".into(),
            version: " nginx ".into(),
        });
        state.attack_surface_mut().ports.push(PortInfo {
            port: 80,
            service: "http".into(),
            version: "nginx".into(),
        });
        state.evidence_bundle_mut().add_credential(Credential {
            username: "admin".into(),
            password: "short".into(),
            source: "html".into(),
        });
        state.evidence_bundle_mut().credentials.push(Credential {
            username: "admin".into(),
            password: "much-longer-secret".into(),
            source: "html".into(),
        });

        let projection = EvidenceEngine::new().project(&mut context);

        assert_eq!(
            projection.updates,
            vec![
                "Discovered service on port 80: http nginx.",
                "Captured credential material for user admin from html.",
            ]
        );
        assert_eq!(
            context.state.evidence_projection.seen_credentials,
            ["admin:html".to_string()].into_iter().collect()
        );
    }

    async fn make_context() -> RuntimeContext {
        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let mind_palace = MindPalace::new(session_db.clone(), memory_store.clone());
        let llm = Arc::new(StaticLlmBackend::new(LlmResponse {
            content: Some("ok".into()),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        }));

        RuntimeContext::new(
            RuntimeSession::new(session_id, SessionMode::Pentest),
            session_db,
            memory_store,
            mind_palace,
            llm,
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            HolmesConfig::default(),
        )
    }
}
