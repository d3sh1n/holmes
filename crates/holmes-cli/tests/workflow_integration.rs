use async_trait::async_trait;
use holmes_core::session::RuntimeSession;
use holmes_core::types::SessionMode;
use holmes_core::workflow::{Workflow, WorkflowError};
use holmes_session::selector::Selector;
use std::sync::atomic::{AtomicU32, Ordering};

/// Test workflow that counts how many times it was called
struct CountingWorkflow {
    name: String,
    desc: String,
    count: AtomicU32,
}

impl CountingWorkflow {
    fn new(name: &str, desc: &str) -> Self {
        Self { name: name.into(), desc: desc.into(), count: AtomicU32::new(0) }
    }
    fn count(&self) -> u32 { self.count.load(Ordering::SeqCst) }
}

#[async_trait]
impl Workflow for CountingWorkflow {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.desc }
    async fn forward(&self, session: &mut RuntimeSession) -> Result<(), WorkflowError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        // Simulate adding a message
        session.messages.push(
            holmes_core::tool_types::Message::user(format!("Workflow {} executed", self.name))
        );
        Ok(())
    }
}

#[test]
fn test_selector_register_and_get() {
    let mut selector = Selector::new();
    let recon = Box::new(CountingWorkflow::new("recon", "信息收集"));
    let exploit = Box::new(CountingWorkflow::new("exploit", "漏洞利用"));

    selector.register(recon);
    selector.register(exploit);

    assert_eq!(selector.workflow_names().len(), 2);
    assert!(selector.get("recon").is_some());
    assert!(selector.get("exploit").is_some());
    assert!(selector.get("nonexistent").is_none());
}

#[test]
fn test_selector_prompt_contains_all_workflows() {
    let mut selector = Selector::new();
    selector.register(Box::new(CountingWorkflow::new("recon", "信息收集：扫描端口")));
    selector.register(Box::new(CountingWorkflow::new("analysis", "深度分析：代码审计")));

    let prompt = selector.selector_prompt();
    assert!(prompt.contains("recon"));
    assert!(prompt.contains("信息收集"));
    assert!(prompt.contains("analysis"));
    assert!(prompt.contains("深度分析"));
    assert!(prompt.contains("DONE"));
}

#[tokio::test]
async fn test_workflow_forward_modifies_session() {
    let wf = CountingWorkflow::new("test", "test workflow");
    let mut session = RuntimeSession::new("test-1".into(), SessionMode::Pentest);

    wf.forward(&mut session).await.unwrap();

    assert_eq!(wf.count(), 1);
    assert!(session.message_count() > 0);
}

#[tokio::test]
async fn test_multiple_workflows_chain() {
    let wf1 = CountingWorkflow::new("recon", "recon");
    let wf2 = CountingWorkflow::new("exploit", "exploit");
    let mut session = RuntimeSession::new("chain-1".into(), SessionMode::Pentest);

    wf1.forward(&mut session).await.unwrap();
    wf2.forward(&mut session).await.unwrap();

    assert_eq!(wf1.count(), 1);
    assert_eq!(wf2.count(), 1);
    assert_eq!(session.message_count(), 2);
}

#[test]
fn test_runtime_session_fork() {
    let original = RuntimeSession::new("orig".into(), SessionMode::Pentest)
        .with_user_message("hello");
    let forked = original.fork();

    assert_ne!(original.id, forked.id);
    assert_eq!(forked.lineage.parent_id, Some("orig".into()));
    assert_eq!(forked.message_count(), original.message_count());
}

#[test]
fn test_runtime_session_detach() {
    let mut session = RuntimeSession::new("child".into(), SessionMode::Pentest);
    session.lineage.parent_id = Some("parent".into());
    session.detach();
    assert!(session.lineage.parent_id.is_none());
}
