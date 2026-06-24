use crate::traits::PostGuard;
use holmes_core::state::AttackState;
use holmes_core::{ToolCall, ToolResult};
use std::fs;

pub struct FileTrackerPostGuard;

#[async_trait::async_trait]
impl PostGuard for FileTrackerPostGuard {
    fn name(&self) -> &str {
        "read_state_seeding_post"
    }

    async fn process(&mut self, call: &ToolCall, result: &ToolResult, state: &mut AttackState) {
        if result.is_error {
            return;
        }

        let name = &call.function.name;
        if name.contains("read") || name.contains("view") || name.contains("cat") {
            if let Ok(args) = call.args_parsed() {
                let path_opt = args
                    .get("path")
                    .or_else(|| args.get("AbsolutePath"))
                    .or_else(|| args.get("target_file"))
                    .and_then(|v| v.as_str());

                if let Some(path) = path_opt {
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(mtime) = meta.modified() {
                            let mtime_secs = mtime
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();
                            state.file_access_tracker.insert(path.to_string(), mtime_secs);
                        }
                    }
                }
            }
        }
    }
}
