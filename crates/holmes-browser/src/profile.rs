use std::path::{Path, PathBuf};

/// Resolve the per-session browser profile directory.
///
/// Layout: `<sessions_dir>/<sanitized_session_id>/browser-profile`.
/// The session id is sanitized so a hostile or malformed id cannot escape
/// `sessions_dir` via traversal or absolute paths.
pub fn profile_dir_for(sessions_dir: &Path, session_id: &str) -> PathBuf {
    let safe = sanitize_session_id(session_id);
    sessions_dir.join(safe).join("browser-profile")
}

fn sanitize_session_id(session_id: &str) -> String {
    let cleaned: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_string();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_resolves_under_sessions_with_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "abc-123");
        assert_eq!(dir, PathBuf::from("/data/sessions/abc-123/browser-profile"));
    }

    #[test]
    fn profile_dir_rejects_traversal_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "../evil");
        assert!(dir.starts_with("/data/sessions"));
        assert!(!dir.to_string_lossy().contains(".."));
        assert!(dir
            .components()
            .all(|c| !matches!(c, std::path::Component::ParentDir)));
    }

    #[test]
    fn profile_dir_handles_empty_session_id() {
        let dir = profile_dir_for(Path::new("/data/sessions"), "");
        assert_eq!(dir, PathBuf::from("/data/sessions/session/browser-profile"));
    }
}
