use crate::traits::PostGuard;
use holmes_core::state::tool_truth::PortInfo;
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use regex::Regex;
use tracing::debug;

pub struct AttackSurfaceUpdater {
    port_re: Regex,
    tech_re: Regex,
    link_re: Regex,
}

impl AttackSurfaceUpdater {
    pub fn new() -> Self {
        Self {
            port_re: Regex::new(r"(\d+)/(tcp|udp)\s+open\s+(\S+)").unwrap(),
            tech_re: Regex::new(r"(?i)(apache|nginx|iis|tomcat|flask|django|express|php|wordpress|joomla|drupal|spring|node\.js|react|vue|angular)[\s/]*([\d.]+)?").unwrap(),
            link_re: Regex::new(r#"(?:href|action|src)=["']([^"']+)["']"#).unwrap(),
        }
    }
}

#[async_trait::async_trait]
impl PostGuard for AttackSurfaceUpdater {
    fn name(&self) -> &str {
        "attack_surface_updater"
    }

    async fn process(&mut self, _call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if result.is_error {
            return;
        }

        let content = &result.text_content();
        let surface = state.attack_surface_mut();

        for cap in self.port_re.captures_iter(content) {
            let port = cap[1].parse::<u16>().unwrap_or(0);
            let service = cap[3].to_string();
            if port > 0 {
                let info = PortInfo {
                    port,
                    service: service.clone(),
                    version: String::new(),
                };
                if !surface.ports.iter().any(|p| p.port == port) {
                    debug!(port, service = %info.service, "discovered port");
                    surface.ports.push(info);
                }
            }
        }

        for cap in self.tech_re.captures_iter(content) {
            let tech = cap[1].to_string();
            let version = cap
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            let entry = if version.is_empty() {
                tech.clone()
            } else {
                format!("{tech}/{version}")
            };
            if !surface.tech_stack.contains(&entry) {
                debug!(tech = %entry, "discovered technology");
                surface.tech_stack.push(entry);
            }
        }

        for cap in self.link_re.captures_iter(content) {
            let link = cap[1].to_string();
            if !surface.links.contains(&link) && surface.links.len() < 200 {
                surface.links.push(link);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    fn make_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "execute_command".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[tokio::test]
    async fn extracts_ports_from_nmap() {
        let mut guard = AttackSurfaceUpdater::new();
        let mut state = make_state();
        let result = ToolResult::success(
            "1",
            "cmd",
            "22/tcp open ssh\n80/tcp open http\n443/tcp open https",
        );
        guard.process(&make_call(), &result, &mut state).await;
        assert_eq!(state.attack_surface().ports.len(), 3);
        assert_eq!(state.attack_surface().ports[0].port, 22);
    }

    #[tokio::test]
    async fn extracts_technologies() {
        let mut guard = AttackSurfaceUpdater::new();
        let mut state = make_state();
        let result =
            ToolResult::success("1", "cmd", "Server: Apache/2.4.41\nX-Powered-By: PHP/7.4");
        guard.process(&make_call(), &result, &mut state).await;
        let techs = &state.attack_surface().tech_stack;
        assert!(techs.iter().any(|t| t.contains("Apache")));
        assert!(techs.iter().any(|t| t.contains("PHP")));
    }

    #[tokio::test]
    async fn deduplicates_ports() {
        let mut guard = AttackSurfaceUpdater::new();
        let mut state = make_state();
        let r1 = ToolResult::success("1", "cmd", "80/tcp open http");
        guard.process(&make_call(), &r1, &mut state).await;
        let r2 = ToolResult::success("2", "cmd", "80/tcp open http");
        guard.process(&make_call(), &r2, &mut state).await;
        assert_eq!(state.attack_surface().ports.len(), 1);
    }

    #[tokio::test]
    async fn skips_error_results() {
        let mut guard = AttackSurfaceUpdater::new();
        let mut state = make_state();
        let result = ToolResult::error("1", "cmd", "80/tcp open http");
        guard.process(&make_call(), &result, &mut state).await;
        assert_eq!(state.attack_surface().ports.len(), 0);
    }
}
