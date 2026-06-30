use std::path::PathBuf;

pub fn profile_dir_for(_sessions_dir: &std::path::Path, _session_id: &str) -> PathBuf {
    PathBuf::new()
}
