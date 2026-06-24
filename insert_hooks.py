import sys

with open('crates/holmes-runtime/src/action.rs', 'r') as f:
    content = f.read()

replacement1 = """            let mut hook_blocked = None;
            for hook in &self.hooks {
                if let Err(e) = hook.pre_tool_use(call) {
                    hook_blocked = Some(e);
                    break;
                }
            }

            if let Some(reason) = hook_blocked {
                record_tool_call(context, call).await?;
                record_tool_blocked(context, call, "hook", &reason).await?;
                let result = ToolResult::blocked(&call.id, reason);
                batch.messages.push(result.to_message_with_vision());
                let finished = DialogueEngine::tool_finished(&result);
                sink.emit_yield(&context.session_id, finished.clone());
                batch.events.push(finished);
                batch.results.push(result);
                continue;
            }

            let started = DialogueEngine::tool_started(call);"""

content = content.replace("let started = DialogueEngine::tool_started(call);", replacement1)

replacement2 = """            for hook in &self.hooks {
                let _ = hook.post_tool_use(call, &result);
            }

            batch.messages.push(result.to_message_with_vision());"""

content = content.replace("batch.messages.push(result.to_message_with_vision());", replacement2)

with open('crates/holmes-runtime/src/action.rs', 'w') as f:
    f.write(content)

