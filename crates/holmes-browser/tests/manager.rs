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

#[tokio::test]
#[ignore = "launches real Chromium; run with: cargo test -p holmes-browser -- --ignored"]
async fn fill_and_get_content_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("fill-session", tmp.path(), enabled_config()).unwrap();
    mgr.navigate("data:text/html,<body><input id=q><p>ok</p></body>")
        .await
        .unwrap();
    mgr.fill("#q", "search-term").await.expect("fill");
    // input.value is not part of innerText; read it back via JS to confirm fill worked.
    let val = mgr
        .execute_js("document.querySelector('#q').value")
        .await
        .expect("get value");
    assert_eq!(val, serde_json::json!("search-term"), "input value: {val}");
    // get_content on body text still works (reads innerText, not input values).
    let body = mgr.get_content(None).await.expect("get body");
    assert!(body.contains("ok"), "body: {body}");
    mgr.close().await;
}

#[tokio::test]
#[ignore = "launches real Chromium; run with: cargo test -p holmes-browser -- --ignored"]
async fn execute_js_returns_value() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = BrowserManager::new("js-session", tmp.path(), enabled_config()).unwrap();
    mgr.navigate("data:text/html,<body></body>").await.unwrap();
    let val = mgr.execute_js("1 + 2").await.expect("js");
    assert_eq!(val, serde_json::json!(3));
    mgr.close().await;
}

#[tokio::test]
#[ignore = "launches real Chromium; run with: cargo test -p holmes-browser -- --ignored"]
async fn screenshot_writes_png_file() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = enabled_config();
    cfg.screenshot_dir = Some(tmp.path().join("shots").to_string_lossy().to_string());
    let mgr = BrowserManager::new("shot-session", tmp.path(), cfg).unwrap();
    mgr.navigate("data:text/html,<body><p>pic</p></body>")
        .await
        .unwrap();
    let shot = mgr.screenshot(false).await.expect("screenshot");
    assert!(shot.path.exists(), "screenshot file: {}", shot.path.display());
    assert!(shot.path.extension().and_then(|e| e.to_str()) == Some("png"));
    mgr.close().await;
}
