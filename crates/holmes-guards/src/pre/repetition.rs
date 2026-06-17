use crate::traits::PreGuard;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};
use std::collections::VecDeque;

pub struct RepetitionGuard {
    window: VecDeque<String>,
    window_size: usize,
    max_repeats: usize,
}

impl RepetitionGuard {
    pub fn new(window_size: usize) -> Self {
        Self {
            window: VecDeque::new(),
            window_size,
            max_repeats: 3,
        }
    }

    fn semantic_signature(call: &ToolCall) -> String {
        let name = &call.function.name;
        let args = &call.function.arguments;

        if name == "http_request" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(url) = v.get("url").and_then(|u| u.as_str()) {
                    let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("GET");
                    let path = Self::normalize_url_path(url);
                    return format!("http_request:{}:{}", method, path);
                }
            }
        }

        if name == "execute_command" || name == "execute_python" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                let text = v
                    .get("command")
                    .or_else(|| v.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or(args);
                if let Some(path) = Self::extract_url_path_from_text(text) {
                    return format!("{}:{}", name, path);
                }
            }
        }

        format!("{}:{}", name, holmes_core::truncate_str(args, 80))
    }

    fn normalize_url_path(url: &str) -> String {
        let after_scheme = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))
            .unwrap_or(url);

        let path = if let Some(slash) = after_scheme.find('/') {
            let p = &after_scheme[slash..];
            let end = p
                .find(|c: char| c == '?' || c == '#' || c == '"' || c == '\'' || c == ' ')
                .unwrap_or(p.len());
            &p[..end]
        } else {
            "/"
        };

        path.split('/')
            .map(|seg| {
                if !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()) {
                    "{id}"
                } else if seg.len() >= 32 && seg.chars().all(|c| c.is_ascii_hexdigit()) {
                    "{hash}"
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn extract_url_path_from_text(text: &str) -> Option<String> {
        let start = text.find("http://").or_else(|| text.find("https://"))?;
        let url_part = &text[start..];
        let end = url_part
            .find(|c: char| c == '"' || c == '\'' || c == ' ' || c == '\\' || c == ')')
            .unwrap_or(url_part.len());
        Some(Self::normalize_url_path(&url_part[..end]))
    }

    pub fn record(&mut self, call: &ToolCall) {
        let sig = Self::semantic_signature(call);
        self.window.push_back(sig);
        while self.window.len() > self.window_size {
            self.window.pop_front();
        }
    }
}

#[async_trait::async_trait]
impl PreGuard for RepetitionGuard {
    fn name(&self) -> &str {
        "repetition"
    }

    async fn check(&self, call: &ToolCall, _state: &AttackState) -> GuardVerdict {
        let sig = Self::semantic_signature(call);
        let count = self.window.iter().filter(|s| **s == sig).count();
        if count >= self.max_repeats {
            return GuardVerdict::block(format!(
                "Semantic pattern '{}' repeated {} times in last {} calls. Switch to a different target or approach.",
                sig, count, self.window_size,
            ));
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

    fn make_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: r#"{"url":"http://t/a"}"#.into(),
            },
        }
    }

    #[tokio::test]
    async fn allows_first_call() {
        let guard = RepetitionGuard::new(10);
        let v = guard.check(&make_call(), &make_state()).await;
        assert!(v.allowed);
    }

    #[tokio::test]
    async fn blocks_after_3_repeats() {
        let mut guard = RepetitionGuard::new(10);
        let call = make_call();
        let state = make_state();

        guard.record(&call);
        guard.record(&call);
        guard.record(&call);

        let v = guard.check(&call, &state).await;
        assert!(!v.allowed);
        assert!(v.guidance.contains("repeated"));
    }

    #[tokio::test]
    async fn window_slides() {
        let mut guard = RepetitionGuard::new(3);
        let call = make_call();
        let state = make_state();

        guard.record(&call);
        guard.record(&call);

        let other = ToolCall {
            id: "2".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "execute_command".into(),
                arguments: r#"{"command":"ls"}"#.into(),
            },
        };
        guard.record(&other);
        guard.record(&other);

        let v = guard.check(&call, &state).await;
        assert!(v.allowed, "old calls should have slid out of window");
    }

    #[tokio::test]
    async fn different_ids_same_semantic_pattern() {
        let mut guard = RepetitionGuard::new(10);
        let state = make_state();

        for i in 1..=3 {
            let call = ToolCall {
                id: format!("{i}"),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "http_request".into(),
                    arguments: format!(r#"{{"url":"http://t/api/users/{i}"}}"#),
                },
            };
            guard.record(&call);
        }

        let next = ToolCall {
            id: "4".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: r#"{"url":"http://t/api/users/99"}"#.into(),
            },
        };
        let v = guard.check(&next, &state).await;
        assert!(
            !v.allowed,
            "different numeric IDs should match same semantic pattern"
        );
    }

    #[tokio::test]
    async fn different_paths_not_blocked() {
        let mut guard = RepetitionGuard::new(10);
        let state = make_state();

        let paths = ["/login", "/admin", "/api/config"];
        for (i, path) in paths.iter().enumerate() {
            let call = ToolCall {
                id: format!("{i}"),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "http_request".into(),
                    arguments: format!(r#"{{"url":"http://t{path}"}}"#),
                },
            };
            guard.record(&call);
        }

        let next = ToolCall {
            id: "9".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "http_request".into(),
                arguments: r#"{"url":"http://t/search"}"#.into(),
            },
        };
        let v = guard.check(&next, &state).await;
        assert!(v.allowed);
    }
}
