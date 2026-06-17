use crate::traits::PreGuard;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};

pub struct ImmutableFieldGuard;

#[async_trait::async_trait]
impl PreGuard for ImmutableFieldGuard {
    fn name(&self) -> &str {
        "immutable_field"
    }

    async fn check(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        let args = &call.function.arguments;
        let target_url = state.target_url();
        let target_ip = state.target_ip();

        if call.function.name == "http_request" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(url) = parsed.get("url").and_then(|v| v.as_str()) {
                    if !url_matches_target(url, target_url, target_ip) {
                        return GuardVerdict::block(format!(
                            "URL {url} does not match target {target_url} / {target_ip}. Only attack the assigned target."
                        ));
                    }
                }
            }
        }

        if call.function.name == "execute_command" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(cmd) = parsed.get("command").and_then(|v| v.as_str()) {
                    if contains_off_target_ip(cmd, target_ip) {
                        return GuardVerdict::block(
                            "Command targets IP/host outside assigned target. Only attack the assigned target."
                                .to_string(),
                        );
                    }
                }
            }
        }

        GuardVerdict::allow()
    }
}

fn url_matches_target(url: &str, target_url: &str, target_ip: &str) -> bool {
    if url.starts_with(target_url) || url.starts_with(&target_url.replace("http://", "https://")) {
        return true;
    }
    if url.contains(target_ip) {
        return true;
    }
    if let Some(host) = extract_host(target_url) {
        if url.contains(&host) {
            return true;
        }
    }
    url.starts_with('/') || url.starts_with("./")
}

fn extract_host(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = without_scheme.split('/').next()?;
    Some(host_port.to_string())
}

fn contains_off_target_ip(cmd: &str, target_ip: &str) -> bool {
    let ip_re = regex::Regex::new(r"\b(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})\b").unwrap();
    for cap in ip_re.captures_iter(cmd) {
        let ip = &cap[1];
        if ip != target_ip && ip != "127.0.0.1" && ip != "0.0.0.0" {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://10.0.0.1:8080".into(),
            "10.0.0.1".into(),
            "ch-1".into(),
            "test".into(),
            vec![],
        )
    }

    #[tokio::test]
    async fn allows_target_url() {
        let guard = ImmutableFieldGuard;
        let state = make_state();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "http_request".into(),
                arguments: r#"{"url":"http://10.0.0.1:8080/login"}"#.into(),
            },
        };
        let v = guard.check(&call, &state).await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_off_target_url() {
        let guard = ImmutableFieldGuard;
        let state = make_state();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "http_request".into(),
                arguments: r#"{"url":"http://192.168.1.1/admin"}"#.into(),
            },
        };
        let v = guard.check(&call, &state).await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn allows_target_ip_in_command() {
        let guard = ImmutableFieldGuard;
        let state = make_state();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "execute_command".into(),
                arguments: r#"{"command":"nmap -sV 10.0.0.1"}"#.into(),
            },
        };
        let v = guard.check(&call, &state).await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_off_target_ip_in_command() {
        let guard = ImmutableFieldGuard;
        let state = make_state();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: "execute_command".into(),
                arguments: r#"{"command":"nmap -sV 192.168.1.100"}"#.into(),
            },
        };
        let v = guard.check(&call, &state).await;
        assert!(!v.allowed);
    }
}
