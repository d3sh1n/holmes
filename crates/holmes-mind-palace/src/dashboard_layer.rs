use crate::context_layer::{CompromisedHostSummary, ContextLayer, ReverseInsightSummary};
use holmes_core::event::{AccessLevel, ReverseInsightType};
use holmes_core::types::*;
use std::collections::HashMap;

pub struct DashboardLayer;

impl DashboardLayer {
    pub fn generate(context: &ContextLayer, mode: &SessionMode) -> DashboardSnapshot {
        let mut sections = HashMap::new();

        match mode {
            SessionMode::Pentest | SessionMode::SecurityResearch | SessionMode::Mixed => {
                if !context.attack_surface.hosts.is_empty()
                    || !context.attack_surface.services.is_empty()
                {
                    sections.insert(
                        "attack_surface".into(),
                        DashboardSection {
                            title: "攻击面".into(),
                            content_summary: format!(
                                "主机: {}, 服务: {}, 端点: {}, 凭据: {}",
                                context.attack_surface.hosts.len(),
                                context.attack_surface.services.len(),
                                context.attack_surface.endpoints.len(),
                                context.attack_surface.credentials_count,
                            ),
                            item_count: context.attack_surface.hosts.len()
                                + context.attack_surface.services.len(),
                        },
                    );
                }
                if let Some(ref topo) = context.network_topology {
                    sections.insert(
                        "network_topology".into(),
                        DashboardSection {
                            title: "内网拓扑".into(),
                            content_summary: format!(
                                "子网: {}, 主机: {}, 信任路径: {}",
                                topo.subnets.len(),
                                topo.hosts.len(),
                                topo.trust_paths.len()
                            ),
                            item_count: topo.hosts.len(),
                        },
                    );
                }
                if !context.compromised_hosts.is_empty() {
                    sections.insert(
                        "compromised".into(),
                        DashboardSection {
                            title: "已控主机".into(),
                            content_summary: context
                                .compromised_hosts
                                .iter()
                                .map(|h| format!("{} ({})", h.host, h.access_level_str()))
                                .collect::<Vec<_>>()
                                .join(", "),
                            item_count: context.compromised_hosts.len(),
                        },
                    );
                }
            }
            SessionMode::CodeAudit => {
                if !context.code_patterns.is_empty() {
                    sections.insert(
                        "code_patterns".into(),
                        DashboardSection {
                            title: "代码模式".into(),
                            content_summary: context
                                .code_patterns
                                .iter()
                                .map(|p| format!("{}:{} ({})", p.file, p.risk, p.pattern_type))
                                .collect::<Vec<_>>()
                                .join(", "),
                            item_count: context.code_patterns.len(),
                        },
                    );
                }
            }
            SessionMode::Reverse => {
                if !context.reverse_insights.is_empty() {
                    sections.insert(
                        "reverse_insights".into(),
                        DashboardSection {
                            title: "逆向发现".into(),
                            content_summary: context
                                .reverse_insights
                                .iter()
                                .map(|i| {
                                    format!(
                                        "{}: {} ({})",
                                        i.insight_type_str(),
                                        i.description,
                                        i.confidence
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("; "),
                            item_count: context.reverse_insights.len(),
                        },
                    );
                }
            }
        }

        if !context.vulnerabilities.is_empty() {
            sections.insert(
                "vulnerabilities".into(),
                DashboardSection {
                    title: "漏洞发现".into(),
                    content_summary: context
                        .vulnerabilities
                        .iter()
                        .map(|v| format!("{} ({})", v.title, v.severity_str()))
                        .collect::<Vec<_>>()
                        .join(", "),
                    item_count: context.vulnerabilities.len(),
                },
            );
        }
        if !context.credentials.is_empty() {
            sections.insert(
                "credentials".into(),
                DashboardSection {
                    title: "凭据".into(),
                    content_summary: format!("发现 {} 组凭据", context.credentials.len()),
                    item_count: context.credentials.len(),
                },
            );
        }
        if context.stack.depth() > 0 {
            sections.insert(
                "context".into(),
                DashboardSection {
                    title: "当前上下文".into(),
                    content_summary: context.stack.chain(),
                    item_count: context.stack.depth(),
                },
            );
        }

        DashboardSnapshot {
            sections,
            timestamp: chrono::Utc::now(),
        }
    }
}

impl CompromisedHostSummary {
    fn access_level_str(&self) -> &str {
        match self.access_level {
            AccessLevel::User => "User",
            AccessLevel::Root => "Root",
            AccessLevel::System => "SYSTEM",
            AccessLevel::DomainAdmin => "Domain Admin",
        }
    }
}

impl ReverseInsightSummary {
    fn insight_type_str(&self) -> &str {
        match self.insight_type {
            ReverseInsightType::FunctionIdentified => "函数识别",
            ReverseInsightType::ProtocolReverse => "协议逆向",
            ReverseInsightType::AlgorithmRecovery => "算法恢复",
            ReverseInsightType::ObfuscationBypass => "混淆绕过",
            ReverseInsightType::StringDecode => "字符串解码",
        }
    }
}

// VulnerabilitySummary::severity_str is defined in context_layer (pub(crate))
