use crate::traits::PreGuard;
use holmes_core::state::AttackState;
use holmes_core::{GuardVerdict, ToolCall};
use std::fs;

pub struct FileTrackerPreGuard;

#[async_trait::async_trait]
impl PreGuard for FileTrackerPreGuard {
    fn name(&self) -> &str {
        "read_state_seeding_pre"
    }

    async fn check(&self, call: &ToolCall, state: &AttackState) -> GuardVerdict {
        let name = &call.function.name;
        if name.contains("write") || name.contains("replace") || name.contains("edit") {
            if let Ok(args) = call.args_parsed() {
                let path_opt = args
                    .get("path")
                    .or_else(|| args.get("TargetFile"))
                    .or_else(|| args.get("target_file"))
                    .or_else(|| args.get("AbsolutePath"))
                    .and_then(|v| v.as_str());

                if let Some(path) = path_opt {
                    if let Ok(meta) = fs::metadata(path) {
                        if let Ok(mtime) = meta.modified() {
                            let current_mtime = mtime
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs();

                            if let Some(tracked_mtime) = state.file_access_tracker.get(path) {
                                if *tracked_mtime < current_mtime {
                                    return GuardVerdict::block(format!(
                                        "File {} has been modified since you last read it. You must read it again before modifying to ensure context is up to date.",
                                        path
                                    ));
                                }
                            } else {
                                return GuardVerdict::block(format!(
                                    "You are trying to modify {} but there is no record of you reading it in this session. You must read it before modifying.",
                                    path
                                ));
                            }
                        }
                    }
                }
            }
        }
        GuardVerdict::allow()
    }
}
