use holmes_core::hook::AgentHook;
use holmes_core::tool_types::ToolCall;
use std::path::{Path, PathBuf};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub struct CheckpointHook {
    pub backup_dir: PathBuf,
}

impl CheckpointHook {
    pub fn new(session_id: &str) -> Self {
        let backup_dir = std::env::temp_dir()
            .join("holmes_checkpoints")
            .join(session_id);
        let _ = fs::create_dir_all(&backup_dir);
        Self { backup_dir }
    }

    fn backup_file(&self, target_path: &Path) -> Result<(), String> {
        if !target_path.exists() || !target_path.is_file() {
            return Ok(()); // Nothing to backup
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let file_name = target_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let backup_name = format!("{}_{}.bak", file_name, timestamp);
        let backup_path = self.backup_dir.join(backup_name);

        fs::copy(target_path, &backup_path).map_err(|e| {
            format!(
                "Failed to backup file {} to {}: {}",
                target_path.display(),
                backup_path.display(),
                e
            )
        })?;

        Ok(())
    }
}

impl AgentHook for CheckpointHook {
    fn pre_tool_use(&self, call: &ToolCall) -> Result<(), String> {
        let is_modifying_tool = match call.function.name.as_str() {
            "write_file" | "replace_file_content" | "multi_replace_file_content" | "write_to_file" => true,
            _ => false,
        };

        if !is_modifying_tool {
            return Ok(());
        }

        // Try to extract the target file path from arguments
        let args: serde_json::Value = match serde_json::from_str(&call.function.arguments) {
            Ok(val) => val,
            Err(_) => return Ok(()),
        };

        let target_file_str = match args.get("TargetFile").and_then(|v| v.as_str()) {
            Some(path) => path,
            None => return Ok(()), // Some tools might not have this exact parameter name
        };

        let target_path = Path::new(target_file_str);
        self.backup_file(target_path)
    }
}
