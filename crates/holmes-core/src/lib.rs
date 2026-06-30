pub mod config;
pub mod error;
pub mod event;
pub mod hook;
pub mod session;
pub mod state;
pub mod subagent;
pub mod tool_types;
pub mod types;
pub mod workflow;

pub use config::*;
pub use event::*;
pub use tool_types::*;
pub use types::*;

pub fn stable_prompt_hash(prompt: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(prompt.as_bytes());
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
