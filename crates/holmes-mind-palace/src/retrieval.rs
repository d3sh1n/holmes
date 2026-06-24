use holmes_core::event::Event;

pub struct Retrieval;

impl Retrieval {
    pub fn extract_features(events: &[Event]) -> Vec<String> {
        let mut features = Vec::new();
        for event in events.iter().rev().take(20) {
            match event {
                Event::ToolCall {
                    name, arguments, ..
                } => {
                    features.push(name.clone());
                    if let Some(args) = arguments.as_object() {
                        for v in args.values() {
                            if let Some(s) = v.as_str() {
                                if s.len() < 100 {
                                    features.push(s.to_string());
                                }
                            }
                        }
                    }
                }
                Event::ToolResult { name, content, .. } => {
                    features.push(name.clone());
                    for word in content.split_whitespace().take(10) {
                        if word.len() > 3 && !word.starts_with("http") {
                            features.push(word.to_string());
                        }
                    }
                }
                Event::VulnerabilityFound { title, .. } => {
                    features.push(title.clone());
                }
                Event::DirectiveSet { attack_type, .. } => {
                    if let Some(at) = attack_type {
                        features.push(at.clone());
                    }
                }
                _ => {}
            }
        }
        features
    }

    pub fn build_query(features: &[String]) -> String {
        features.join(" OR ")
    }
}
