pub mod browser;
pub mod execute_command;
pub mod execute_python;
pub mod http_request;
pub mod hypothesis;
pub mod report_finding;
pub mod report_progress;
pub mod report_recon;
pub mod subagent;

use crate::registry::ToolRegistry;
use holmes_browser::BrowserManager;
use holmes_core::config::HolmesConfig;
use holmes_core::subagent::SubagentRunner;
use std::sync::Arc;

pub fn register_all(
    registry: &mut ToolRegistry,
    _config: &HolmesConfig,
    runner: Option<Arc<dyn SubagentRunner>>,
    browser: Option<Arc<BrowserManager>>,
) {
    registry.register(Box::new(execute_command::ExecuteCommandTool));
    registry.register(Box::new(execute_python::ExecutePythonTool));
    registry.register(Box::new(http_request::HttpRequestTool::new()));
    registry.register(Box::new(report_finding::ReportFindingTool));
    registry.register(Box::new(report_progress::ReportProgressTool));
    registry.register(Box::new(report_recon::ReportReconTool));
    registry.register(Box::new(hypothesis::AddHypothesisTool));
    registry.register(Box::new(hypothesis::RejectHypothesisTool));
    registry.register(Box::new(hypothesis::ConfirmHypothesisTool));

    if let Some(r) = runner {
        registry.register(Box::new(subagent::SpawnSubagentTool::new(r)));
    }
    if let Some(mgr) = browser {
        registry.register(Box::new(browser::BrowserTool::new(mgr)));
    }
}
