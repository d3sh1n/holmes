//! holmes-skills — placeholder.
//!
//! The full skill_store and skill_evolver implementations from
//! Apeiron V3 require yaml/embedding/llm dependencies that have not
//! yet been wired into the Holmes workspace. The original sources are
//! preserved alongside this file with a `.deferred` extension so they
//! can be ported once the dependencies (serde_yaml, holmes-llm, etc.)
//! and supporting types (SkillMeta) are available.

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Lightweight skill metadata stub matching the YAML front-matter schema.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// Placeholder in-memory skill record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
}

/// Placeholder skill store. Real implementation deferred — see
/// `skill_store.rs.deferred`.
#[derive(Debug, Default)]
pub struct SkillStore {
    skills: Vec<Skill>,
}

impl SkillStore {
    pub fn new() -> Self { Self::default() }

    pub fn list(&self) -> &[Skill] { &self.skills }

    pub fn add(&mut self, skill: Skill) { self.skills.push(skill); }

    pub fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.meta.name == name)
    }
}
