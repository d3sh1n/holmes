use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Attack surface — populated only from tool output via PostGuards.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AttackSurface {
    pub ports: Vec<PortInfo>,
    pub forms: Vec<FormInfo>,
    pub links: Vec<String>,
    pub tech_stack: Vec<String>,
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortInfo {
    pub port: u16,
    pub service: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormInfo {
    pub action: String,
    pub method: String,
    pub inputs: Vec<String>,
}

impl AttackSurface {
    pub fn tech_summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.ports.is_empty() {
            let port_str: Vec<String> = self
                .ports
                .iter()
                .map(|p| format!("{}:{}", p.port, p.service))
                .collect();
            parts.push(format!("ports=[{}]", port_str.join(",")));
        }
        if !self.tech_stack.is_empty() {
            parts.push(format!("tech=[{}]", self.tech_stack.join(",")));
        }
        if !self.forms.is_empty() {
            parts.push(format!("forms={}", self.forms.len()));
        }
        parts.join(" ")
    }

    pub fn merge_recon(&mut self, other: AttackSurface) {
        for port in other.ports {
            if !self.ports.iter().any(|p| p.port == port.port) {
                self.ports.push(port);
            }
        }
        for form in other.forms {
            self.forms.push(form);
        }
        for link in other.links {
            if !self.links.contains(&link) {
                self.links.push(link);
            }
        }
        for tech in other.tech_stack {
            if !self.tech_stack.contains(&tech) {
                self.tech_stack.push(tech);
            }
        }
        for (k, v) in other.headers {
            self.headers.entry(k).or_insert(v);
        }
    }
}

/// Evidence bundle — structured evidence extracted from tool output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub credentials: Vec<Credential>,
    pub object_refs: Vec<ObjectRef>,
    pub vulns: Vec<VulnEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Credential {
    pub username: String,
    pub password: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRef {
    pub path: String,
    pub id_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulnEvidence {
    pub vuln_type: String,
    pub endpoint: String,
    pub evidence: String,
}

impl EvidenceBundle {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.credentials.is_empty() {
            parts.push(format!("creds={}", self.credentials.len()));
        }
        if !self.object_refs.is_empty() {
            parts.push(format!("obj_refs={}", self.object_refs.len()));
        }
        if !self.vulns.is_empty() {
            parts.push(format!("vulns={}", self.vulns.len()));
        }
        parts.join(" ")
    }

    pub fn add_credential(&mut self, cred: Credential) {
        if !self.credentials.contains(&cred) {
            self.credentials.push(cred);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attack_surface_merge_deduplicates_ports() {
        let mut surface = AttackSurface::default();
        surface.ports.push(PortInfo {
            port: 80,
            service: "http".into(),
            version: "".into(),
        });

        let other = AttackSurface {
            ports: vec![
                PortInfo {
                    port: 80,
                    service: "http".into(),
                    version: "".into(),
                },
                PortInfo {
                    port: 3306,
                    service: "mysql".into(),
                    version: "5.7".into(),
                },
            ],
            ..Default::default()
        };
        surface.merge_recon(other);
        assert_eq!(surface.ports.len(), 2);
    }

    #[test]
    fn evidence_bundle_deduplicates_credentials() {
        let mut bundle = EvidenceBundle::default();
        let cred = Credential {
            username: "admin".into(),
            password: "pass".into(),
            source: "html".into(),
        };
        bundle.add_credential(cred.clone());
        bundle.add_credential(cred);
        assert_eq!(bundle.credentials.len(), 1);
    }

    #[test]
    fn attack_surface_tech_summary_formats() {
        let surface = AttackSurface {
            ports: vec![PortInfo {
                port: 80,
                service: "http".into(),
                version: "".into(),
            }],
            tech_stack: vec!["Django".into()],
            forms: vec![FormInfo {
                action: "/login".into(),
                method: "POST".into(),
                inputs: vec![],
            }],
            ..Default::default()
        };
        let summary = surface.tech_summary();
        assert!(summary.contains("80:http"));
        assert!(summary.contains("Django"));
        assert!(summary.contains("forms=1"));
    }
}
