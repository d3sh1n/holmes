use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_mind_palace::MindPalace;
use holmes_session::db::SessionDB;
use holmes_session::memory_store::MemoryStore;
use std::sync::Arc;

use crate::turn::{TurnPhase, TurnState};

pub struct TurnEngine {
    pub mind_palace: MindPalace,
    session_db: Arc<SessionDB>,
    session_id: String,
    turn_index: u64,
    #[allow(dead_code)]
    config: holmes_core::config::HolmesConfig,
}

impl TurnEngine {
    pub fn new(
        session_id: String,
        session_db: Arc<SessionDB>,
        long_term: Arc<MemoryStore>,
        config: holmes_core::config::HolmesConfig,
    ) -> Self {
        Self {
            mind_palace: MindPalace::new(session_db.clone(), long_term),
            session_db,
            session_id,
            turn_index: 0,
            config,
        }
    }

    pub async fn resume(
        session_id: &str,
        session_db: Arc<SessionDB>,
        long_term: Arc<MemoryStore>,
        config: holmes_core::config::HolmesConfig,
    ) -> Result<Self, String> {
        let session = session_db
            .get_session(session_id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("session not found: {}", session_id))?;

        let mind_palace = MindPalace::from_events(session_id, session_db.clone(), long_term).await?;

        Ok(Self {
            mind_palace,
            session_db,
            session_id: session_id.to_string(),
            turn_index: session.message_count,
            config,
        })
    }

    pub async fn turn(
        &mut self,
        input: UserInput,
    ) -> Result<TurnResult, String> {
        self.turn_index += 1;
        let events_start = self.mind_palace.memory.event_count() as u64;

        let mut turn_state = TurnState::new(self.turn_index, events_start);

        let user_content = match &input {
            UserInput::Message { content } => content.clone(),
            UserInput::SlashCommand { command, args } => format!("/{} {}", command, args),
            UserInput::DirectTool { tool_name, arguments } => format!("!{} {}", tool_name, arguments),
        };

        let user_event = Event::UserMessage {
            content: user_content.clone(),
            timestamp: chrono::Utc::now(),
        };
        self.emit_event(&user_event).await?;

        turn_state.transition(TurnPhase::Understanding);
        let context_injection = self.build_context_injection().await?;

        turn_state.transition(TurnPhase::Executing);
        let thinking = Event::Thinking {
            content: format!("分析用户请求: {}", user_content),
            reasoning_type: None,
        };
        self.emit_event(&thinking).await?;

        let reply = format!("[Holmes] 收到您的请求。上下文: {}", context_injection);

        turn_state.transition(TurnPhase::Replying);
        let events_end = self.mind_palace.memory.event_count() as u64;
        let complete_event = Event::TurnComplete {
            event_range: (events_start, events_end),
            tokens_used: turn_state.tokens.clone(),
            sub_agents_spawned: turn_state.sub_agents_spawned.clone(),
        };
        self.emit_event(&complete_event).await?;

        self.session_db
            .update_token_counts(&self.session_id, &turn_state.tokens)
            .await
            .map_err(|e| e.to_string())?;

        let mut result = turn_state.complete(events_end);
        result.reply = reply;
        result.dashboard_snapshot = Some(self.mind_palace.dashboard(&SessionMode::Pentest));
        Ok(result)
    }

    async fn build_context_injection(&self) -> Result<String, String> {
        Ok(self.mind_palace.situation_summary())
    }

    async fn emit_event(&mut self, event: &Event) -> Result<(), String> {
        self.session_db
            .append_event(&self.session_id, event)
            .await
            .map_err(|e| e.to_string())?;
        self.mind_palace.ingest(event.clone());
        Ok(())
    }
}
