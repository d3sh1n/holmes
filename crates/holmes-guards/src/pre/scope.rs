//! `ScopeGuard` — config-driven engagement scope guard over tool arguments.
//!
//! Unlike the state-based `ImmutableFieldGuard` (which only fires when the runtime sets a
//! target — never, in the current production path), this guard is built from
//! `guards.scope` and rejects out-of-allowlist requests the moment an allowlist is
//! configured. It inspects `http_request` / `web_fetch` / `browser` (navigate) URLs and
//! `execute_command` / `execute_python` command text, extracts every host/IP referenced,
//! and blocks anything not in scope. Private / loopback / link-local / cloud-metadata
//! addresses are blocked unless `allow_private` (defence against SSRF-to-internal even
//! when an allowlist would otherwise match).
//!
//! This is a HEURISTIC guard, not a hard security boundary (see P0-01): it only checks
//! the initial arguments of known tools. Redirect targets, DNS changes between check and
//! connect, unknown or MCP tools, and hosts constructed dynamically at runtime (shell
//! variables, subprocesses) can all bypass it. The model remains responsible for staying
//! within the authorized scope.

use crate::traits::PreGuard;
use holmes_core::config::ScopeConfig;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};
use std::net::Ipv4Addr;

pub struct ScopeGuard {
    allow: Vec<String>,
    deny: Vec<String>,
    allow_private: bool,
}

impl ScopeGuard {
    pub fn new(cfg: &ScopeConfig) -> Self {
        Self {
            allow: cfg.allow.iter().map(|s| s.to_lowercase()).collect(),
            deny: cfg.deny.iter().map(|s| s.to_lowercase()).collect(),
            allow_private: cfg.allow_private,
        }
    }

    /// Enforcement is on only when an allowlist is configured (fail-closed). With no
    /// allowlist the guard is a no-op so lab/local usage isn't broken.
    pub fn enforcing(&self) -> bool {
        !self.allow.is_empty()
    }

    fn host_verdict(&self, host: &str) -> Result<(), String> {
        let host = host.trim().trim_end_matches('.').to_lowercase();
        if host.is_empty() {
            return Ok(());
        }
        if self.deny.iter().any(|d| matches_entry(&host, d)) {
            return Err(format!(
                "host '{host}' is explicitly out of scope (deny list)"
            ));
        }
        // Private / metadata addresses are blocked unless explicitly permitted.
        if is_private_or_metadata(&host) && !self.allow_private {
            // still require it to be in the allowlist AND allow_private to pass
            return Err(format!(
                "host '{host}' is a private/loopback/link-local/metadata address; blocked \
                 (set guards.scope.allow_private to permit)"
            ));
        }
        if !self.allow.iter().any(|a| matches_entry(&host, a)) {
            return Err(format!(
                "host '{host}' is not in the engagement scope allowlist — only assigned \
                 targets may be touched"
            ));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PreGuard for ScopeGuard {
    fn name(&self) -> &str {
        "scope"
    }

    async fn check(&self, call: &ToolCall, _state: &AttackState) -> GuardVerdict {
        if !self.enforcing() {
            return GuardVerdict::allow();
        }
        let args = &call.function.arguments;
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap_or_default();

        let hosts: Vec<String> = match call.function.name.as_str() {
            "http_request" | "web_fetch" => parsed
                .get("url")
                .and_then(|v| v.as_str())
                .and_then(host_of_url)
                .into_iter()
                .collect(),
            "browser" => {
                // Only navigation introduces a new host; other actions stay on the page.
                let action = parsed.get("action").and_then(|v| v.as_str()).unwrap_or("");
                if action == "navigate" {
                    parsed
                        .get("url")
                        .and_then(|v| v.as_str())
                        .and_then(host_of_url)
                        .into_iter()
                        .collect()
                } else {
                    Vec::new()
                }
            }
            "execute_command" => {
                extract_hosts(parsed.get("command").and_then(|v| v.as_str()).unwrap_or(""))
            }
            "execute_python" => {
                extract_hosts(parsed.get("code").and_then(|v| v.as_str()).unwrap_or(""))
            }
            _ => Vec::new(),
        };

        for host in hosts {
            if let Err(reason) = self.host_verdict(&host) {
                return GuardVerdict::block(reason);
            }
        }
        GuardVerdict::allow()
    }
}

/// Does `host` match an allow/deny entry? Supports exact host, domain-suffix
/// (`example.com` matches `api.example.com`), bare IP, and IPv4 CIDR (`10.0.0.0/8`).
fn matches_entry(host: &str, entry: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if host == entry {
        return true;
    }
    // Domain suffix: entry "example.com" matches "*.example.com".
    if !entry.contains('/') && host.ends_with(&format!(".{entry}")) {
        return true;
    }
    // CIDR match (IPv4).
    if let Some((base, bits)) = parse_cidr(entry) {
        if let Ok(ip) = host.parse::<Ipv4Addr>() {
            let ip_u = u32::from(ip);
            let mask = if bits == 0 {
                0
            } else {
                u32::MAX << (32 - bits)
            };
            return (ip_u & mask) == (base & mask);
        }
    }
    false
}

fn parse_cidr(entry: &str) -> Option<(u32, u8)> {
    let (addr, bits) = entry.split_once('/')?;
    let ip: Ipv4Addr = addr.parse().ok()?;
    let bits: u8 = bits.parse().ok()?;
    if bits > 32 {
        return None;
    }
    Some((u32::from(ip), bits))
}

fn host_of_url(url: &str) -> Option<String> {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    // strip userinfo@ and :port
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// Heuristic host extraction from a shell command or python source: every URL host plus
/// every bare IPv4 and domain-like token. Conservative — better to over-detect (and
/// block an off-scope host) than to miss one.
fn extract_hosts(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let url_re = regex::Regex::new(r#"[a-zA-Z][a-zA-Z0-9+.-]*://[^\s'"]+"#).unwrap();
    for m in url_re.find_iter(text) {
        if let Some(h) = host_of_url(m.as_str()) {
            out.push(h);
        }
    }
    // bare IPv4
    let ip_re = regex::Regex::new(r"\b\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}\b").unwrap();
    for m in ip_re.find_iter(text) {
        out.push(m.as_str().to_string());
    }
    // domain-like tokens (foo.bar.tld) not already covered
    let dom_re = regex::Regex::new(r"\b(?:[a-zA-Z0-9-]+\.)+[a-zA-Z]{2,}\b").unwrap();
    for m in dom_re.find_iter(text) {
        let d = m.as_str().to_string();
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

fn is_private_or_metadata(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.octets() == [169, 254, 169, 254] // cloud metadata (also link-local)
            || ip.is_unspecified();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(allow: &[&str], deny: &[&str], allow_private: bool) -> ScopeGuard {
        ScopeGuard::new(&ScopeConfig {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            allow_private,
        })
    }

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: holmes_core::FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    fn dummy_state() -> AttackState {
        AttackState::new(String::new(), String::new(), "c".into(), "t".into(), vec![])
    }

    #[tokio::test]
    async fn no_allowlist_is_noop() {
        let g = guard(&[], &[], false);
        let v = g
            .check(
                &call("http_request", r#"{"url":"http://evil.com/x"}"#),
                &dummy_state(),
            )
            .await;
        assert!(v.allowed, "no scope configured → not enforced");
    }

    #[tokio::test]
    async fn blocks_off_scope_url_and_allows_in_scope() {
        let g = guard(&["example.com"], &[], false);
        let blocked = g
            .check(
                &call("http_request", r#"{"url":"http://evil.com/x"}"#),
                &dummy_state(),
            )
            .await;
        assert!(!blocked.allowed);
        let allowed = g
            .check(
                &call("http_request", r#"{"url":"https://api.example.com/login"}"#),
                &dummy_state(),
            )
            .await;
        assert!(allowed.allowed, "domain suffix should match");
    }

    #[tokio::test]
    async fn web_fetch_and_browser_navigate_are_scoped() {
        let g = guard(&["example.com"], &[], false);
        assert!(
            !g.check(
                &call("web_fetch", r#"{"url":"http://evil.com"}"#),
                &dummy_state()
            )
            .await
            .allowed
        );
        assert!(
            !g.check(
                &call(
                    "browser",
                    r#"{"action":"navigate","url":"http://evil.com"}"#
                ),
                &dummy_state()
            )
            .await
            .allowed
        );
        // non-navigate browser actions stay on the page → allowed
        assert!(
            g.check(
                &call("browser", r#"{"action":"screenshot"}"#),
                &dummy_state()
            )
            .await
            .allowed
        );
    }

    #[tokio::test]
    async fn command_hostname_is_extracted_and_blocked() {
        let g = guard(&["example.com"], &[], false);
        let v = g
            .check(
                &call(
                    "execute_command",
                    r#"{"command":"curl http://victim.other.com/x"}"#,
                ),
                &dummy_state(),
            )
            .await;
        assert!(!v.allowed, "hostname in command must be caught");
    }

    #[tokio::test]
    async fn cidr_allow_and_private_block() {
        let g = guard(&["10.0.0.0/8"], &[], false);
        // in-CIDR but private → blocked unless allow_private
        let v = g
            .check(
                &call("http_request", r#"{"url":"http://10.1.2.3/"}"#),
                &dummy_state(),
            )
            .await;
        assert!(!v.allowed, "private blocked without allow_private");

        let g2 = guard(&["10.0.0.0/8"], &[], true);
        let v2 = g2
            .check(
                &call("http_request", r#"{"url":"http://10.1.2.3/"}"#),
                &dummy_state(),
            )
            .await;
        assert!(v2.allowed, "private allowed with allow_private + CIDR");
    }

    #[tokio::test]
    async fn metadata_endpoint_blocked_even_with_broad_allow() {
        let g = guard(&["169.254.0.0/16"], &[], false);
        let v = g
            .check(
                &call(
                    "browser",
                    r#"{"action":"navigate","url":"http://169.254.169.254/latest/meta-data/"}"#,
                ),
                &dummy_state(),
            )
            .await;
        assert!(
            !v.allowed,
            "cloud metadata must be blocked without allow_private"
        );
    }

    #[tokio::test]
    async fn deny_overrides_allow() {
        let g = guard(&["example.com"], &["secret.example.com"], false);
        let v = g
            .check(
                &call("http_request", r#"{"url":"http://secret.example.com/"}"#),
                &dummy_state(),
            )
            .await;
        assert!(!v.allowed, "deny wins over allow");
    }
}
