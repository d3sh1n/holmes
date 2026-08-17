use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;
use tracing::debug;

use crate::process::{self, ProcessRun};
use crate::registry::{Tool, ToolExecutionFailure};
use holmes_core::execution_context::ExecutionContext;
use holmes_core::{FunctionDefinition, ToolDefinition};

const TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 300;
const OUTPUT_LIMIT: usize = 32768;

pub struct ExecuteCommandTool;

#[derive(Deserialize)]
struct Args {
    command: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_timeout() -> u64 {
    TIMEOUT_SECS
}

#[async_trait::async_trait]
impl Tool for ExecuteCommandTool {
    fn name(&self) -> &str {
        "execute_command"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "execute_command".into(),
                description: "Execute a shell command and return stdout/stderr/exit_code.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "Shell command to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 30)" }
                    },
                    "required": ["command"]
                }),
            },
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, args: &str) -> Result<String> {
        self.execute_with_context(args, &ExecutionContext::default())
            .await
    }

    async fn execute_with_context(&self, args: &str, ctx: &ExecutionContext) -> Result<String> {
        let parsed: Args = serde_json::from_str(args)?;
        debug!(command = %parsed.command, "executing command");

        // The call's own timeout (capped), further capped by the execution context —
        // a command may never outlive its turn.
        let requested = Duration::from_secs(parsed.timeout.min(MAX_TIMEOUT_SECS));
        let deadline = ctx.effective_deadline(Some(requested));
        let started = std::time::Instant::now();

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&parsed.command);
        match process::run_command(cmd, None, deadline, Some(&ctx.token())).await {
            ProcessRun::Completed(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let truncated_stdout = truncate(&stdout, OUTPUT_LIMIT);
                let truncated_stderr = truncate(&stderr, OUTPUT_LIMIT / 4);
                let exit_code = output.status.code().unwrap_or(-1);
                let payload = json!({
                    "stdout": truncated_stdout,
                    "stderr": truncated_stderr,
                    "exit_code": exit_code
                })
                .to_string();
                if output.status.success() {
                    Ok(payload)
                } else {
                    Err(ToolExecutionFailure::failed(payload).into())
                }
            }
            ProcessRun::TimedOut => Err(ToolExecutionFailure::timed_out(timeout_payload(
                self.name(),
                ctx,
                started,
                deadline,
                &parsed.command,
            ))
            .into()),
            ProcessRun::Cancelled => Err(ToolExecutionFailure::cancelled(json!({
                "stdout": "",
                "stderr": format!(
                    "tool '{}' interrupted by cancellation after {}ms (task {}); process group terminated",
                    self.name(),
                    started.elapsed().as_millis(),
                    ctx.task_id()
                ),
                "exit_code": -1
            })
            .to_string())
            .into()),
            ProcessRun::Failed(e) => Err(ToolExecutionFailure::failed(json!({
                "stdout": "",
                "stderr": format!("execution error: {e}"),
                "exit_code": -1
            })
            .to_string())
            .into()),
        }
    }
}

/// Timeout payload carrying the fields the observability contract requires on every
/// timeout: tool, task, elapsed and deadline (mirrored by the `ProcessKilled` /
/// `ToolDeadlineExceeded` tracing events).
fn timeout_payload(
    tool: &str,
    ctx: &ExecutionContext,
    started: std::time::Instant,
    deadline: Duration,
    command: &str,
) -> String {
    let elapsed = started.elapsed();
    tracing::warn!(
        event = "ToolDeadlineExceeded",
        tool = %tool,
        task_id = %ctx.task_id(),
        elapsed_ms = elapsed.as_millis() as u64,
        deadline_ms = deadline.as_millis() as u64,
        "command exceeded its deadline; process group terminated"
    );
    json!({
        "stdout": "",
        "stderr": format!(
            "tool '{tool}' timed out: command exceeded its deadline of {}ms after {}ms elapsed (task {}); process group terminated",
            deadline.as_millis(),
            elapsed.as_millis(),
            ctx.task_id()
        ),
        "command": command,
        "exit_code": -1
    })
    .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    holmes_core::truncate_with_note(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nonzero_exit_is_a_typed_failure() {
        let error = ExecuteCommandTool
            .execute(r#"{"command":"printf boom >&2; exit 7"}"#)
            .await
            .expect_err("non-zero exit must not be returned as success");
        let failure = error
            .downcast_ref::<ToolExecutionFailure>()
            .expect("typed tool failure");
        assert_eq!(failure.status, holmes_core::ToolOutcomeStatus::Failed);
        let payload: serde_json::Value = serde_json::from_str(&failure.content).unwrap();
        assert_eq!(payload["exit_code"], 7);
        assert!(payload["stderr"].as_str().unwrap().contains("boom"));
    }
}
