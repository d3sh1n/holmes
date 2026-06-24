use crate::context_stack::ContextStack;
use holmes_core::event::Event;
use holmes_core::event::{
    AccessLevel, CredentialType, FindingStatus, HostInfo, ReverseInsightType, Severity,
};
use holmes_core::types::*;

#[derive(Debug, Clone)]
pub struct ContextLayer {
    pub stack: ContextStack,
    pub attack_surface: AttackSurfaceSummary,
    pub vulnerabilities: Vec<VulnerabilitySummary>,
    pub code_patterns: Vec<CodePatternSummary>,
    pub reverse_insights: Vec<ReverseInsightSummary>,
    pub credentials: Vec<CredentialSummary>,
    pub compromised_hosts: Vec<CompromisedHostSummary>,
    pub lateral_movements: Vec<LateralMovementSummary>,
    pub network_topology: Option<NetworkTopologySummary>,
    pub directive: Option<DirectiveSummary>,
    pub hypothesis: Option<HypothesisState>,
    pub reflections: Vec<ReflectionSummary>,
    pub current_phase: String,
    events_processed: usize,
}

#[derive(Debug, Clone)]
pub struct VulnerabilitySummary {
    pub title: String,
    pub cwe: Option<String>,
    pub cvss: Option<f64>,
    pub severity: Severity,
    pub location: String,
    pub status: FindingStatus,
}

#[derive(Debug, Clone)]
pub struct CodePatternSummary {
    pub pattern_type: String,
    pub file: String,
    pub line_range: Option<(u32, u32)>,
    pub risk: String,
}

#[derive(Debug, Clone)]
pub struct ReverseInsightSummary {
    pub insight_type: ReverseInsightType,
    pub description: String,
    pub confidence: String,
}

#[derive(Debug, Clone)]
pub struct CredentialSummary {
    pub username: String,
    pub credential_type: CredentialType,
    pub host: String,
    pub cracked: bool,
}

#[derive(Debug, Clone)]
pub struct CompromisedHostSummary {
    pub host: String,
    pub access_level: AccessLevel,
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct LateralMovementSummary {
    pub from: String,
    pub to: String,
    pub method: String,
}

#[derive(Debug, Clone)]
pub struct NetworkTopologySummary {
    pub subnets: Vec<String>,
    pub hosts: Vec<HostInfo>,
    pub trust_paths: Vec<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct DirectiveSummary {
    pub attack_type: Option<String>,
    pub objective: String,
    pub approach: String,
}

#[derive(Debug, Clone)]
pub struct HypothesisState {
    pub active: Option<String>,
    pub pending_count: usize,
    pub rejected: Vec<String>,
    pub confirmed: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReflectionSummary {
    pub diagnosis: String,
    pub failure_type: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl ContextLayer {
    pub fn new() -> Self {
        Self {
            stack: ContextStack::new(),
            attack_surface: AttackSurfaceSummary::default(),
            vulnerabilities: Vec::new(),
            code_patterns: Vec::new(),
            reverse_insights: Vec::new(),
            credentials: Vec::new(),
            compromised_hosts: Vec::new(),
            lateral_movements: Vec::new(),
            network_topology: None,
            directive: None,
            hypothesis: None,
            reflections: Vec::new(),
            current_phase: "initial".into(),
            events_processed: 0,
        }
    }

    pub fn ingest(&mut self, event: &Event) {
        self.events_processed += 1;
        match event {
            Event::AttackSurfaceUpdate {
                hosts,
                services,
                tech_stack,
                endpoints,
                credentials,
                ..
            } => {
                for h in hosts {
                    if !self.attack_surface.hosts.contains(h) {
                        self.attack_surface.hosts.push(h.clone());
                    }
                }
                for s in services {
                    let svc_str = format!("{}:{}/{}", s.host, s.port, s.service);
                    if !self.attack_surface.services.contains(&svc_str) {
                        self.attack_surface.services.push(svc_str);
                    }
                }
                for t in tech_stack {
                    if !self.attack_surface.tech_stack.contains(t) {
                        self.attack_surface.tech_stack.push(t.clone());
                    }
                }
                for e in endpoints {
                    if !self.attack_surface.endpoints.contains(e) {
                        self.attack_surface.endpoints.push(e.clone());
                    }
                }
                self.attack_surface.credentials_count += credentials.len();
            }
            Event::VulnerabilityFound {
                title,
                cwe,
                cvss,
                severity,
                location,
                status,
                ..
            } => {
                self.vulnerabilities.push(VulnerabilitySummary {
                    title: title.clone(),
                    cwe: cwe.clone(),
                    cvss: *cvss,
                    severity: severity.clone(),
                    location: location.clone(),
                    status: status.clone(),
                });
            }
            Event::CodePatternFound {
                pattern_type,
                file,
                line_range,
                risk_assessment,
                ..
            } => {
                self.code_patterns.push(CodePatternSummary {
                    pattern_type: pattern_type.clone(),
                    file: file.clone(),
                    line_range: *line_range,
                    risk: risk_assessment.clone(),
                });
            }
            Event::ReverseInsight {
                insight_type,
                description,
                confidence,
                ..
            } => {
                self.reverse_insights.push(ReverseInsightSummary {
                    insight_type: insight_type.clone(),
                    description: description.clone(),
                    confidence: confidence.clone(),
                });
            }
            Event::CredentialFound {
                username,
                credential_type,
                source_host,
                cracked,
                ..
            } => {
                self.credentials.push(CredentialSummary {
                    username: username.clone(),
                    credential_type: credential_type.clone(),
                    host: source_host.clone(),
                    cracked: cracked.unwrap_or(false),
                });
            }
            Event::HostCompromised {
                host,
                access_level,
                method,
                ..
            } => {
                self.compromised_hosts.push(CompromisedHostSummary {
                    host: host.clone(),
                    access_level: access_level.clone(),
                    method: method.clone(),
                });
            }
            Event::LateralMovement {
                from_host,
                to_host,
                method,
                ..
            } => {
                self.lateral_movements.push(LateralMovementSummary {
                    from: from_host.clone(),
                    to: to_host.clone(),
                    method: method.clone(),
                });
            }
            Event::NetworkTopologyUpdate {
                subnets,
                hosts,
                trust_paths,
                ..
            } => {
                self.network_topology = Some(NetworkTopologySummary {
                    subnets: subnets.clone(),
                    hosts: hosts.clone(),
                    trust_paths: trust_paths.clone(),
                });
            }
            Event::DirectiveSet {
                attack_type,
                objective,
                approach,
                ..
            } => {
                self.directive = Some(DirectiveSummary {
                    attack_type: attack_type.clone(),
                    objective: objective.clone(),
                    approach: approach.clone(),
                });
            }
            Event::HypothesisUpdate {
                active,
                pending_count,
                rejected,
                confirmed,
            } => {
                self.hypothesis = Some(HypothesisState {
                    active: active.clone(),
                    pending_count: *pending_count,
                    rejected: rejected.clone(),
                    confirmed: confirmed.clone(),
                });
            }
            Event::ReflectionRecorded {
                diagnosis,
                failure_type,
                ..
            } => {
                self.reflections.push(ReflectionSummary {
                    diagnosis: diagnosis.clone(),
                    failure_type: failure_type.clone(),
                    timestamp: chrono::Utc::now(),
                });
            }
            Event::ContextSwitched { to_context, .. } => {
                let _ = self.stack.push(to_context.clone(), "event driven");
            }
            _ => {}
        }
    }

    pub fn situation_summary(&self) -> String {
        let mut parts = Vec::new();
        if self.stack.depth() > 0 {
            parts.push(format!("[当前位置] {}", self.stack.chain()));
        }
        if !self.attack_surface.hosts.is_empty() {
            parts.push(format!(
                "[目标] 主机: {}, 服务: {}, 端点: {}",
                self.attack_surface.hosts.len(),
                self.attack_surface.services.len(),
                self.attack_surface.endpoints.len()
            ));
        }
        if !self.attack_surface.tech_stack.is_empty() {
            parts.push(format!(
                "[技术栈] {}",
                self.attack_surface.tech_stack.join(", ")
            ));
        }
        if !self.vulnerabilities.is_empty() {
            let confirmed: Vec<_> = self
                .vulnerabilities
                .iter()
                .filter(|v| v.status == FindingStatus::Confirmed)
                .collect();
            if !confirmed.is_empty() {
                parts.push(format!(
                    "[已确认漏洞] {}",
                    confirmed
                        .iter()
                        .map(|v| format!("{} ({})", v.title, v.severity_str()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        if !self.credentials.is_empty() {
            parts.push(format!("[凭据] 发现 {} 组", self.credentials.len()));
        }
        if let Some(ref d) = self.directive {
            parts.push(format!(
                "[当前策略] {} — {}",
                d.attack_type.as_deref().unwrap_or("unknown"),
                d.objective
            ));
        }
        if let Some(ref h) = self.hypothesis {
            if let Some(ref active) = h.active {
                parts.push(format!("[活跃假设] {}", active));
            }
            parts.push(format!(
                "[假设状态] 待验证: {}, 已否定: {}, 已确认: {}",
                h.pending_count,
                h.rejected.len(),
                h.confirmed.len()
            ));
        }
        parts.join("\n")
    }

    pub fn snapshot(&self) -> ContextSnapshot {
        ContextSnapshot {
            summary: self.situation_summary(),
            preserved_keys: vec![
                self.attack_surface.tech_stack.join(","),
                self.attack_surface.endpoints.join(","),
            ],
            active_contexts: self.stack.list().to_vec(),
            timestamp: chrono::Utc::now(),
        }
    }

    pub fn compress(&mut self) {
        if self.reflections.len() > 5 {
            self.reflections = self.reflections.split_off(self.reflections.len() - 5);
        }
        self.vulnerabilities
            .retain(|v| v.status != FindingStatus::FalsePositive);
    }
}

impl Default for ContextLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl VulnerabilitySummary {
    pub(crate) fn severity_str(&self) -> &str {
        match self.severity {
            Severity::Critical => "严重",
            Severity::High => "高危",
            Severity::Medium => "中危",
            Severity::Low => "低危",
            Severity::Info => "信息",
        }
    }
}
