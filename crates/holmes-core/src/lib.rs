pub mod background;
pub mod bounty;
pub mod config;
pub mod error;
pub mod event;
pub mod execution_context;
pub mod hook;
pub mod ledger;
pub mod metrics;
pub mod sensitive;
pub mod session;
pub mod state;
pub mod subagent;
pub mod tool_types;
pub mod types;
pub mod workflow;

pub use config::*;
pub use event::*;
pub use sensitive::screen_sensitive;
pub use tool_types::*;
pub use types::*;

pub use background::{BackgroundTasks, FinishedTask, TaskId, TaskState, TaskStatus};

pub fn stable_prompt_hash(prompt: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256.digest(prompt.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SHA-256 hex digest of arbitrary content. Used to bind evidence records to the
/// exact tool output they were derived from (integrity, not secrecy).
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256.digest(content.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_prompt_hash_uses_sha256_hex() {
        assert_eq!(
            stable_prompt_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
