use serde::{Deserialize, Serialize};

/// Supervisor decision output — injected into the agent's conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Directive {
    pub attack_type: String,
    pub objective: String,
    pub entry_points: Vec<String>,
    pub reasoning: String,
    pub recommended_skills: Vec<String>,
}

/// A candidate attack chain discovered during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateChain {
    pub attack_type: String,
    pub entry_point: String,
    pub confidence: f32,
    pub source: String,
    pub attempted: bool,
}

impl Directive {
    pub fn to_injection_message(&self) -> String {
        let mut parts = vec![
            "[战略调整] 切换攻击方向。".to_string(),
            format!("攻击类型: {}", self.attack_type),
            format!("目标: {}", self.objective),
        ];
        if !self.entry_points.is_empty() {
            parts.push(format!("入口点: {}", self.entry_points.join(", ")));
        }
        parts.push(format!("原因: {}", self.reasoning));
        if !self.recommended_skills.is_empty() {
            parts.push(format!("推荐技能: {}", self.recommended_skills.join(", ")));
        }
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directive_injection_message_format() {
        let d = Directive {
            attack_type: "sqli".into(),
            objective: "extract admin creds".into(),
            entry_points: vec!["http://target/search".into()],
            reasoning: "search box has unfiltered input".into(),
            recommended_skills: vec!["sqli-bypass".into()],
        };
        let msg = d.to_injection_message();
        assert!(msg.contains("sqli"));
        assert!(msg.contains("extract admin creds"));
        assert!(msg.contains("http://target/search"));
        assert!(msg.contains("sqli-bypass"));
    }

    #[test]
    fn candidate_chain_default_not_attempted() {
        let chain = CandidateChain {
            attack_type: "idor".into(),
            entry_point: "/api/users/3".into(),
            confidence: 0.7,
            source: "evidence_extractor".into(),
            attempted: false,
        };
        assert!(!chain.attempted);
    }
}
