use crate::traits::PreGuard;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};

const BLOCKED_PATTERNS: &[&str] = &[
    "rm -rf /",
    "rm -rf /*",
    "mkfs",
    "dd if=",
    "> /dev/sd",
    "chmod 777 /",
    "export PATH=",
    "unset PATH",
    ":(){ :|:& };:",
];

const BLOCKED_PREFIXES: &[&str] = &["shutdown", "reboot", "halt", "init 0", "init 6"];

const CHEAT_PREFIXES: &[&str] = &[
    "docker ",
    "docker-compose ",
    "podman ",
    "kubectl ",
    "crictl ",
    "nerdctl ",
    "nsenter ",
    "ctr ",
];

const PRIVATE_PREFIXES: &[&str] = &[
    "http://127.",
    "https://127.",
    "http://localhost",
    "https://localhost",
    "http://10.",
    "https://10.",
    "http://192.168.",
    "https://192.168.",
    "http://172.16.",
    "https://172.16.",
    "http://172.17.",
    "https://172.17.",
    "http://172.18.",
    "https://172.18.",
    "http://172.19.",
    "https://172.19.",
    "http://172.2",
    "https://172.2",
    "http://172.3",
    "https://172.3",
];

const BLOCKED_JS: &[&str] = &["window.close", "self.close"];

pub struct DangerousCommandGuard;

#[async_trait::async_trait]
impl PreGuard for DangerousCommandGuard {
    fn name(&self) -> &str {
        "dangerous_command"
    }

    async fn check(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        if call.function.name == "browser" {
            return self.check_browser(call, state);
        }

        if call.function.name != "execute_command" {
            return GuardVerdict::allow();
        }

        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(_) => return GuardVerdict::allow(),
        };

        let cmd = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return GuardVerdict::allow(),
        };

        let lower = cmd.to_lowercase();

        for pattern in BLOCKED_PATTERNS {
            if lower.contains(pattern) {
                return GuardVerdict::block(format!(
                    "Blocked destructive command containing '{pattern}'. Use a safer alternative."
                ));
            }
        }

        for prefix in BLOCKED_PREFIXES {
            if lower.trim().starts_with(prefix) {
                return GuardVerdict::block(format!(
                    "Blocked system command '{prefix}'. This could disrupt the environment."
                ));
            }
        }

        for cheat in CHEAT_PREFIXES {
            if lower.trim().starts_with(cheat)
                || lower.contains(&format!("| {cheat}"))
                || lower.contains(&format!("; {cheat}"))
                || lower.contains(&format!("$({cheat}"))
            {
                return GuardVerdict::block(format!(
                    "Blocked container command '{cheat}'. \
                     You must exploit the target through its exposed services (HTTP, etc.), \
                     not by accessing the container infrastructure directly."
                ));
            }
        }

        GuardVerdict::allow()
    }
}

impl DangerousCommandGuard {
    fn check_browser(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(v) => v,
            Err(_) => return GuardVerdict::allow(),
        };

        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => return GuardVerdict::allow(),
        };

        if action == "navigate" {
            if let Some(url) = args.get("url").and_then(|v| v.as_str()) {
                let lower = url.to_lowercase();
                let target_ip = state.target_ip();
                for prefix in PRIVATE_PREFIXES {
                    if lower.starts_with(prefix) {
                        if lower.contains(target_ip) {
                            return GuardVerdict::allow();
                        }
                        return GuardVerdict::block(format!(
                            "Blocked browser navigation to private address '{}'. \
                             Only navigate to the target ({}) or public addresses.",
                            url, target_ip
                        ));
                    }
                }
            }
        }

        if action == "execute_js" {
            if let Some(code) = args.get("code").and_then(|v| v.as_str()) {
                let lower = code.to_lowercase();
                for pattern in BLOCKED_JS {
                    if lower.contains(pattern) {
                        return GuardVerdict::block(format!(
                            "Blocked dangerous JavaScript '{}'. This could disrupt the browser session.",
                            pattern
                        ));
                    }
                }
            }
        }

        GuardVerdict::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use holmes_core::FunctionCall;

    fn make_state() -> AttackState {
        AttackState::new(
            "http://t:80".into(),
            "10.0.0.1".into(),
            "c".into(),
            "t".into(),
            vec![],
        )
    }

    fn cmd_call(cmd: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "execute_command".into(),
                arguments: format!(r#"{{"command":"{}"}}"#, cmd),
            },
        }
    }

    #[tokio::test]
    async fn allows_normal_command() {
        let v = DangerousCommandGuard
            .check(&cmd_call("nmap -sV 10.0.0.1"), &make_state())
            .await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_rm_rf() {
        let v = DangerousCommandGuard
            .check(&cmd_call("rm -rf /"), &make_state())
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_fork_bomb() {
        let v = DangerousCommandGuard
            .check(&cmd_call(":(){ :|:& };:"), &make_state())
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_shutdown() {
        let v = DangerousCommandGuard
            .check(&cmd_call("shutdown -h now"), &make_state())
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn ignores_non_command_tools() {
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: "{}".into(),
            },
        };
        let v = DangerousCommandGuard.check(&call, &make_state()).await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_docker_exec() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("docker exec app-1 cat /app/flag.txt"),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
        assert!(v.guidance.contains("container command"));
    }

    #[tokio::test]
    async fn blocks_docker_ps() {
        let v = DangerousCommandGuard
            .check(&cmd_call("docker ps"), &make_state())
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_docker_logs() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("docker logs app-1 2>&1 | head -50"),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_kubectl_exec() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("kubectl exec -it pod -- cat /flag"),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_nsenter() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("nsenter -t 1234 -m -p -- cat /flag"),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_piped_docker() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("cat ids.txt | docker exec -i app sh"),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn allows_curl_with_docker_in_url() {
        let v = DangerousCommandGuard
            .check(
                &cmd_call("curl http://docker.example.com/api"),
                &make_state(),
            )
            .await;
        assert!(v.allowed);
    }

    fn browser_call(action: &str, extra: &str) -> ToolCall {
        let args = if extra.is_empty() {
            format!(r#"{{"action":"{}"}}"#, action)
        } else {
            format!(r#"{{"action":"{}",{}}}"#, action, extra)
        };
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "browser".into(),
                arguments: args,
            },
        }
    }

    #[tokio::test]
    async fn blocks_browser_navigate_to_localhost() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("navigate", r#""url":"http://127.0.0.1:8080""#),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_browser_navigate_to_private_10() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("navigate", r#""url":"http://10.1.2.3/admin""#),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn blocks_browser_navigate_to_private_192() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("navigate", r#""url":"http://192.168.1.1""#),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn allows_browser_navigate_to_target() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("navigate", r#""url":"http://10.0.0.1:8080/login""#),
                &make_state(),
            )
            .await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn allows_browser_navigate_to_public() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("navigate", r#""url":"http://example.com""#),
                &make_state(),
            )
            .await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_browser_execute_js_window_close() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("execute_js", r#""code":"window.close()""#),
                &make_state(),
            )
            .await;
        assert!(!v.allowed);
    }

    #[tokio::test]
    async fn allows_browser_execute_js_document_cookie() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("execute_js", r#""code":"document.cookie""#),
                &make_state(),
            )
            .await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn allows_browser_click() {
        let v = DangerousCommandGuard
            .check(
                &browser_call("click", r##""selector":"#submit""##),
                &make_state(),
            )
            .await;
        assert!(v.allowed);
    }
}
