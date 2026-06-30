use holmes_core::event::Event;
use holmes_core::types::*;
use holmes_session::db::*;
use holmes_session::SessionStore;

#[tokio::test]
async fn test_full_session_lifecycle() {
    let db = SessionDB::open(":memory:").await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: None,
            title: Some("integration test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    let event = Event::UserMessage {
        content: "test message".into(),
        timestamp: chrono::Utc::now(),
    };
    db.append_event(&session.id, &event).await.unwrap();

    let events = db.get_events(&session.id).await.unwrap();
    assert_eq!(events.len(), 1);

    db.end_session(&session.id, EndReason::UserQuit)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_automatic_archiving_and_bypass() {
    let temp_dir = std::env::temp_dir().join(format!("holmes_test_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("holmes_test.db");
    
    let db = SessionDB::open(&db_path).await.unwrap();

    let session = db
        .create_session(CreateSessionParams {
            id: Some("test_session_123".into()),
            title: Some("bypass test".into()),
            mode: Some(SessionMode::Pentest),
            model: None,
            system_prompt: None,
            parent_session_id: None,
            fork_point: None,
            source: Some("test".into()),
            tags: vec![],
        })
        .await
        .unwrap();

    // 1. 验证会话目录及其子目录是否已经创建
    let sessions_dir = db_path.parent().unwrap().join("sessions");
    let session_workspace = sessions_dir.join("test_session_123");
    let tool_results_dir = session_workspace.join("tool-results");
    assert!(session_workspace.exists());
    assert!(tool_results_dir.exists());

    // 2. 创建一个超大 ToolResult 事件 (>10KB)
    let large_content = "A".repeat(15000); // 15KB
    let event = Event::ToolResult {
        name: "test_tool".into(),
        success: true,
        content: large_content.clone(),
        error: None,
        artifacts: vec![],
    };

    // 写入事件
    db.append_event(&session.id, &event).await.unwrap();

    // 3. 验证本地磁盘上是否生成了对应的 call_xxx.txt 文件
    let mut files = std::fs::read_dir(&tool_results_dir).unwrap();
    let entry = files.next().unwrap().unwrap();
    let txt_path = entry.path();
    assert!(txt_path.is_file());
    assert_eq!(txt_path.extension().unwrap(), "txt");
    
    // 验证 txt 文件内容
    let file_content = std::fs::read_to_string(&txt_path).unwrap();
    assert_eq!(file_content, large_content);

    // 4. 验证 transcript.jsonl 是否同步追加，且包含了旁路引用的 event_data
    let jsonl_path = session_workspace.join("transcript.jsonl");
    assert!(jsonl_path.exists());
    let jsonl_content = std::fs::read_to_string(&jsonl_path).unwrap();
    assert!(jsonl_content.contains("__BYPASS_FILE__:file://"));

    // 5. 验证读取 get_events 时，内容是否被透明还原
    let events = db.get_events(&session.id).await.unwrap();
    assert_eq!(events.len(), 1);
    if let Event::ToolResult { content, .. } = &events[0].event {
        assert_eq!(content.as_str(), large_content.as_str());
    } else {
        panic!("Event type mismatch");
    }

    // 清理临时文件
    std::fs::remove_dir_all(temp_dir).ok();
}
