/// Deterministic local text embedding for hybrid memory recall (AGT-011).
///
/// There is no remote embedding provider in the LLM layer, so semantic recall
/// uses a hashed bag-of-features vector: lowercase word unigrams plus
/// character trigrams are hashed into a fixed-width vector and L2-normalized.
/// Character trigrams give partial credit for morphological variants and work
/// for CJK text, where the FTS5 tokenizer falls back to `LIKE`. A small domain
/// synonym expansion (`expand_query`) covers common security abbreviations so
/// a query like "sqli" can recall a memory about "SQL injection".
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;

pub const EMBEDDING_DIM: usize = 256;

/// Weight of character trigrams relative to word unigrams.
const TRIGRAM_WEIGHT: f32 = 0.5;

pub fn embed(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0f32; EMBEDDING_DIM];
    let lower = text.to_lowercase();

    for token in lower.split(|c: char| !c.is_alphanumeric()) {
        if token.is_empty() {
            continue;
        }
        add_feature(&mut vector, token, 1.0);
    }

    let chars: Vec<char> = lower.chars().collect();
    for window in chars.windows(3) {
        if window.iter().all(|c| c.is_alphanumeric()) {
            let gram: String = window.iter().collect();
            add_feature(&mut vector, &gram, TRIGRAM_WEIGHT);
        }
    }

    let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut vector {
            *v /= norm;
        }
    }
    vector
}

fn add_feature(vector: &mut [f32], feature: &str, weight: f32) {
    let mut hasher = DefaultHasher::new();
    hasher.write(feature.as_bytes());
    let hash = hasher.finish();
    let index = (hash % EMBEDDING_DIM as u64) as usize;
    // Sign from a second bit of the hash reduces systematic collisions.
    let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
    vector[index] += sign * weight;
}

/// Cosine similarity of two L2-normalized embeddings (dot product).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum::<f32>() as f64
}

/// Expand well-known security abbreviations in a query so lexical and
/// semantic recall both see the long form alongside the abbreviation.
pub fn expand_query(query: &str) -> String {
    const SYNONYMS: &[(&[&str], &str)] = &[
        (&["sqli", "sql-injection"], "sql injection"),
        (&["xss"], "cross site scripting"),
        (&["ssrf"], "server side request forgery"),
        (&["csrf"], "cross site request forgery"),
        (&["rce"], "remote code execution"),
        (&["lfi"], "local file inclusion"),
        (&["rfi"], "remote file inclusion"),
        (&["idor"], "insecure direct object reference"),
        (&["xxe"], "xml external entity"),
        (&["ssti"], "server side template injection"),
        (&["privesc"], "privilege escalation"),
        (&["authn"], "authentication"),
        (&["authz"], "authorization"),
        (&["enum"], "enumeration"),
        (&["cmdi"], "command injection"),
    ];

    let mut expanded = query.to_string();
    let lower = query.to_lowercase();
    for token in lower.split(|c: char| !c.is_alphanumeric() && c != '-') {
        for (needles, expansion) in SYNONYMS {
            if needles.contains(&token) && !lower.contains(expansion) {
                expanded.push(' ');
                expanded.push_str(expansion);
            }
        }
    }
    expanded
}

pub fn embedding_to_json(embedding: &[f32]) -> String {
    serde_json::to_string(embedding).unwrap_or_else(|_| "[]".into())
}

pub fn embedding_from_json(json: &str) -> Option<Vec<f32>> {
    let parsed: Vec<f32> = serde_json::from_str(json).ok()?;
    if parsed.len() == EMBEDDING_DIM {
        Some(parsed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_texts_score_higher_than_unrelated() {
        let a = embed("SQL injection via UNION SELECT on the login form");
        let b = embed("UNION SELECT sql injection bypasses login");
        let c = embed("phillips hue firmware update notes");
        assert!(cosine(&a, &b) > cosine(&a, &c));
        assert!(cosine(&a, &b) > 0.3);
    }

    #[test]
    fn expansion_covers_security_abbreviations() {
        assert!(expand_query("sqli on login").contains("sql injection"));
        assert!(expand_query("test for XSS").contains("cross site scripting"));
        // No duplication when the long form is already present.
        assert_eq!(
            expand_query("sql injection")
                .matches("sql injection")
                .count(),
            1
        );
    }

    #[test]
    fn embedding_json_roundtrip() {
        let embedding = embed("roundtrip test");
        let json = embedding_to_json(&embedding);
        assert_eq!(embedding_from_json(&json), Some(embedding));
        assert_eq!(embedding_from_json("[1.0, 2.0]"), None);
        assert_eq!(embedding_from_json("not json"), None);
    }
}
