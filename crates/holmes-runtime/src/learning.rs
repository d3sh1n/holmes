use holmes_core::event::{Event, StoredEvent};
use holmes_core::truncate_str;
use holmes_core::types::{MemoryCategory, MemorySource, MemoryStatus};
use holmes_session::memory_store::{MemoryEntry, StoreError};

use crate::context::RuntimeContext;
use crate::deliberation::RuntimeError;
use crate::memory::store_transition_error;

const MAX_CANDIDATE_CONTENT_BYTES: usize = 800;

#[derive(Debug, Clone, Default)]
pub struct LearningEngine;

#[derive(Debug, Clone, Default)]
pub struct LearningReview {
    pub candidates: Vec<LearningCandidate>,
    pub rationale: String,
    pub trigger: String,
}

#[derive(Debug, Clone)]
pub enum LearningCandidate {
    Memory(MemoryCandidate),
}

#[derive(Debug, Clone)]
pub struct MemoryCandidate {
    pub category: MemoryCategory,
    pub content: String,
    pub tags: Vec<String>,
    pub relevance_score: f64,
    /// Provenance. Agent-inferred candidates may only enter long-term memory
    /// as staged; user/tool-originated candidates may activate directly.
    pub source: MemorySource,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LearningApplication {
    pub applied: usize,
    pub staged: usize,
    pub rejected: usize,
}

impl LearningEngine {
    pub fn new() -> Self {
        Self
    }

    pub fn review_turn(
        &self,
        context: &RuntimeContext,
        turn_events: &[StoredEvent],
    ) -> LearningReview {
        if !context.config.learning.enabled {
            return LearningReview::default();
        }

        let mut candidates = Vec::new();
        let max_candidates = context.config.learning.max_candidates_per_turn;
        for event in turn_events {
            match &event.event {
                Event::UserMessage { content, .. } => {
                    if !looks_like_watson_correction(content) {
                        continue;
                    }

                    let content = format!(
                        "Watson preference or correction: {}",
                        truncate_str(content.trim(), MAX_CANDIDATE_CONTENT_BYTES)
                    );
                    // User-originated: may activate directly (subject to the
                    // optional write-approval config).
                    candidates.push(LearningCandidate::Memory(MemoryCandidate {
                        category: MemoryCategory::TargetKnowledge,
                        content,
                        tags: vec![
                            "learning".into(),
                            "watson_correction".into(),
                            "preference".into(),
                        ],
                        relevance_score: 0.86,
                        source: MemorySource::User,
                    }));
                }
                // A verified-satisfied goal yields a skill candidate
                // (AGT-012). Skill candidates are agent inferences and always
                // stay staged until validated and approved.
                Event::GoalEvaluated {
                    satisfied: true,
                    reason,
                    ..
                } if context.config.learning.skill_extraction && reason.contains("[verified") => {
                    let content = format!(
                        "Skill candidate from verified goal completion: {}",
                        truncate_str(reason.trim(), MAX_CANDIDATE_CONTENT_BYTES)
                    );
                    candidates.push(LearningCandidate::Memory(MemoryCandidate {
                        category: MemoryCategory::Skill,
                        content,
                        tags: vec!["learning".into(), "skill".into(), "verified_goal".into()],
                        relevance_score: 0.8,
                        source: MemorySource::AgentInferred,
                    }));
                }
                _ => continue,
            }

            if candidates.len() >= max_candidates {
                break;
            }
        }

        let trigger = if candidates.is_empty() {
            String::new()
        } else if candidates.iter().any(|candidate| {
            matches!(
                candidate,
                LearningCandidate::Memory(MemoryCandidate {
                    category: MemoryCategory::Skill,
                    ..
                })
            )
        }) {
            "verified_goal".into()
        } else {
            "watson_correction".into()
        };
        let rationale = if candidates.is_empty() {
            String::new()
        } else {
            format!(
                "Detected {} learnable signal(s) (Watson corrections and/or verified goals).",
                candidates.len()
            )
        };

        LearningReview {
            candidates,
            rationale,
            trigger,
        }
    }

    pub async fn apply_review(
        &self,
        context: &mut RuntimeContext,
        review: LearningReview,
    ) -> Result<LearningApplication, RuntimeError> {
        let mut application = LearningApplication::default();
        let candidate_count = review.candidates.len();

        for candidate in review.candidates {
            match candidate {
                LearningCandidate::Memory(candidate) => {
                    if let Some(reason) = reject_memory_candidate(&candidate) {
                        record_learning_event(
                            context,
                            Event::LearningCandidateRejected {
                                kind: "memory".into(),
                                reason,
                            },
                        )
                        .await?;
                        application.rejected += 1;
                        continue;
                    }

                    if self
                        .is_duplicate_memory(context, &candidate.content)
                        .await?
                    {
                        record_learning_event(
                            context,
                            Event::LearningCandidateRejected {
                                kind: "memory".into(),
                                reason: "duplicate memory candidate".into(),
                            },
                        )
                        .await?;
                        application.rejected += 1;
                        continue;
                    }

                    // Agent inferences and skills may only ever be staged
                    // (AGT-012); user/tool-originated candidates activate
                    // directly unless write approval is configured.
                    let must_stage = matches!(candidate.category, MemoryCategory::Skill)
                        || matches!(candidate.source, MemorySource::AgentInferred)
                        || context.config.learning.memory_write_approval;
                    let is_user_correction = matches!(candidate.source, MemorySource::User);

                    let status = if must_stage {
                        MemoryStatus::Staged
                    } else {
                        MemoryStatus::Active
                    };
                    let entry = MemoryEntry {
                        category: candidate.category.clone(),
                        content: candidate.content.clone(),
                        tags: candidate.tags.clone(),
                        success: true,
                        relevance_score: candidate.relevance_score,
                        source_session_id: Some(context.session_id.clone()),
                        source: candidate.source,
                        status: Some(status),
                        ..Default::default()
                    };
                    let stored = context.memory_store.store(entry).await;
                    match stored {
                        Ok(_outcome) if must_stage => {
                            let metrics = holmes_core::metrics::metrics();
                            metrics.count("memory.learning.staged");
                            if is_user_correction {
                                metrics.count("memory.learning.user_correction");
                            }
                            record_learning_event(
                                context,
                                Event::MemoryWriteStaged {
                                    content: candidate.content,
                                    reason: review.rationale.clone(),
                                },
                            )
                            .await?;
                            application.staged += 1;
                        }
                        Ok(_outcome) => {
                            let metrics = holmes_core::metrics::metrics();
                            metrics.count("memory.learning.applied");
                            if is_user_correction {
                                metrics.count("memory.learning.user_correction");
                            }
                            record_learning_event(
                                context,
                                Event::MemoryStored {
                                    category: candidate.category,
                                    content: candidate.content,
                                    tags: candidate.tags,
                                    relevance_score: candidate.relevance_score,
                                    source_session_id: Some(context.session_id.clone()),
                                },
                            )
                            .await?;
                            application.applied += 1;
                        }
                        Err(StoreError::Rejected(reason)) => {
                            // Store-level screening caught what the candidate
                            // filter missed; audited as a rejection.
                            record_learning_event(
                                context,
                                Event::LearningCandidateRejected {
                                    kind: "memory".into(),
                                    reason,
                                },
                            )
                            .await?;
                            application.rejected += 1;
                        }
                        Err(error) => {
                            return Err(RuntimeError::recoverable(format!(
                                "failed to store learned memory: {error}"
                            )));
                        }
                    }
                }
            }
        }

        record_learning_event(
            context,
            Event::LearningReviewCompleted {
                candidates: candidate_count,
                applied: application.applied,
                staged: application.staged,
            },
        )
        .await?;

        // `learning.skill_write_approval = false` automates the approval half of
        // the AGT-012 gate: staged skills that already carry a recorded passed
        // validation promote without waiting for a manual approver. The
        // validation requirement itself is not configurable.
        if !context.config.learning.skill_write_approval {
            let promotable = context
                .memory_store
                .staged_validated_skills()
                .await
                .map_err(|error| {
                    RuntimeError::recoverable(format!(
                        "failed to list staged validated skills: {error}"
                    ))
                })?;
            for skill in promotable {
                context
                    .memory_store
                    .promote(&skill.id, "auto:learning.skill_write_approval=false")
                    .await
                    .map_err(store_transition_error)?;
                record_learning_event(
                    context,
                    Event::MemoryStatusChanged {
                        memory_id: skill.id.clone(),
                        from_status: "staged".into(),
                        to_status: "active".into(),
                        reason:
                            "auto-approved (learning.skill_write_approval=false); validation passed"
                                .into(),
                    },
                )
                .await?;
            }
        }

        Ok(application)
    }

    async fn is_duplicate_memory(
        &self,
        context: &RuntimeContext,
        content: &str,
    ) -> Result<bool, RuntimeError> {
        let normalized = normalize_text(content);
        let results = context
            .memory_store
            .search(content, 8)
            .await
            .map_err(|error| {
                RuntimeError::recoverable(format!("failed to search learned memories: {error}"))
            })?;

        Ok(results
            .iter()
            .any(|memory| normalize_text(&memory.content) == normalized))
    }
}

pub async fn record_review_started(
    context: &mut RuntimeContext,
    trigger: String,
    event_range: (u64, u64),
) -> Result<(), RuntimeError> {
    record_learning_event(
        context,
        Event::LearningReviewStarted {
            trigger,
            event_range,
        },
    )
    .await
}

async fn record_learning_event(
    context: &mut RuntimeContext,
    event: Event,
) -> Result<(), RuntimeError> {
    context
        .session_db
        .append_event(&context.session_id, &event)
        .await
        .map_err(|error| {
            RuntimeError::recoverable(format!(
                "failed to persist learning event for session {}: {}",
                context.session_id, error
            ))
        })?;
    context.mind_palace.ingest(event);
    Ok(())
}

fn looks_like_watson_correction(content: &str) -> bool {
    let normalized = content.to_lowercase();
    if looks_like_turn_scoped_instruction(&normalized) {
        return false;
    }

    let explicit_memory_signal = [
        "remember",
        "please remember",
        "next time",
        "going forward",
        "from now on",
        "we prefer",
        "i prefer",
        "my preference",
        "our preference",
        "correction",
        "记住",
        "请记住",
        "下次",
        "以后",
        "后续",
        "我们偏好",
        "我的偏好",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));

    if explicit_memory_signal {
        return true;
    }

    let persistent_chinese_preference = (normalized.contains("我们希望")
        || normalized.contains("希望你"))
        && (normalized.contains("以后")
            || normalized.contains("下次")
            || normalized.contains("默认")
            || normalized.contains("长期")
            || normalized.contains("偏好"));

    let persistent_english_preference = (normalized.contains("always")
        || normalized.contains("prefer"))
        && (normalized.contains("future")
            || normalized.contains("next")
            || normalized.contains("default")
            || normalized.contains("going forward"));

    persistent_chinese_preference || persistent_english_preference
}

fn looks_like_turn_scoped_instruction(normalized: &str) -> bool {
    let scoped_task_markers = [
        "do not call tools",
        "don't call tools",
        "without tools",
        "no tools",
        "do not use tools",
        "不要调用工具",
        "请不要调用工具",
        "不要使用工具",
        "不要输出 markdown",
        "不要解释",
        "只回答",
        "原样输出",
    ];

    scoped_task_markers
        .iter()
        .any(|needle| normalized.contains(needle))
        && ![
            "remember",
            "next time",
            "going forward",
            "from now on",
            "记住",
            "下次",
            "以后",
            "后续",
            "偏好",
        ]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn reject_memory_candidate(candidate: &MemoryCandidate) -> Option<String> {
    // Shared screening (holmes-core): secrets, credentials and prompt
    // injection never reach long-term memory. The store re-screens at the
    // write boundary as defense in depth.
    holmes_core::screen_sensitive(&candidate.content)
}

fn normalize_text(content: &str) -> String {
    content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use holmes_core::config::HolmesConfig;
    use holmes_core::session::RuntimeSession;
    use holmes_core::{LlmResponse, SessionMode};
    use holmes_guards::GuardChain;
    use holmes_mind_palace::MindPalace;
    use holmes_session::{memory_store::MemoryStore, CreateSessionParams, SessionDB, SessionStore};
    use holmes_tools::ToolRegistry;

    use crate::context::RuntimeState;
    use crate::deliberation::StaticLlmBackend;

    use super::*;

    #[test]
    fn watson_correction_creates_memory_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![StoredEvent {
            id: 1,
            session_id: "session-1".into(),
            event_index: 0,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event: Event::UserMessage {
                content: "Remember next time: we prefer HEAD before GET.".into(),
                timestamp: chrono::Utc::now(),
            },
        }];

        let review = LearningEngine::new().review_turn(&context, &events);

        assert_eq!(review.candidates.len(), 1);
        assert_eq!(review.trigger, "watson_correction");
        // LearningCandidate currently has a single variant, so this binding is
        // irrefutable; reintroduce a check when more variants are added.
        let LearningCandidate::Memory(candidate) = &review.candidates[0];
        assert!(candidate.content.contains("HEAD before GET"));
    }

    #[test]
    fn turn_scoped_tool_instruction_does_not_create_memory_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![user_event("请不要调用工具，不要输出 Markdown，只回答 ok。")];

        let review = LearningEngine::new().review_turn(&context, &events);

        assert!(review.candidates.is_empty());
        assert!(review.trigger.is_empty());
    }

    #[test]
    fn live_deduction_probe_does_not_create_memory_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![user_event(
            "请不要调用工具。请先把以下观察写入 Holmes deduction ledger，然后再给出一句最终回答：观察 evidence-admin-403：/admin 返回 403。",
        )];

        let review = LearningEngine::new().review_turn(&context, &events);

        assert!(review.candidates.is_empty());
        assert!(review.trigger.is_empty());
    }

    #[test]
    fn chinese_future_preference_creates_memory_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![user_event("以后请优先使用 HEAD 请求做安全探测。")];

        let review = LearningEngine::new().review_turn(&context, &events);

        assert_eq!(review.candidates.len(), 1);
        assert_eq!(review.trigger, "watson_correction");
    }

    #[tokio::test]
    async fn approval_mode_stages_memory_candidate() {
        let mut config = HolmesConfig::default();
        config.learning.memory_write_approval = true;
        let mut context = make_context(config).await;
        let review = LearningReview {
            candidates: vec![LearningCandidate::Memory(MemoryCandidate {
                category: MemoryCategory::TargetKnowledge,
                content: "Watson preference or correction: Remember to prefer safe probes.".into(),
                tags: vec!["learning".into()],
                relevance_score: 0.86,
                source: MemorySource::User,
            })],
            rationale: "test".into(),
            trigger: "watson_correction".into(),
        };

        let application = LearningEngine::new()
            .apply_review(&mut context, review)
            .await
            .expect("apply review");

        assert_eq!(
            application,
            LearningApplication {
                applied: 0,
                staged: 1,
                rejected: 0,
            }
        );
        // The staged candidate is persisted (so it can later be validated and
        // promoted) but is invisible to turn-time recall.
        let memories = context
            .memory_store
            .search("safe probes", 3)
            .await
            .expect("search");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].status, MemoryStatus::Staged);
        let recalled = context
            .memory_store
            .recall("safe probes", 3, true, None)
            .await
            .expect("recall");
        assert!(recalled.hits.is_empty());

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::MemoryWriteStaged { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event.event, Event::LearningReviewCompleted { .. })));
    }

    #[test]
    fn verified_goal_creates_staged_skill_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![StoredEvent {
            id: 1,
            session_id: "session-1".into(),
            event_index: 0,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event: Event::GoalEvaluated {
                satisfied: true,
                reason: "Goal achieved. [verified; evidence: login-bypass]".into(),
                turn_count: 3,
                tokens_spent: 1200,
            },
        }];

        let review = LearningEngine::new().review_turn(&context, &events);

        assert_eq!(review.candidates.len(), 1);
        assert_eq!(review.trigger, "verified_goal");
        let LearningCandidate::Memory(candidate) = &review.candidates[0];
        assert_eq!(candidate.category, MemoryCategory::Skill);
        assert_eq!(candidate.source, MemorySource::AgentInferred);
    }

    #[test]
    fn unverified_goal_creates_no_skill_candidate() {
        let context = make_context_sync(HolmesConfig::default());
        let events = vec![StoredEvent {
            id: 1,
            session_id: "session-1".into(),
            event_index: 0,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event: Event::GoalEvaluated {
                satisfied: true,
                reason: "The model claims the goal is done.".into(),
                turn_count: 3,
                tokens_spent: 1200,
            },
        }];

        let review = LearningEngine::new().review_turn(&context, &events);
        assert!(review.candidates.is_empty());
    }

    #[tokio::test]
    async fn agent_inferred_candidate_is_staged_even_without_approval_config() {
        let mut context = make_context(HolmesConfig::default()).await;
        let review = LearningReview {
            candidates: vec![LearningCandidate::Memory(MemoryCandidate {
                category: MemoryCategory::Skill,
                content: "Skill candidate from verified goal completion: enumerate then fuzz."
                    .into(),
                tags: vec!["learning".into(), "skill".into()],
                relevance_score: 0.8,
                source: MemorySource::AgentInferred,
            })],
            rationale: "test".into(),
            trigger: "verified_goal".into(),
        };

        let application = LearningEngine::new()
            .apply_review(&mut context, review)
            .await
            .expect("apply review");

        assert_eq!(application.staged, 1);
        assert_eq!(application.applied, 0);
        let memories = context
            .memory_store
            .search("enumerate then fuzz", 3)
            .await
            .expect("search");
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0].status, MemoryStatus::Staged);
        assert_eq!(memories[0].category, MemoryCategory::Skill);
        // Unvalidated skill cannot be promoted — the production-skill gate.
        assert!(context
            .memory_store
            .promote(&memories[0].id, "watson")
            .await
            .is_err());
    }

    /// Seed a staged skill with a recorded passed validation and return its id.
    async fn seed_validated_staged_skill(context: &RuntimeContext, content: &str) -> String {
        let outcome = context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                content: content.into(),
                tags: vec!["learning".into(), "skill".into()],
                success: true,
                relevance_score: 0.8,
                source_session_id: Some(context.session_id.clone()),
                source: MemorySource::AgentInferred,
                status: Some(MemoryStatus::Staged),
                ..Default::default()
            })
            .await
            .expect("seed skill");
        context
            .memory_store
            .record_validation(&outcome.id, true)
            .await
            .expect("record validation");
        outcome.id
    }

    fn empty_review() -> LearningReview {
        LearningReview {
            candidates: Vec::new(),
            rationale: "interval review".into(),
            trigger: "verified_goal".into(),
        }
    }

    #[tokio::test]
    async fn skill_write_approval_off_auto_promotes_validated_skill() {
        let mut config = HolmesConfig::default();
        config.learning.skill_write_approval = false;
        let mut context = make_context(config).await;
        let skill_id = seed_validated_staged_skill(&context, "enumerate then fuzz").await;

        LearningEngine::new()
            .apply_review(&mut context, empty_review())
            .await
            .expect("apply review");

        let memory = context
            .memory_store
            .get(&skill_id)
            .await
            .expect("get")
            .expect("skill exists");
        assert_eq!(memory.status, MemoryStatus::Active);
        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| matches!(
            &event.event,
            Event::MemoryStatusChanged { memory_id, to_status, .. }
                if memory_id == &skill_id && to_status == "active"
        )));
    }

    #[tokio::test]
    async fn skill_write_approval_on_keeps_validated_skill_staged() {
        // Default: approval is manual — validation alone never activates a skill.
        let mut context = make_context(HolmesConfig::default()).await;
        let skill_id = seed_validated_staged_skill(&context, "enumerate then fuzz").await;

        LearningEngine::new()
            .apply_review(&mut context, empty_review())
            .await
            .expect("apply review");

        let memory = context
            .memory_store
            .get(&skill_id)
            .await
            .expect("get")
            .expect("skill exists");
        assert_eq!(memory.status, MemoryStatus::Staged);
    }

    #[tokio::test]
    async fn skill_write_approval_off_never_promotes_unvalidated_skill() {
        // The validation half of the AGT-012 gate is not configurable.
        let mut config = HolmesConfig::default();
        config.learning.skill_write_approval = false;
        let mut context = make_context(config).await;
        let outcome = context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::Skill,
                content: "unvalidated skill".into(),
                tags: vec!["learning".into(), "skill".into()],
                success: true,
                relevance_score: 0.8,
                source_session_id: Some(context.session_id.clone()),
                source: MemorySource::AgentInferred,
                status: Some(MemoryStatus::Staged),
                ..Default::default()
            })
            .await
            .expect("seed skill");

        LearningEngine::new()
            .apply_review(&mut context, empty_review())
            .await
            .expect("apply review");

        let memory = context
            .memory_store
            .get(&outcome.id)
            .await
            .expect("get")
            .expect("skill exists");
        assert_eq!(memory.status, MemoryStatus::Staged);
    }

    #[tokio::test]
    async fn secret_like_memory_candidate_is_rejected() {
        let mut context = make_context(HolmesConfig::default()).await;
        let review = LearningReview {
            candidates: vec![LearningCandidate::Memory(MemoryCandidate {
                category: MemoryCategory::TargetKnowledge,
                content: "Watson preference or correction: remember password=hunter2".into(),
                tags: vec!["learning".into()],
                relevance_score: 0.86,
                source: MemorySource::User,
            })],
            rationale: "test".into(),
            trigger: "watson_correction".into(),
        };

        let application = LearningEngine::new()
            .apply_review(&mut context, review)
            .await
            .expect("apply review");

        assert_eq!(application.rejected, 1);
        let memories = context
            .memory_store
            .search("hunter2", 3)
            .await
            .expect("search");
        assert!(memories.is_empty());

        let events = context
            .session_db
            .get_events(&context.session_id)
            .await
            .expect("events");
        assert!(events.iter().any(|event| {
            matches!(
                &event.event,
                Event::LearningCandidateRejected { reason, .. }
                    if reason.contains("secret")
            )
        }));
    }

    #[tokio::test]
    async fn duplicate_memory_candidate_is_rejected() {
        let mut context = make_context(HolmesConfig::default()).await;
        let content = "Watson preference or correction: Remember to prefer HEAD before GET.";
        context
            .memory_store
            .store(MemoryEntry {
                category: MemoryCategory::TargetKnowledge,
                content: content.into(),
                tags: vec!["learning".into()],
                success: true,
                relevance_score: 0.86,
                source_session_id: Some(context.session_id.clone()),
                ..Default::default()
            })
            .await
            .expect("seed memory");
        let review = LearningReview {
            candidates: vec![LearningCandidate::Memory(MemoryCandidate {
                category: MemoryCategory::TargetKnowledge,
                content: content.into(),
                tags: vec!["learning".into()],
                relevance_score: 0.86,
                source: MemorySource::User,
            })],
            rationale: "test".into(),
            trigger: "watson_correction".into(),
        };

        let application = LearningEngine::new()
            .apply_review(&mut context, review)
            .await
            .expect("apply review");

        assert_eq!(application.rejected, 1);
    }

    fn make_context_sync(config: HolmesConfig) -> RuntimeContext {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(make_context(config))
    }

    fn user_event(content: &str) -> StoredEvent {
        StoredEvent {
            id: 1,
            session_id: "session-1".into(),
            event_index: 0,
            turn_index: None,
            timestamp: chrono::Utc::now(),
            event: Event::UserMessage {
                content: content.into(),
                timestamp: chrono::Utc::now(),
            },
        }
    }

    async fn make_context(config: HolmesConfig) -> RuntimeContext {
        let session_id = "session-1".to_string();
        let session_db = Arc::new(SessionDB::open(":memory:").await.expect("session db"));
        session_db
            .create_session(CreateSessionParams {
                id: Some(session_id.clone()),
                title: None,
                mode: Some(SessionMode::Pentest),
                model: None,
                system_prompt: None,
                parent_session_id: None,
                fork_point: None,
                source: Some("test".into()),
                tags: Vec::new(),
            })
            .await
            .expect("create session");

        let memory_store = Arc::new(MemoryStore::open(":memory:").await.expect("memory store"));
        let session = RuntimeSession::new(session_id.clone(), SessionMode::Pentest);
        RuntimeContext::new(
            session,
            session_db.clone(),
            memory_store.clone(),
            MindPalace::new(session_db, memory_store),
            Arc::new(StaticLlmBackend::new(LlmResponse {
                content: None,
                tool_calls: Vec::new(),
                finish_reason: None,
                usage: None,
                ..Default::default()
            })),
            Arc::new(ToolRegistry::new()),
            GuardChain::new(),
            RuntimeState::new(SessionMode::Pentest),
            config,
        )
    }
}
