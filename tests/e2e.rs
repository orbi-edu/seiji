//! End-to-end over real HTTP + MCP with the device layer faked (no simulators in CI) and a stub
//! `maestro` script. Exercises quotas, capacity, lease ownership, billing and the reaper.

mod common;

use std::sync::atomic::Ordering;

use serde_json::{Value, json};

use common::*;
use seiji::config::Platform;
use seiji::store::{Lease, LeaseStatus, now};

#[tokio::test]
async fn rejects_missing_and_wrong_tokens() {
    let env = setup(2).await;
    let http = reqwest::Client::new();
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}});
    let no_token = http.post(format!("{}/mcp", env.url)).json(&body).send().await.unwrap();
    assert_eq!(no_token.status(), 401);
    let wrong = http.post(format!("{}/mcp", env.url)).bearer_auth("sj_nope").json(&body).send().await.unwrap();
    assert_eq!(wrong.status(), 401);
}

#[tokio::test]
async fn full_session_acquire_upload_install_flow_release() {
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let c = client(&env, &token).await;

    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    assert_eq!(lease["status"], "ready");
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();

    // Upload over HTTP, install through MCP.
    let up: Value = reqwest::Client::new()
        .post(format!("{}/v1/apps", env.url))
        .bearer_auth(&token)
        .header("X-Filename", "My App.zip")
        .body("ZIPBYTES")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let app_id = up["app_id"].as_str().unwrap();
    let installed = call(&c, "install_app", json!({"app_id": app_id})).await;
    assert!(!is_err(&installed), "{}", text(&installed));
    assert_eq!(env.fake.installed.lock().unwrap().as_slice(), ["My_App.zip:ZIPBYTES"]);

    // Flow runs through the Maestro CLI pinned to this lease's device and driver port.
    let flow =
        call(&c, "run_flow", json!({"yaml": "appId: com.example\n---\n- launchApp", "env": {"USER_NAME": "bob"}}))
            .await;
    let run = ok_json(&flow);
    assert_eq!(run["status"], "passed", "{run}");
    let output = call(&c, "get_run_artifact", json!({"run_id": run["run_id"], "path": "maestro/output.txt"})).await;
    let out = text(&output);
    assert!(out.contains(&format!("--device FAKE-{lease_id}")), "{out}");
    assert!(out.contains("--driver-host-port"), "{out}");
    assert!(out.contains("-e USER_NAME=bob"), "{out}");
    let manifest = ok_json(&call(&c, "get_run", json!({"run_id": run["run_id"]})).await);
    assert_eq!(manifest["input"]["env_keys"], json!(["USER_NAME"]), "env values stay out of the evidence");
    assert!(!manifest.to_string().contains("\"bob\""), "{manifest}");

    // A failing flow is an error result with a screenshot attached.
    let failed = call(&c, "run_flow", json!({"yaml": "appId: x\n---\n- assertVisible: FAILME"})).await;
    assert!(is_err(&failed));
    assert_eq!(any_json(&failed)["status"], "failed");
    assert!(any_json(&failed)["verdict"].as_str().unwrap().contains("exit 1"), "{}", text(&failed));
    assert!(has_image(&failed));

    let tree = call(&c, "inspect_screen", json!({})).await;
    assert!(text(&tree).contains("Button label=\"Sign In\" id=\"signin\""), "{}", text(&tree));

    let released = call(&c, "release_device", json!({"lease_id": lease_id})).await;
    assert!(!is_err(&released), "{}", text(&released));
    assert_eq!(env.fake.destroyed.lock().unwrap().as_slice(), std::slice::from_ref(&lease_id));

    let again = call(&c, "take_screenshot", json!({})).await;
    assert!(text(&again).contains("no device"), "{}", text(&again));
}

#[tokio::test]
async fn enforces_concurrency_capacity_and_ownership() {
    let env = setup(2).await;
    let alice = client(&env, &add_user(&env, "alice", QUOTA)).await;
    let bob = client(&env, &add_user(&env, "bob", QUOTA)).await;
    let carol = client(&env, &add_user(&env, "carol", QUOTA)).await;

    let a = ok_json(&call(&alice, "acquire_device", json!({"platform": "ios"})).await);
    let second = call(&alice, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&second) && text(&second).contains("your limit is 1"), "{}", text(&second));

    ok_json(&call(&bob, "acquire_device", json!({"platform": "ios"})).await);
    let full = call(&carol, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&full) && text(&full).contains("host is full: 2/2"), "{}", text(&full));

    let android = call(&carol, "acquire_device", json!({"platform": "android"})).await;
    assert!(is_err(&android) && text(&android).contains("not available on this host"), "{}", text(&android));

    // Bob cannot touch Alice's device.
    let steal = call(&bob, "release_device", json!({"lease_id": a["lease_id"]})).await;
    assert!(is_err(&steal) && text(&steal).contains("no active lease"), "{}", text(&steal));
    let peek = call(&bob, "take_screenshot", json!({"lease_id": a["lease_id"]})).await;
    assert!(is_err(&peek));
    assert!(env.fake.destroyed.lock().unwrap().is_empty());

    // Fourth seat does not exist.
    let err = env.pool.store.add_user("dave", None, QUOTA, env.pool.config.seats).unwrap_err();
    assert!(err.to_string().contains("all 3 seats are taken"));
}

#[tokio::test]
async fn monthly_budget_blocks_new_devices_and_reaper_reclaims() {
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let c = client(&env, &token).await;

    // 50 of 60 minutes already used this month.
    let t = now();
    let past = Lease {
        id: "l_past".into(),
        user_id: user.id,
        platform: Platform::Ios,
        model: "iphone-16".into(),
        device_id: Some("X".into()),
        driver_port: 1,
        console_port: None,
        status: LeaseStatus::Booting,
        started_at: t - 50 * 60 - 5,
        last_activity_at: t,
        ended_at: None,
        end_reason: None,
    };
    env.pool.store.insert_lease(&past).unwrap();
    env.pool.store.end_lease("l_past", "released", t - 5, true).unwrap();

    let status = ok_json(&call(&c, "budget_status", json!({})).await);
    assert_eq!(status["used_minutes"], 50.0);
    assert_eq!(status["remaining_minutes"], 10.0);
    assert_eq!(status["estimated_cost_usd"], 1.25);

    // A running lease that started 11 minutes ago crosses the budget: the reaper releases it.
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let id = lease["lease_id"].as_str().unwrap();
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        conn.execute("UPDATE leases SET started_at = ?1 WHERE id = ?2", rusqlite::params![t - 11 * 60, id]).unwrap();
    }
    env.pool.reap().await.unwrap();
    let ended = env.pool.store.lease(id).unwrap().unwrap();
    assert_eq!(ended.end_reason.as_deref(), Some("budget_exhausted"));
    assert_eq!(env.fake.destroyed.lock().unwrap().as_slice(), [id.to_string()]);

    let blocked = call(&c, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&blocked) && text(&blocked).contains("monthly budget exhausted"), "{}", text(&blocked));
}

#[tokio::test]
async fn idle_leases_are_released_and_failed_boots_are_free() {
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let c = client(&env, &token).await;

    env.fake.fail_boot.store(true, Ordering::SeqCst);
    let failed = call(&c, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&failed) && text(&failed).contains("not billed"), "{}", text(&failed));
    assert_eq!(env.fake.destroyed.lock().unwrap().len(), 1);
    env.fake.fail_boot.store(false, Ordering::SeqCst);

    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let id = lease["lease_id"].as_str().unwrap();
    let t = now();
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        // Booted 15 min ago, last tool call 11 min ago (idle limit 10).
        conn.execute(
            "UPDATE leases SET started_at = ?1, last_activity_at = ?2 WHERE id = ?3",
            rusqlite::params![t - 15 * 60, t - 11 * 60, id],
        )
        .unwrap();
    }
    env.pool.reap().await.unwrap();
    assert_eq!(env.pool.store.lease(id).unwrap().unwrap().end_reason.as_deref(), Some("idle"));

    // Only the 15 minutes of the real lease are billed; the failed boot costs nothing.
    let (from, to) = seiji::store::month_window(t);
    let used = env.pool.store.used_seconds(user.id, from, to, now()).unwrap();
    assert!((15 * 60..15 * 60 + 5).contains(&used), "{used}");
}

#[tokio::test]
async fn expired_builds_are_deleted() {
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let c = client(&env, &token).await;
    let up: Value = reqwest::Client::new()
        .post(format!("{}/v1/apps", env.url))
        .bearer_auth(&token)
        .header("X-Filename", "app.zip")
        .body("ZIP")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let app_id = up["app_id"].as_str().unwrap();
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let dir = env.pool.apps_dir(user.id, app_id);
    assert!(dir.exists());
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        let old = now() - env.pool.config.app_retention_hours * 3600 - 1;
        conn.execute("UPDATE apps SET created_at = ?1 WHERE id = ?2", rusqlite::params![old, app_id]).unwrap();
    }
    env.pool.reap().await.unwrap();
    assert!(!dir.exists());
    ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let install = call(&c, "install_app", json!({"app_id": app_id})).await;
    assert!(is_err(&install) && text(&install).contains("no uploaded app"), "{}", text(&install));
}

#[tokio::test]
async fn uploads_respect_size_and_count_limits() {
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let upload = |body: &'static str| {
        reqwest::Client::new()
            .post(format!("{}/v1/apps", env.url))
            .bearer_auth(&token)
            .header("X-Filename", "app.apk")
            .body(body)
            .send()
    };
    let too_big = upload("0123456789abcdefX").await.unwrap();
    assert_eq!(too_big.status(), 413);
    assert!(!env.pool.config.data_dir.join("apps").join(user.id.to_string()).read_dir().unwrap().any(|_| true));
    assert_eq!(upload("").await.unwrap().status(), 400);
    assert_eq!(upload("one").await.unwrap().status(), 200);
    assert_eq!(upload("two").await.unwrap().status(), 200);
    let third = upload("three").await.unwrap();
    assert_eq!(third.status(), 429);
    assert!(third.text().await.unwrap().contains("the limit is 2"));
}

#[tokio::test]
async fn deleting_a_build_frees_its_place() {
    let env = setup(2).await;
    let alice = add_user(&env, "alice", QUOTA);
    let bob = add_user(&env, "bob", QUOTA);
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let http = reqwest::Client::new();
    let upload = |body: &'static str| {
        http.post(format!("{}/v1/apps", env.url)).bearer_auth(&alice).header("X-Filename", "app.apk").body(body).send()
    };
    let delete = |token: &str, app_id: &str| {
        http.delete(format!("{}/v1/apps/{app_id}", env.url)).bearer_auth(token.to_string()).send()
    };
    let first: Value = upload("one").await.unwrap().json().await.unwrap();
    let app_id = first["app_id"].as_str().unwrap();
    assert_eq!(upload("two").await.unwrap().status(), 200);
    assert_eq!(upload("three").await.unwrap().status(), 429);

    let foreign = delete(&bob, app_id).await.unwrap();
    assert_eq!(foreign.status(), 404);
    assert_eq!(foreign.json::<Value>().await.unwrap()["error"], format!("no uploaded app \"{app_id}\""));
    let dir = env.pool.apps_dir(user.id, app_id);
    assert!(dir.exists());

    assert_eq!(delete(&alice, app_id).await.unwrap().status(), 204);
    assert!(!dir.exists());
    assert!(env.pool.store.app(app_id, user.id).unwrap().is_none());
    assert_eq!(upload("three").await.unwrap().status(), 200);

    let again = delete(&alice, app_id).await.unwrap();
    assert_eq!(again.status(), 404);
    assert_eq!(again.json::<Value>().await.unwrap()["error"], format!("no uploaded app \"{app_id}\""));
    assert_eq!(delete("sj_invalid", app_id).await.unwrap().status(), 401);
}

#[tokio::test]
async fn a_user_build_limit_overrides_the_default() {
    let env = setup_with(2, json!({ "maxBuildsPerUser": 10 })).await;
    let service = add_user(&env, "alice", seiji::config::Quota { max_builds: Some(3), ..QUOTA });
    let person = add_user(&env, "bob", QUOTA);
    let http = reqwest::Client::new();
    let upload = |token: &str| {
        http.post(format!("{}/v1/apps", env.url))
            .bearer_auth(token.to_string())
            .header("X-Filename", "app.apk")
            .body("x")
            .send()
    };
    let me = |token: &str| {
        let req = http.get(format!("{}/v1/me", env.url)).bearer_auth(token.to_string());
        async move { req.send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let full =
        |n: i64, limit: i64| format!("you already keep {n} builds, the limit is {limit}; they expire 72h after upload");

    for _ in 0..3 {
        assert_eq!(upload(&service).await.unwrap().status(), 200);
    }
    let fourth = upload(&service).await.unwrap();
    assert_eq!(fourth.status(), 429);
    assert_eq!(fourth.json::<Value>().await.unwrap()["error"], full(3, 3));
    for _ in 0..10 {
        assert_eq!(upload(&person).await.unwrap().status(), 200);
    }
    let eleventh = upload(&person).await.unwrap();
    assert_eq!(eleventh.status(), 429);
    assert_eq!(eleventh.json::<Value>().await.unwrap()["error"], full(10, 10));

    let alice = me(&service).await;
    assert_eq!(alice["quota"]["maxBuilds"], 3);
    assert_eq!(alice["app_retention_hours"], 72);
    assert_eq!(me(&person).await["quota"]["maxBuilds"], 10);

    // A new limit applies to the next upload, without a restart.
    let quota = env.pool.store.user_by_name("alice").unwrap().unwrap().quota;
    env.pool.store.set_quota("alice", seiji::config::Quota { max_builds: Some(5), ..quota }).unwrap();
    assert_eq!(upload(&service).await.unwrap().status(), 200);
    assert_eq!(upload(&service).await.unwrap().status(), 200);
    assert_eq!(upload(&service).await.unwrap().json::<Value>().await.unwrap()["error"], full(5, 5));
    assert_eq!(me(&service).await["quota"]["maxBuilds"], 5);

    let c = client(&env, &service).await;
    let budget = ok_json(&call(&c, "budget_status", json!({})).await);
    assert_eq!(budget["max_builds"], 5);
    assert_eq!(budget["app_retention_hours"], 72);
}

#[tokio::test]
async fn reaper_ends_leases_of_disabled_users_and_overlong_leases() {
    let env = setup(2).await;
    let alice = client(&env, &add_user(&env, "alice", QUOTA)).await;
    let bob = client(&env, &add_user(&env, "bob", QUOTA)).await;
    let a = ok_json(&call(&alice, "acquire_device", json!({"platform": "ios"})).await);
    let b = ok_json(&call(&bob, "acquire_device", json!({"platform": "ios"})).await);
    let (a, b) = (a["lease_id"].as_str().unwrap(), b["lease_id"].as_str().unwrap());

    env.pool.store.set_disabled("alice", true).unwrap();
    {
        // Bob's lease is 31 minutes old (limit 30) but was used a minute ago.
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        let t = now();
        conn.execute(
            "UPDATE leases SET started_at = ?1, last_activity_at = ?2 WHERE id = ?3",
            rusqlite::params![t - 31 * 60, t - 60, b],
        )
        .unwrap();
    }
    env.pool.reap().await.unwrap();
    assert_eq!(env.pool.store.lease(a).unwrap().unwrap().end_reason.as_deref(), Some("user_disabled"));
    assert_eq!(env.pool.store.lease(b).unwrap().unwrap().end_reason.as_deref(), Some("max_lease_duration"));

    let again = call(&bob, "release_device", json!({"lease_id": b})).await;
    assert!(is_err(&again) && text(&again).contains("no active lease"), "{}", text(&again));
}

#[tokio::test]
async fn releasing_during_boot_is_billed_and_destroys_the_device() {
    let env = setup(2).await;
    add_user(&env, "alice", QUOTA);
    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    env.fake.hang_boot.store(true, Ordering::SeqCst);

    let pool = env.pool.clone();
    let booting_user = user.clone();
    let acquire = tokio::spawn(async move { pool.acquire(&booting_user, Platform::Ios, None).await });
    let id = loop {
        if let Some(l) = env.pool.store.active_leases().unwrap().into_iter().next()
            && l.device_id.is_some()
        {
            break l.id;
        }
        tokio::task::yield_now().await;
    };
    assert!(env.pool.release(&id, "released").await.unwrap());
    let err = acquire.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("released while the device booted"), "{err:#}");

    let lease = env.pool.store.lease(&id).unwrap().unwrap();
    assert_eq!(lease.end_reason.as_deref(), Some("released"));
    assert_eq!(env.fake.destroyed.lock().unwrap().as_slice(), std::slice::from_ref(&id));
    let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
    let billable: i64 = conn.query_row("SELECT billable FROM leases WHERE id = ?1", [&id], |r| r.get(0)).unwrap();
    assert_eq!(billable, 1);
}

#[tokio::test]
async fn sse_streams_headers_and_keepalive_before_tool_completes() {
    use std::time::Duration;

    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    env.fake.hang_boot.store(true, Ordering::SeqCst);

    let http = reqwest::Client::new();
    let body = json!({
        "jsonrpc": "2.0", "id": 1,
        "method": "tools/call",
        "params": {"name": "acquire_device", "arguments": {"platform": "ios"}}
    });

    // Send the request. The server returns SSE headers at once, before the boot completes.
    let mut resp = tokio::time::timeout(Duration::from_secs(5), async {
        http.post(format!("{}/mcp", env.url))
            .bearer_auth(&token)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&body)
            .send()
            .await
            .unwrap()
    })
    .await
    .expect("headers did not arrive within 5 s");

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");

    // Confirm the boot is active before we start reading the stream.
    tokio::time::timeout(Duration::from_secs(5), env.fake.boot_entered.notified())
        .await
        .expect("boot did not enter within 5 s");

    // Read until at least one keepalive comment (`:`) arrives (15 s interval).
    let mut buf = String::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match resp.chunk().await.unwrap() {
                Some(bytes) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    if buf.lines().any(|l| l.starts_with(':')) {
                        return;
                    }
                }
                None => panic!("SSE stream ended before a keepalive comment"),
            }
        }
    })
    .await
    .expect("no keepalive within 20 s");

    // Release the lease now that the keepalive proves the stream is alive.
    let lease_id = env
        .pool
        .store
        .active_leases()
        .unwrap()
        .into_iter()
        .find(|l| l.device_id.is_some())
        .expect("no active lease with a device")
        .id;
    env.pool.release(&lease_id, "released").await.unwrap();

    // Read the complete final SSE data event (JSON may span chunks).
    let msg: Value = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(val) = sse_data_json(&buf) {
                return val;
            }
            match resp.chunk().await.unwrap() {
                Some(bytes) => buf.push_str(&String::from_utf8_lossy(&bytes)),
                None => return sse_data_json(&buf).expect("stream ended without data event"),
            }
        }
    })
    .await
    .expect("data event did not arrive within 10 s");

    assert_eq!(msg["jsonrpc"], "2.0");
    assert_eq!(msg["id"], 1);
    assert_eq!(msg["result"]["isError"], true);
    let content = msg["result"]["content"][0]["text"].as_str().unwrap();
    assert!(content.contains("released while the device booted"), "unexpected: {content}");

    // The specific lease was destroyed and ended with the right reason.
    let destroyed = env.fake.destroyed.lock().unwrap();
    assert!(destroyed.contains(&lease_id), "lease {lease_id} not in destroyed: {destroyed:?}");
    let ended = env.pool.store.lease(&lease_id).unwrap().unwrap();
    assert_eq!(ended.end_reason.as_deref(), Some("released"));
}

/// Extracts the first complete JSON-RPC message from SSE `data:` lines in `buf`.
fn sse_data_json(buf: &str) -> Option<Value> {
    for line in buf.lines() {
        if let Some(payload) = line.strip_prefix("data:")
            && let Ok(val) = serde_json::from_str::<Value>(payload.trim())
        {
            return Some(val);
        }
    }
    None
}
