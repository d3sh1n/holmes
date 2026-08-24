pub mod bounty;
pub mod browser;
pub mod codec;
pub mod execute_command;
pub mod execute_python;
pub mod file_ops;
pub mod http_request;
pub mod read_pdf;
pub mod report_finding;
pub mod report_progress;
pub mod report_recon;
pub mod search;
pub mod subagent;
pub mod web_fetch;

use crate::registry::ToolRegistry;
use holmes_browser::BrowserManager;
use holmes_core::background::{BackgroundTasks, DurableTaskBinding};
use holmes_core::config::HolmesConfig;
use holmes_core::subagent::SubagentRunner;
use std::sync::Arc;

pub fn register_all(
    registry: &mut ToolRegistry,
    config: &HolmesConfig,
    runner: Option<Arc<dyn SubagentRunner>>,
    browser: Option<Arc<BrowserManager>>,
    background_tasks: Option<BackgroundTasks>,
    durable_tasks: Option<DurableTaskBinding>,
    subagent_slots: Option<Arc<tokio::sync::Semaphore>>,
) {
    registry.register(Box::new(execute_command::ExecuteCommandTool));
    registry.register(Box::new(execute_python::ExecutePythonTool));
    registry.register(Box::new(http_request::HttpRequestTool::new()));
    registry.register(Box::new(web_fetch::WebFetchTool::new()));
    registry.register(Box::new(codec::CodecTool));
    registry.register(Box::new(read_pdf::ReadPdfTool));
    registry.register(Box::new(file_ops::ReadFileTool));
    registry.register(Box::new(file_ops::WriteFileTool));
    registry.register(Box::new(file_ops::EditFileTool));
    registry.register(Box::new(file_ops::WriteTodosTool));
    registry.register(Box::new(search::GrepTool));
    registry.register(Box::new(search::GlobTool));
    registry.register(Box::new(report_finding::ReportFindingTool));
    registry.register(Box::new(report_progress::ReportProgressTool));
    registry.register(Box::new(report_recon::ReportReconTool));
    registry.register(Box::new(bounty::SetProgramScopeTool));
    registry.register(Box::new(bounty::GetProgramScopeTool));
    registry.register(Box::new(bounty::RecordAssetTool));
    registry.register(Box::new(bounty::ListAssetsTool));
    registry.register(Box::new(bounty::GenerateBountyReportTool));
    if let Some(r) = runner {
        // The background task registry must be the SAME handle the surface wires into
        // the runtime context, or completions would never be drained/injected. A
        // missing handle falls back to a detached registry: background spawns still
        // run and stay queryable, only the auto-injection is inert.
        let tasks = background_tasks.unwrap_or_default();
        // Isolation knobs (AGT-014): depth comes from config; the concurrency
        // semaphore must be shared by the caller across nesting levels — a fresh
        // one per registry would multiply the cap with every nested subagent.
        let limits = holmes_core::subagent::SubagentLimits {
            max_depth: config.subagent.max_depth,
            slots: subagent_slots.unwrap_or_else(|| {
                Arc::new(tokio::sync::Semaphore::new(
                    (config.subagent.max_concurrent as usize).max(1),
                ))
            }),
        };
        let mut spawn_tool = subagent::SpawnSubagentTool::new(r, tasks.clone())
            .with_limits(limits)
            .with_heartbeat_interval(std::time::Duration::from_millis(
                config.experiments.heartbeat_ms.max(1),
            ));
        if let Some(binding) = durable_tasks {
            spawn_tool = spawn_tool.with_durable_binding(binding);
        }
        registry.register(Box::new(spawn_tool));
        registry.register(Box::new(subagent::GetTaskOutputTool::new(tasks)));
    }
    if let Some(mgr) = browser {
        registry.register(Box::new(browser::BrowserTool::new(mgr)));
    }
}
