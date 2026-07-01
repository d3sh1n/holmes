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
        cdp_endpoint: None,
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

#[tokio::test]
#[ignore = "real network + Chromium; run with: cargo test -p holmes-browser xiaohongshu -- --ignored"]
async fn xiaohongshu_smoke() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = enabled_config();
    cfg.timeout = 60; // real site may be slow (redirects, JS)
    cfg.screenshot_dir = Some(tmp.path().join("shots").to_string_lossy().to_string());
    let mgr = BrowserManager::new("xhs-smoke", tmp.path(), cfg).unwrap();

    let snap = mgr
        .navigate("https://www.xiaohongshu.com")
        .await
        .expect("navigate should not time out / error");
    println!("XHS url after navigate: {}", snap.url);
    println!("XHS title: {}", snap.title);
    println!("XHS body excerpt (first 400 chars):");
    let head: String = snap.text_excerpt.chars().take(400).collect();
    println!("{head}");

    let shot = mgr.screenshot(true).await.expect("screenshot");
    println!("XHS screenshot saved: {}", shot.path.display());
    assert!(shot.path.exists(), "screenshot file missing");

    let body = mgr.get_content(None).await.expect("get_content");
    println!("XHS get_content length: {}", body.len());

    assert!(mgr.is_launched().await);
    assert!(
        !body.trim().is_empty(),
        "page body should not be empty after navigate"
    );
    mgr.close().await;
}

#[tokio::test]
#[ignore = "real network; confirms browser works against a normal HTTPS site"]
async fn real_site_example_com_works() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = enabled_config();
    let mgr = BrowserManager::new("example-com", tmp.path(), cfg).unwrap();
    let snap = mgr.navigate("https://example.com").await.expect("navigate");
    println!("url: {} | title: {}", snap.url, snap.title);
    assert!(snap.text_excerpt.contains("Example Domain"), "body: {}", snap.text_excerpt);
    mgr.close().await;
}

#[tokio::test]
#[ignore = "launches real Chromium; verifies stealth hides the webdriver flag"]
async fn stealth_hides_webdriver_flag() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = enabled_config();
    let mgr = BrowserManager::new("stealth", tmp.path(), cfg).unwrap();
    mgr.navigate("data:text/html,<body>x</body>").await.unwrap();
    let val = mgr.execute_js("navigator.webdriver").await.unwrap();
    // After stealth patch, navigator.webdriver must be masked (null/undefined).
    assert!(
        val.is_null(),
        "navigator.webdriver should be masked (null), got {val}"
    );
    mgr.close().await;
}

#[tokio::test]
#[ignore = "requires a real Chrome running with --remote-debugging-port=9222; attach mode"]
async fn attach_to_real_chrome_navigates() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = enabled_config();
    cfg.cdp_endpoint = Some("http://127.0.0.1:9222".to_string());
    cfg.timeout = 60;
    let mgr = BrowserManager::new("attach-test", tmp.path(), cfg).unwrap();
    // about:blank navigation proves the attach + new_page path works.
    let snap = mgr.navigate("https://example.com").await.expect("attach navigate");
    println!("attached url: {} | title: {}", snap.url, snap.title);
    assert!(snap.text_excerpt.contains("Example Domain"));
    // close() in attach mode must NOT kill the user's Chrome; verify the
    // process is still listening afterward.
    mgr.close().await;
    // The fact that we got here without panicking + the browser is still up
    // (next test run can re-attach) is the contract.
}

#[tokio::test]
#[ignore = "real network; launch with system Chrome binary to bypass anti-bot"]
async fn xiaohongshu_with_system_chrome() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = enabled_config();
    cfg.timeout = 60;
    cfg.executable_path =
        Some("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    let mgr = BrowserManager::new("xhs-chrome", tmp.path(), cfg).unwrap();

    // First, diagnose the UA the launched browser reports.
    let snap = mgr.navigate("data:text/html,<body>x</body>").await.unwrap();
    let ua = mgr
        .execute_js("navigator.userAgent")
        .await
        .unwrap_or(serde_json::Value::Null);
    println!("UA reported: {}", ua);
    println!("title: {}", snap.title);

    // Now hit the real anti-bot target.
    let snap = match mgr.navigate("https://www.xiaohongshu.com").await {
        Ok(s) => s,
        Err(e) => {
            println!("xiaohongshu navigate error: {e}");
            panic!("still blocked: {e}");
        }
    };
    println!("XHS url: {} | title: {}", snap.url, snap.title);
    let head: String = snap.text_excerpt.chars().take(300).collect();
    println!("XHS body excerpt: {head}");
    mgr.close().await;
}

#[tokio::test]
#[ignore = "real network; verifies auto-detected system Chrome bypasses xiaohongshu anti-bot without explicit executable_path"]
async fn xiaohongshu_auto_detected_chrome() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = enabled_config();
    cfg.timeout = 60;
    // intentionally no executable_path — should auto-detect system Chrome
    let mgr = BrowserManager::new("xhs-auto", tmp.path(), cfg).unwrap();
    let snap = mgr.navigate("https://www.xiaohongshu.com").await.expect("navigate");
    println!("XHS auto url: {} | title: {}", snap.url, snap.title);
    assert!(!snap.title.is_empty());
    assert!(!snap.text_excerpt.is_empty());
    mgr.close().await;
}
