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

const TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 300;
const OUTPUT_LIMIT: usize = 32768;

const AUTO_IMPORTS: &str = r#"
import sys, os, re, json, base64, hashlib, urllib.parse, subprocess
from pathlib import Path
try:
    import requests
except ImportError:
    pass
"#;

pub struct ExecutePythonTool;

#[derive(Deserialize)]
struct Args {
    code: String,
    #[serde(default = "default_timeout")]
    timeout: u64,
}

fn default_timeout() -> u64 {
    TIMEOUT_SECS
}

#[async_trait::async_trait]
impl Tool for ExecutePythonTool {
    fn name(&self) -> &str {
        "execute_python"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".into(),
            function: FunctionDefinition {
                name: "execute_python".into(),
                description: "Execute Python code. Common libs auto-imported (requests, re, json, base64, hashlib, etc).".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "description": "Python code to execute" },
                        "timeout": { "type": "integer", "description": "Timeout in seconds (default 60)" }
                    },
                    "required": ["code"]
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
        debug!(code_len = parsed.code.len(), "executing python");

        let full_code = format!("{}\n{}", AUTO_IMPORTS, parsed.code);
        // Unique temp *directory* per call holding the script. Python puts the
        // script's own directory at sys.path[0], so every import performs path
        // lookups there; writing the script directly into the shared system
        // temp root means those lookups scan whatever unrelated files have
        // accumulated there (leaked session/test dirs can number in the
        // thousands), which under parallel execution made imports slow enough
        // to hit the tool deadline — and a stray `json.py`-style file there
        // could even shadow stdlib modules. A private, near-empty directory
        // avoids both. TempDir deletes the directory (and script) on drop.
        // When the turn has an isolated scratch directory installed (subagent
        // runs, AGT-014), the per-call directory nests inside it instead of
        // the shared system temp.
        let parent = match ctx.temp_dir() {
            Some(dir) => {
                tokio::fs::create_dir_all(dir).await?;
                dir.to_path_buf()
            }
            None => std::env::temp_dir(),
        };
        let scratch = tempfile::Builder::new()
            .prefix("holmes_py_")
            .tempdir_in(&parent)?;
        let script = scratch.path().join("script.py");
        tokio::fs::write(&script, &full_code).await?;

        let requested = Duration::from_secs(parsed.timeout.min(MAX_TIMEOUT_SECS));
        let deadline = ctx.effective_deadline(Some(requested));
        let started = std::time::Instant::now();

        let mut cmd = Command::new("python3");
        cmd.arg(&script);
        match process::run_command(cmd, None, deadline, Some(&ctx.token())).await {
            ProcessRun::Completed(output) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let exit_code = output.status.code().unwrap_or(-1);
                let payload = json!({
                    "stdout": truncate(&stdout, OUTPUT_LIMIT),
                    "stderr": truncate(&stderr, OUTPUT_LIMIT / 4),
                    "exit_code": exit_code,
                })
                .to_string();
                if output.status.success() {
                    Ok(payload)
                } else {
                    Err(ToolExecutionFailure::failed(payload).into())
                }
            }
            ProcessRun::TimedOut => {
                let elapsed = started.elapsed();
                tracing::warn!(
                    event = "ToolDeadlineExceeded",
                    tool = %self.name(),
                    task_id = %ctx.task_id(),
                    elapsed_ms = elapsed.as_millis() as u64,
                    deadline_ms = deadline.as_millis() as u64,
                    "python execution exceeded its deadline; process group terminated"
                );
                Err(ToolExecutionFailure::timed_out(json!({
                    "stdout": "",
                    "stderr": format!(
                        "tool '{}' timed out: python exceeded its deadline of {}ms after {}ms elapsed (task {}); process group terminated",
                        self.name(),
                        deadline.as_millis(),
                        elapsed.as_millis(),
                        ctx.task_id()
                    ),
                    "exit_code": -1,
                })
                .to_string())
                .into())
            }
            ProcessRun::Cancelled => Err(ToolExecutionFailure::cancelled(json!({
                "stdout": "",
                "stderr": format!(
                    "tool '{}' interrupted by cancellation after {}ms (task {}); process group terminated",
                    self.name(),
                    started.elapsed().as_millis(),
                    ctx.task_id()
                ),
                "exit_code": -1,
            })
            .to_string())
            .into()),
            ProcessRun::Failed(e) => Err(ToolExecutionFailure::failed(json!({
                "stdout": "",
                "stderr": format!("execution error: {e}"),
                "exit_code": -1,
            })
            .to_string())
            .into()),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    holmes_core::truncate_with_note(s, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn parallel_executions_use_isolated_temp_files() {
        if !python3_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        // 100 concurrent executions (workflow B acceptance): every call must get its
        // own script file and its own output — no cross-talk, no clobbering.
        let mut handles = Vec::new();
        for i in 0..100 {
            let args = json!({ "code": format!("print('task-{i}')") }).to_string();
            handles.push(tokio::spawn(async move {
                // The tool is stateless; a fresh instance per task exercises concurrent
                // temp-file usage within one process (shared PID).
                ExecutePythonTool.execute(&args).await.expect("execute")
            }));
        }
        let mut outputs = std::collections::HashSet::new();
        for (i, handle) in handles.into_iter().enumerate() {
            let out = handle.await.expect("join");
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            let stdout = parsed["stdout"].as_str().unwrap().trim().to_string();
            assert_eq!(
                stdout,
                format!("task-{i}"),
                "call {i} returned wrong output"
            );
            outputs.insert(stdout);
        }
        assert_eq!(
            outputs.len(),
            100,
            "every parallel call returned its own output"
        );
    }

    #[tokio::test]
    async fn multibyte_output_truncates_without_panic() {
        if !python3_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let tool = ExecutePythonTool;
        // Emit well over OUTPUT_LIMIT bytes of 3-byte characters so the cut lands
        // mid-character without the boundary-safe truncation.
        let args = json!({ "code": "print('中' * 40000)" }).to_string();
        let out = tool.execute(&args).await.expect("execute");
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        let stdout = parsed["stdout"].as_str().unwrap();
        assert!(stdout.contains("[truncated,"), "expected truncation note");
    }

    #[tokio::test]
    async fn timeout_terminates_process_group_and_reports_context_fields() {
        if !python3_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let tool = ExecutePythonTool;
        let ctx = ExecutionContext::new("py-timeout-task");
        // Fork a child sleep inside the python script: the whole tree must die.
        let args = json!({
            "code": "import subprocess, time\nsubprocess.Popen(['sleep', '300'])\ntime.sleep(300)",
            "timeout": 1
        })
        .to_string();
        let start = std::time::Instant::now();
        let error = tool
            .execute_with_context(&args, &ctx)
            .await
            .expect_err("timeout is not a successful tool execution");
        let elapsed = start.elapsed();
        let failure = error
            .downcast_ref::<ToolExecutionFailure>()
            .expect("typed timeout");
        assert_eq!(failure.status, holmes_core::ToolOutcomeStatus::TimedOut);
        let parsed: serde_json::Value = serde_json::from_str(&failure.content).unwrap();
        let stderr = parsed["stderr"].as_str().unwrap();
        assert!(stderr.contains("timed out"), "got: {stderr}");
        // Timeout payload must carry tool, task, elapsed and deadline.
        assert!(stderr.contains("execute_python"), "got: {stderr}");
        assert!(stderr.contains("py-timeout-task"), "got: {stderr}");
        assert!(stderr.contains("elapsed"), "got: {stderr}");
        assert!(stderr.contains("deadline"), "got: {stderr}");
        assert!(
            elapsed < Duration::from_secs(10),
            "bounded at ~1s, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_a_typed_failure() {
        if !python3_available() {
            eprintln!("python3 not available; skipping");
            return;
        }
        let error = ExecutePythonTool
            .execute(r#"{"code":"raise SystemExit(9)"}"#)
            .await
            .expect_err("non-zero python exit must not be returned as success");
        let failure = error
            .downcast_ref::<ToolExecutionFailure>()
            .expect("typed tool failure");
        assert_eq!(failure.status, holmes_core::ToolOutcomeStatus::Failed);
        let payload: serde_json::Value = serde_json::from_str(&failure.content).unwrap();
        assert_eq!(payload["exit_code"], 9);
    }
}
