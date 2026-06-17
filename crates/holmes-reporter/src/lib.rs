//! holmes-reporter — placeholder.
//!
//! The full reporter implementation from Apeiron V3 depends on
//! `holmes-llm`, `thiserror`, `AttackResult`, and other internal types
//! that have not yet been re-introduced in the Holmes tree. The
//! original source is preserved as `reporter.rs.deferred` for later
//! migration.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportRequest {
    pub title: String,
    pub summary: String,
    pub sections: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReportOutput {
    pub path: String,
    pub bytes: usize,
}

/// Placeholder reporter trait — real Markdown / LLM-backed implementation deferred.
#[async_trait::async_trait]
pub trait Reporter: Send + Sync {
    async fn render(&self, request: &ReportRequest) -> anyhow::Result<ReportOutput>;
}
