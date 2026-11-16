//! Recorded runs: actions, XCTest, evidence files and hashes, downloads, quota, retention,
//! native device operations.

mod common;

use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use common::*;

/// Acquires an iOS device and installs the demo app; returns (token, client, lease id).
async fn ready_device(
    env: &Env,
    user: &str,
) -> (String, rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>, String) {
    let token = add_user(env, user, QUOTA);
    let c = client(env, &token).await;
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let app = upload(env, &token, "Demo.zip", "ZIP").await;
    let installed = ok_json(&call(&c, "install_app", json!({"app_id": app["app_id"]})).await);
    assert_eq!(installed["bundle_id"], "com.example.demo");
    (token, c, lease["lease_id"].as_str().unwrap().to_string())
}

fn files(manifest: &Value) -> Vec<String> {
    manifest["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn run_actions_records_every_step_with_video_logs_crashes_and_hashes() {
    let env = setup(2).await;
    let (token, c, lease_id) = ready_device(&env, "alice").await;

    let run = call(
        &c,
        "run_actions",
        json!({
            "name": "Login with a valid account",
            "tags": ["smoke", "PR-7"],
            "actions": [
                {"launch_app": {}},
                {"tap": {"label": "Sign In"}},
                {"type": {"text": "ana@example.com", "submit": true}},
                {"type": {"text": "hunter2", "secret": true}},
                {"wait_for": {"contains": "welcome", "timeout_seconds": 2}},
                {"assert_visible": {"id": "signin", "element_type": "Button"}},
                {"screenshot": {"name": "home"}}
            ]
        }),
    )
    .await;
    let summary = ok_json(&run);
    assert_eq!(summary["status"], "passed", "{summary}");
    assert_eq!(summary["verdict"], "7 of 7 steps passed");
    assert!(summary["video"].as_bool().unwrap());
    assert_eq!(summary["crash_reports"], 1);
    let run_id = summary["run_id"].as_str().unwrap().to_string();

    // The device saw the steps in order; the default app and the typed text came through.
    assert_eq!(
        env.fake.log(),
        ["op launch_app com.example.demo", "tap label=Sign In", "type ana@example.com", "key 40", "type hunter2"]
    );
    assert_eq!(env.fake.stopped_captures.load(Ordering::SeqCst), 2, "video and log capture both stopped");

    let manifest = ok_json(&call(&c, "get_run", json!({"run_id": run_id})).await);
    assert_eq!(manifest["schema"], "seiji.run.v1");
    assert_eq!(manifest["lease_id"], lease_id);
    assert_eq!(manifest["app"]["executable"], "Demo");
    assert_eq!(manifest["device"]["runtime"], "iOS-18");
    assert!(!manifest.to_string().contains("hunter2"), "secret text stays out of the manifest");
    assert_eq!(manifest["steps"][3]["action"]["type"]["text"], "<redacted>");
    let paths = files(&manifest);
    for expected in [
        "video.mp4",
        "app.log",
        "crashes/Demo-2026-11-16.ips",
        "steps/000-launch_app.png",
        "steps/006-screenshot-home.png",
    ] {
        assert!(paths.contains(&expected.to_string()), "{expected} missing from {paths:?}");
    }
    assert!(!paths.iter().any(|p| p.contains(".started")), "{paths:?}");
    let log = text(&call(&c, "get_run_artifact", json!({"run_id": run_id, "path": "app.log"})).await);
    assert!(log.contains(r#"process == "Demo" OR subsystem BEGINSWITH "com.example.demo""#), "{log}");

    // Every listed file matches its SHA-256 and is downloadable by its owner only.
    let http = reqwest::Client::new();
    for f in manifest["files"].as_array().unwrap() {
        let url = format!("{}/v1/runs/{run_id}/files/{}", env.url, f["path"].as_str().unwrap());
        let body = http.get(&url).bearer_auth(&token).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&body)), f["sha256"], "{url}");
    }
    let image = call(&c, "get_run_artifact", json!({"run_id": run_id, "path": "steps/006-screenshot-home.png"})).await;
    assert!(has_image(&image), "{}", text(&image));
    let missing = call(&c, "get_run_artifact", json!({"run_id": run_id, "path": "../../seiji.db"})).await;
    assert!(is_err(&missing));

    let zip = http.get(format!("{}/v1/runs/{run_id}/evidence.zip", env.url)).bearer_auth(&token).send().await.unwrap();
    assert_eq!(zip.status(), 200);
    assert!(zip.bytes().await.unwrap().starts_with(b"PK"));

    let bob = add_user(&env, "bob", QUOTA);
    let other = http.get(format!("{}/v1/runs/{run_id}/evidence.zip", env.url)).bearer_auth(&bob).send().await.unwrap();
    assert_eq!(other.status(), 404);
    let bob_c = client(&env, &bob).await;
    assert!(is_err(&call(&bob_c, "get_run", json!({"run_id": run_id})).await));

    let md = text(&call(&c, "export_evidence", json!({"run_id": run_id})).await);
    assert!(md.starts_with("### ✅ Login with a valid account"), "{md}");
    assert!(md.contains("evidence.zip") && md.contains("manifest SHA-256"), "{md}");
    assert!(md.contains("[video.mp4]"), "{md}");

    let listed = ok_json(&call(&c, "list_runs", json!({"tag": "PR-7"})).await);
    assert_eq!(listed[0]["run_id"], run_id);
    assert!(ok_json(&call(&c, "list_runs", json!({"tag": "nope"})).await).as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_step_stops_the_run_and_keeps_the_screen_and_tree() {
    let env = setup(2).await;
    let (_, c, _) = ready_device(&env, "alice").await;
    let run = call(
        &c,
        "run_actions",
        json!({"record_video": false, "screenshot_each_step": false, "actions": [
            {"tap": {"label": "Sign In"}},
            {"tap": {"label": "Missing"}},
            {"screenshot": {"name": "never"}}
        ]}),
    )
    .await;
    assert!(is_err(&run));
    assert!(has_image(&run), "a failed run attaches the screen");
    let summary = any_json(&run);
    assert_eq!(summary["status"], "failed");
    assert!(
        summary["verdict"].as_str().unwrap().starts_with("step 2 (tap) failed: tap failed (elementNotFound)"),
        "{summary}"
    );
    let steps: Vec<&str> = summary["steps"].as_array().unwrap().iter().map(|s| s["status"].as_str().unwrap()).collect();
    assert_eq!(steps, ["passed", "failed", "skipped"]);
    assert_eq!(summary["video"], false);

    let manifest = ok_json(&call(&c, "get_run", json!({"run_id": summary["run_id"]})).await);
    let paths = files(&manifest);
    assert!(paths.contains(&"steps/001-tap-failure.png".to_string()), "{paths:?}");
    assert!(!paths.iter().any(|p| p.starts_with("steps/000")), "no screenshot for a passed step: {paths:?}");
    let tree = text(
        &call(&c, "get_run_artifact", json!({"run_id": summary["run_id"], "path": "steps/001-tap-tree.txt"})).await,
    );
    assert!(tree.contains("Welcome, Ana!"), "{tree}");
}

#[tokio::test]
async fn invalid_actions_are_refused_before_anything_runs() {
    let env = setup(2).await;
    let (_, c, _) = ready_device(&env, "alice").await;
    let bad =
        call(&c, "run_actions", json!({"actions": [{"tap": {"label": "A"}}, {"screenshot": {"name": "../x"}}]})).await;
    assert!(is_err(&bad) && text(&bad).contains("action 2 (screenshot)"), "{}", text(&bad));
    assert!(env.fake.log().is_empty());
    assert!(ok_json(&call(&c, "list_runs", json!({})).await).as_array().unwrap().is_empty());
}

#[tokio::test]
async fn xctest_runs_keep_the_result_bundle_and_per_test_results() {
    let env = setup(2).await;
    let (token, c, _) = ready_device(&env, "alice").await;
    let bundle = upload(&env, &token, "DemoUITests.zip", "PK").await;
    let run = call(
        &c,
        "run_xctest",
        json!({"app_id": bundle["app_id"], "only_testing": ["DemoUITests/LoginTests"], "tags": ["nightly"]}),
    )
    .await;
    assert!(is_err(&run), "a failing test fails the run");
    let summary = any_json(&run);
    assert_eq!(summary["status"], "failed");
    assert_eq!(summary["verdict"], "2 tests: 1 passed, 1 failed, 0 skipped");
    assert_eq!(env.fake.log(), [r#"xctest only=["DemoUITests/LoginTests"]"#]);

    let manifest = ok_json(&call(&c, "get_run", json!({"run_id": summary["run_id"]})).await);
    assert_eq!(manifest["steps"][1]["name"], "DemoUITests/LoginTests/testLogout()");
    assert_eq!(manifest["steps"][1]["error"], "XCTAssertTrue failed");
    let paths = files(&manifest);
    for expected in ["xctest/result.xcresult.zip", "xctest/attachments/Login.png", "xctest/xcodebuild.log", "video.mp4"]
    {
        assert!(paths.contains(&expected.to_string()), "{expected} missing from {paths:?}");
    }

    let bad = call(&c, "run_xctest", json!({"app_id": bundle["app_id"], "only_testing": ["A;rm -rf /"]})).await;
    assert!(is_err(&bad) && text(&bad).contains("test identifier"), "{}", text(&bad));
}

#[tokio::test]
async fn evidence_quota_refuses_new_runs_and_retention_frees_it() {
    // The manifest alone outgrows 600 bytes, so one run fills the quota.
    let env = setup_with(2, json!({"evidence": {"maxBytesPerUser": 600, "retentionHours": 1}})).await;
    let (_, c, _) = ready_device(&env, "alice").await;
    let first = ok_json(&call(&c, "run_actions", json!({"actions": [{"screenshot": {"name": "a"}}]})).await);
    let second = call(&c, "run_actions", json!({"actions": [{"screenshot": {"name": "b"}}]})).await;
    assert!(is_err(&second) && text(&second).contains("evidence quota full"), "{}", text(&second));

    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let dir = seiji::evidence::run_dir(&env.pool.config, user.id, first["run_id"].as_str().unwrap());
    assert!(dir.exists());
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        conn.execute("UPDATE runs SET ended_at = ?1", [seiji::store::now() - 3601]).unwrap();
    }
    env.pool.reap().await.unwrap();
    assert!(!dir.exists());
    assert!(is_err(&call(&c, "get_run", json!({"run_id": first["run_id"]})).await));
    let third = call(&c, "run_actions", json!({"actions": [{"screenshot": {"name": "c"}}]})).await;
    assert!(!is_err(&third), "{}", text(&third));
}

#[tokio::test]
async fn native_device_operations_default_to_the_installed_app() {
    let env = setup(2).await;
    let (_, c, _) = ready_device(&env, "alice").await;

    let configured = call(
        &c,
        "configure_device",
        json!({
            "status_bar": {"time": "9:41", "battery_level": 100, "battery_state": "charged"},
            "appearance": "dark",
            "location": {"latitude": -23.55, "longitude": -46.63},
            "permissions": [{"action": "grant", "service": "photos"}]
        }),
    )
    .await;
    assert_eq!(text(&configured), "applied: status_bar, appearance, location, permissions");
    let pushed = call(&c, "push_notification", json!({"payload": {"aps": {"alert": "Hi"}}})).await;
    assert!(!is_err(&pushed), "{}", text(&pushed));
    assert!(!is_err(&call(&c, "terminate_app", json!({})).await));
    let log = env.fake.log();
    assert!(log.contains(&"op permissions grant photos com.example.demo".to_string()), "{log:?}");
    assert!(log.contains(&r#"op push_notification com.example.demo {"aps":{"alert":"Hi"}}"#.to_string()), "{log:?}");
    assert!(log.contains(&"op terminate_app com.example.demo".to_string()), "{log:?}");

    let logs = text(&call(&c, "get_logs", json!({"last_seconds": 60})).await);
    assert!(logs.contains(r#"process == "Demo""#), "{logs}");

    let bad = call(&c, "configure_device", json!({"location": {"latitude": 91, "longitude": 0}})).await;
    assert!(is_err(&bad) && text(&bad).contains("latitude"), "{}", text(&bad));
    let big = call(&c, "push_notification", json!({"payload": {"aps": {"alert": "x".repeat(5000)}}})).await;
    assert!(is_err(&big) && text(&big).contains("APNs allows 4096"), "{}", text(&big));

    assert!(!is_err(&call(&c, "uninstall_app", json!({})).await));
    let after = call(&c, "launch_app", json!({})).await;
    assert!(is_err(&after) && text(&after).contains("no app installed"), "{}", text(&after));
}

#[tokio::test]
async fn a_run_cut_by_a_restart_stays_readable_and_counts_against_the_quota() {
    let env = setup(2).await;
    let (_, c, lease_id) = ready_device(&env, "alice").await;
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let run = seiji::store::RunRow {
        id: "r_interrupted0".into(),
        user_id: user.id,
        lease_id,
        kind: "actions".into(),
        name: Some("half done".into()),
        tags: vec!["nightly".into()],
        status: "running".into(),
        started_at: seiji::store::now(),
        ended_at: None,
        bytes: 0,
        summary: None,
    };
    env.pool.store.insert_run(&run).unwrap();
    let dir = seiji::evidence::run_dir(&env.pool.config, user.id, &run.id);
    std::fs::create_dir_all(dir.join("steps")).unwrap();
    std::fs::write(dir.join("steps/000-tap.png"), PNG).unwrap();

    env.pool.recover().await.unwrap();

    let manifest = ok_json(&call(&c, "get_run", json!({"run_id": run.id})).await);
    assert_eq!(manifest["status"], "error");
    assert_eq!(manifest["verdict"], "server restarted during the run");
    assert_eq!(files(&manifest), ["steps/000-tap.png"]);
    let listed = ok_json(&call(&c, "list_runs", json!({"status": "error"})).await);
    assert!(listed[0]["bytes"].as_i64().unwrap() > PNG.len() as i64, "{listed}");
    let bad = call(&c, "list_runs", json!({"status": "weird"})).await;
    assert!(is_err(&bad));
}
