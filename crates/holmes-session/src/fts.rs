/// Sanitize a query string for FTS5.
/// FTS5 has special characters that need quoting: hyphens, dots, etc.
pub fn sanitize_fts5_query(query: &str) -> String {
    let mut result = String::new();
    let needs_quoting = |c: char| matches!(c, '-' | '.' | '@' | '/' | '\\' | ':' | '|' | '^');

    for word in query.split_whitespace() {
        if !result.is_empty() {
            result.push(' ');
        }
        if word.chars().any(needs_quoting) {
            result.push('"');
            result.push_str(word);
            result.push('"');
        } else {
            result.push_str(word);
        }
    }
    result
}

/// Check if a string contains CJK characters that FTS5's default tokenizer
/// handles poorly. Hermes uses a LIKE fallback for these queries.
pub fn contains_cjk(query: &str) -> bool {
    query.chars().any(|c| {
        matches!(c,
            '\u{4E00}'..='\u{9FFF}' |
            '\u{3400}'..='\u{4DBF}' |
            '\u{3040}'..='\u{309F}' |
            '\u{30A0}'..='\u{30FF}' |
            '\u{AC00}'..='\u{D7AF}'
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_fts5_query() {
        assert_eq!(sanitize_fts5_query("simple query"), "simple query");
        assert_eq!(sanitize_fts5_query("host-name.com"), "\"host-name.com\"");
        assert_eq!(sanitize_fts5_query("192.168.1.1"), "\"192.168.1.1\"");
    }

    #[test]
    fn test_contains_cjk() {
        assert!(contains_cjk("SQL注入"));
        assert!(contains_cjk("漏洞扫描"));
        assert!(!contains_cjk("SQL injection"));
    }
}
