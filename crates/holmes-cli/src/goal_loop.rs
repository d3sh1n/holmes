use holmes_core::session::RuntimeSession;
use holmes_core::tool_types::Message;
use holmes_llm::client::LlmClient;
use holmes_session::selector::Selector;
use std::sync::Arc;

/// Result of one goal evaluation cycle
pub struct GoalEvalResult {
    pub satisfied: bool,
    pub reason: String,
    pub turns: u64,
}

/// Run the goal loop: repeatedly select workflow → execute → evaluate until condition met
pub async fn run_goal_loop(
    selector: &Selector,
    session: &mut RuntimeSession,
    llm: &Arc<LlmClient>,
    condition: &str,
    max_turns: Option<u32>,
) -> anyhow::Result<GoalEvalResult> {
    let max = max_turns.unwrap_or(50);
    let mut turns = 0u64;

    // Inject goal context
    session.messages.push(Message::user(format!(
        "[Goal] 目标: {}\n请自主完成此目标。每轮完成后我会评估进度。", condition
    )));

    loop {
        if turns >= max as u64 {
            return Ok(GoalEvalResult {
                satisfied: false,
                reason: format!("达到最大轮次限制 ({})", max),
                turns,
            });
        }
        turns += 1;

        // Select and execute workflow
        match selector.select(session, llm).await {
            Ok(Some(name)) => {
                if let Some(wf) = selector.get(&name) {
                    wf.forward(session).await?;
                }
            }
            Ok(None) => {
                // Selector returned DONE — evaluate condition
                let eval = evaluate_condition(session, condition, llm).await?;
                if eval.satisfied {
                    return Ok(GoalEvalResult {
                        satisfied: true,
                        reason: eval.reason,
                        turns,
                    });
                }
                // Not satisfied yet — inject feedback and continue
                session.messages.push(Message::user(format!(
                    "[评估] 条件尚未满足: {}\n请继续努力完成目标。", eval.reason
                )));
            }
            Err(e) => {
                tracing::warn!(error = %e, "selector error in goal loop");
                return Ok(GoalEvalResult {
                    satisfied: false,
                    reason: format!("Selector error: {}", e),
                    turns,
                });
            }
        }
    }
}

/// Evaluate whether the goal condition is met using the LLM
async fn evaluate_condition(
    session: &RuntimeSession,
    condition: &str,
    llm: &LlmClient,
) -> anyhow::Result<GoalEvalResult> {
    let summary: String = session.messages.iter()
        .filter_map(|m| m.content.as_ref())
        .map(|c| c.chars().take(200).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");

    // Char-safe truncation to ~4000 chars to avoid slicing in the middle of a UTF-8 codepoint.
    let truncated: String = summary.chars().take(4000).collect();

    let prompt = format!(
        "完成条件: {}\n\n对话记录摘要:\n{}\n\n请判断以上条件是否已满足。只回答 YES 或 NO，然后给出一句话理由。",
        condition,
        truncated
    );

    match llm.chat_completion_oneshot(&prompt, "判断条件是否满足。只回答 YES/NO + 理由。", "attack_agent").await {
        Ok(resp) => {
            let text = resp.content.unwrap_or_default();
            let satisfied = text.trim().to_uppercase().starts_with("YES");
            Ok(GoalEvalResult { satisfied, reason: text.trim().to_string(), turns: 0 })
        }
        Err(e) => {
            Ok(GoalEvalResult { satisfied: false, reason: format!("Evaluator error: {}", e), turns: 0 })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_goal_eval_result() {
        let r = GoalEvalResult { satisfied: true, reason: "done".into(), turns: 5 };
        assert!(r.satisfied);
        assert_eq!(r.turns, 5);
    }

    #[test]
    fn test_goal_eval_not_satisfied() {
        let r = GoalEvalResult { satisfied: false, reason: "not yet".into(), turns: 10 };
        assert!(!r.satisfied);
        assert_eq!(r.reason, "not yet");
    }
}
