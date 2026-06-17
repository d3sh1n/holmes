use serde::{Deserialize, Serialize};

/// Immutable fields — set once at construction, never modified.
/// All fields are pub(crate) for read access, no setters exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImmutableFields {
    target_url: String,
    target_ip: String,
    challenge_id: String,
    challenge_name: String,
    hints: Vec<String>,
}

impl ImmutableFields {
    pub(crate) fn new(
        target_url: String,
        target_ip: String,
        challenge_id: String,
        challenge_name: String,
        hints: Vec<String>,
    ) -> Self {
        Self {
            target_url,
            target_ip,
            challenge_id,
            challenge_name,
            hints,
        }
    }

    pub fn target_url(&self) -> &str {
        &self.target_url
    }
    pub fn target_ip(&self) -> &str {
        &self.target_ip
    }
    pub fn challenge_id(&self) -> &str {
        &self.challenge_id
    }
    pub fn challenge_name(&self) -> &str {
        &self.challenge_name
    }
    pub fn hints(&self) -> &[String] {
        &self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn immutable_fields_are_readable() {
        let f = ImmutableFields::new(
            "http://target:8080".into(),
            "10.0.0.1".into(),
            "ch-001".into(),
            "SQLi Challenge".into(),
            vec!["search box".into()],
        );
        assert_eq!(f.target_url(), "http://target:8080");
        assert_eq!(f.target_ip(), "10.0.0.1");
        assert_eq!(f.challenge_id(), "ch-001");
        assert_eq!(f.challenge_name(), "SQLi Challenge");
        assert_eq!(f.hints().len(), 1);
    }
}
