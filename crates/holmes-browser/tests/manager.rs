use holmes_browser::BrowserManager;
use holmes_core::config::BrowserConfig;

fn enabled_config() -> BrowserConfig {
    BrowserConfig {
        enabled: true,
        headless: false,
        vision: false,
        content_limit: 2000,
        timeout: 30,
        proxy: None,
        ignore_https_errors: true,
        executable_path: None,
        extra_launch_args: vec![],
        screenshot_dir: None,
    }
}

#[tokio::test]
#[ignore = "launches real Chromium; run with: cargo test -p holmes-browser -- --ignored"]
async fn manager_navigates_and_returns_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("it-session", tmp.path(), enabled_config()).unwrap();
    assert!(!mgr.is_launched().await);

    let snap = mgr
        .navigate("data:text/html,<html><head><title>T</title></head><body><p id=x>hello</p></body></html>")
        .await
        .expect("navigate");
    assert!(snap.text_excerpt.contains("hello"), "snapshot: {snap:?}");
    assert!(mgr.is_launched().await);

    mgr.close().await;
    assert!(!mgr.is_launched().await);
}

#[tokio::test]
#[ignore = "launches real Chromium; run with: cargo test -p holmes-browser -- --ignored"]
async fn manager_resume_reopens_same_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("resume-session", tmp.path(), enabled_config()).unwrap();
    mgr.navigate("data:text/html,<body><p>first</p></body>")
        .await
        .unwrap();
    mgr.close().await;

    // Re-open on the SAME profile dir (same session id) — must relaunch fine.
    let mgr2 = BrowserManager::new("resume-session", tmp.path(), enabled_config()).unwrap();
    mgr2
        .navigate("data:text/html,<body><p>second</p></body>")
        .await
        .expect("re-navigate");
    mgr2.close().await;
}
