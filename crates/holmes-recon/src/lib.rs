//! holmes-recon — placeholder.
//!
//! The full auto_recon implementation depends on `holmes-tools`,
//! `regex`, `events`, and `hypothesis` modules that have not yet been
//! re-introduced in the Holmes tree. The original source is preserved
//! as `auto_recon.rs.deferred` for later migration.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconTarget {
    pub host: String,
    pub ports: Vec<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReconReport {
    pub targets: Vec<ReconTarget>,
    pub notes: Vec<String>,
}

/// Placeholder recon runner. Real nmap / dirb / wfuzz orchestration deferred.
pub struct AutoRecon;

impl AutoRecon {
    pub fn new() -> Self { Self }

    pub async fn run(&self, _target: &str) -> anyhow::Result<ReconReport> {
        Ok(ReconReport::default())
    }
}
