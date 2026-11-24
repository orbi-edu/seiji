//! The live screen viewer: links, sessions, the shared stream, input and its guards.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use common::*;

fn http() -> reqwest::Client {
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

/// Acquires an iOS device for `user`; returns its token and lease id.
async fn device(env: &Env, user: &str) -> (String, String) {
    let token = add_user(env, user, QUOTA);
    let c = client(env, &token).await;
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    (token, lease["lease_id"].as_str().unwrap().to_string())
}

async fn link(env: &Env, token: &str, lease: &str, body: Value) -> reqwest::Response {
    http().post(format!("{}/v1/leases/{lease}/viewer", env.url)).bearer_auth(token).json(&body).send().await.unwrap()
}

/// A viewer session: its base URL and the cookie that unlocks it.
struct View {
    base: String,
    cookie: String,
}

async fn open(env: &Env, token: &str, lease: &str, control: bool) -> View {
    let l: Value = link(env, token, lease, json!({"control": control})).await.json().await.unwrap();
    open_link(env, &l).await
}

/// Opens the link of a viewer link response.
async fn open_link(env: &Env, l: &Value) -> View {
    let opened = http().get(format!("{}{}", env.url, l["url"].as_str().unwrap())).send().await.unwrap();
    assert_eq!(opened.status(), 303);
    let location = opened.headers()["location"].to_str().unwrap().to_string();
    let set_cookie = opened.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains(&format!("Path={}", location.trim_end_matches('/'))), "{set_cookie} {location}");
    View { base: format!("{}{location}", env.url), cookie: set_cookie.split(';').next().unwrap().to_string() }
}

async fn admin_link(env: &Env, token: &str, lease: &str, body: Option<Value>) -> reqwest::Response {
    let req = http().post(format!("{}/v1/admin/leases/{lease}/viewer", env.url)).bearer_auth(token);
    match body {
        Some(b) => req.json(&b),
        None => req,
    }
    .send()
    .await
    .unwrap()
}

impl View {
    async fn get(&self, path: &str) -> reqwest::Response {
        http().get(format!("{}{path}", self.base)).header("cookie", &self.cookie).send().await.unwrap()
    }
    async fn input(&self, input: Value) -> reqwest::Response {
        http()
            .post(format!("{}input", self.base))
            .header("cookie", &self.cookie)
            .header("x-seiji-view", "1")
            .json(&json!({ "input": input, "aspect": 402.0 / 874.0 }))
            .send()
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_link_opens_one_session_for_its_owner_only() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let (bob, _) = device(&env, "bob").await;

    assert_eq!(http().post(format!("{}/v1/leases/{lease}/viewer", env.url)).send().await.unwrap().status(), 401);
    let theirs = link(&env, &bob, &lease, json!({})).await;
    assert_eq!(theirs.status(), 404);

    let l: Value = link(&env, &token, &lease, json!({})).await.json().await.unwrap();
    assert_eq!(l["control"], true);
    assert_eq!(l["expires_in"], 60);
    let url = format!("{}{}", env.url, l["url"].as_str().unwrap());
    assert_eq!(http().get(&url).send().await.unwrap().status(), 303);
    assert_eq!(http().get(&url).send().await.unwrap().status(), 401, "a ticket opens once");

    let view = open(&env, &token, &lease, true).await;
    let page = view.get("").await;
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-security-policy"].to_str().unwrap().contains("default-src 'none'"));
    assert!(page.text().await.unwrap().contains("/view/assets/viewer.js"));

    let anonymous = http().get(&view.base).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    let forged = http().get(&view.base).header("cookie", "seiji_view=00").send().await.unwrap();
    assert_eq!(forged.status(), 401);

    let info: Value = view.get("info").await.json().await.unwrap();
    assert_eq!(info["lease_id"], lease);
    assert_eq!(info["model"], "iphone-16");
    assert_eq!(info["control"], true);
    assert_eq!(info["busy"], false);
}

#[tokio::test]
async fn an_admin_lists_every_device_and_watches_any_of_them() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice = add_user(&env, "alice", QUOTA);
    let (bob, lease) = device(&env, "bob").await;

    let leases = |token: String| {
        let url = format!("{}/v1/admin/leases", env.url);
        async move { http().get(url).bearer_auth(token).send().await.unwrap() }
    };
    assert_eq!(leases(bob.clone()).await.status(), 403);
    let list: Value = leases(alice.clone()).await.json().await.unwrap();
    let entries = list["leases"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{list}");
    assert_eq!(entries[0]["lease_id"], lease);
    assert_eq!(entries[0]["user"], "bob");
    assert_eq!(entries[0]["status"], "ready");
    assert_eq!(entries[0]["platform"], "ios");

    // Only an admin gets a link to another user's device; the owner's own route still refuses it.
    assert_eq!(admin_link(&env, &bob, &lease, None).await.status(), 403);
    assert_eq!(link(&env, &alice, &lease, json!({})).await.status(), 404);
    assert_eq!(admin_link(&env, &alice, "l_000000000000", None).await.status(), 404);
    assert_eq!(admin_link(&env, &alice, &lease, Some(json!({"role": "x"}))).await.status(), 400);

    // View-only by default: the session shows bob's device and refuses input.
    let resp = admin_link(&env, &alice, &lease, None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let l: Value = resp.json().await.unwrap();
    assert_eq!(l["control"], false);
    let watcher = open_link(&env, &l).await;
    let info: Value = watcher.get("info").await.json().await.unwrap();
    assert_eq!(info["lease_id"], lease);
    assert_eq!(info["user"], "bob");
    assert_eq!(info["control"], false);
    assert_eq!(watcher.input(json!({"kind": "key", "code": 40})).await.status(), 403);

    // With control, the admin's input reaches bob's device.
    let l: Value = admin_link(&env, &alice, &lease, Some(json!({"control": true}))).await.json().await.unwrap();
    assert_eq!(l["control"], true);
    let driver = open_link(&env, &l).await;
    assert_eq!(driver.input(json!({"kind": "key", "code": 40})).await.status(), 204);
    assert!(env.fake.log().contains(&"key 40".to_string()), "{:?}", env.fake.log());

    // Disabling the admin ends the admin's sessions; the owner's own session keeps working.
    let own = open(&env, &bob, &lease, false).await;
    env.pool.store.set_disabled("alice", true).unwrap();
    assert_eq!(watcher.get("info").await.status(), 401);
    assert_eq!(driver.input(json!({"kind": "key", "code": 40})).await.status(), 401);
    assert_eq!(own.get("info").await.status(), 200);
}

#[tokio::test]
async fn viewers_share_one_stream_until_the_lease_ends() {
    let env = setup_with(2, json!({"viewer": {"maxViewersPerLease": 2, "fps": 20}})).await;
    let (token, lease) = device(&env, "alice").await;
    let a = open(&env, &token, &lease, false).await;
    let b = open(&env, &token, &lease, false).await;
    let c = open(&env, &token, &lease, false).await;

    let mut s1 = a.get("stream").await;
    assert_eq!(s1.status(), 200);
    assert!(
        s1.headers()["content-type"].to_str().unwrap().starts_with("multipart/x-mixed-replace; boundary=seijiframe")
    );
    let first = tokio::time::timeout(Duration::from_secs(2), s1.chunk()).await.unwrap().unwrap().unwrap();
    let first_str = String::from_utf8_lossy(&first);
    assert!(
        first_str.contains("--seijiframe\r\n")
            && first_str.contains("Content-Type: image/jpeg")
            && first_str.contains("X-Capture-Time:"),
        "{first_str}"
    );
    let s2 = b.get("stream").await;
    assert_eq!(s2.status(), 200);
    assert_eq!(c.get("stream").await.status(), 429, "maxViewersPerLease");
    assert_eq!(env.fake.streams_started.load(Ordering::SeqCst), 1, "one device stream for every viewer");

    let user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let c2 = client(&env, &token).await;
    assert!(!is_err(&call(&c2, "release_device", json!({"lease_id": lease})).await));
    // The stream ends with the lease, and the session with it.
    let rest =
        tokio::time::timeout(Duration::from_secs(3), async { while let Ok(Some(_)) = s1.chunk().await {} }).await;
    assert!(rest.is_ok(), "the stream should end when the lease is released");
    assert_eq!(a.get("info").await.status(), 410);
    drop((s2, user));
}

#[tokio::test]
async fn a_still_screen_still_reaches_every_viewer() {
    // The host skips identical frames: on a screen that does not change, the stream holds one
    // frame. A viewer that joins later must still get it, and an open stream must not go quiet.
    let env = setup_with(2, json!({"viewer": {"maxViewersPerLease": 2}})).await;
    env.fake.still.store(true, Ordering::SeqCst);
    let (token, lease) = device(&env, "alice").await;
    let a = open(&env, &token, &lease, false).await;
    let mut s1 = a.get("stream").await;
    let first = tokio::time::timeout(Duration::from_secs(2), s1.chunk()).await.unwrap().unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("--seijiframe\r\n"));

    let b = open(&env, &token, &lease, false).await;
    let mut s2 = b.get("stream").await;
    let joined = tokio::time::timeout(Duration::from_millis(500), s2.chunk()).await;
    assert!(matches!(joined, Ok(Ok(Some(_)))), "a later viewer gets the current frame at once");

    let again = tokio::time::timeout(Duration::from_secs(2), s1.chunk()).await;
    assert!(matches!(again, Ok(Ok(Some(_)))), "the last frame is repeated while the screen is still");
}

#[tokio::test]
async fn input_maps_to_points_and_respects_view_only_and_runs() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let view = open(&env, &token, &lease, true).await;

    assert_eq!(view.input(json!({"kind": "tap", "x": 0.5, "y": 0.5})).await.status(), 204);
    assert_eq!(view.input(json!({"kind": "tap", "x": 0.1, "y": 0.2, "hold": 0.8})).await.status(), 204);
    assert_eq!(view.input(json!({"kind": "type", "text": "hello"})).await.status(), 204);
    assert_eq!(view.input(json!({"kind": "key", "code": 40})).await.status(), 204);
    assert_eq!(view.input(json!({"kind": "button", "name": "home"})).await.status(), 204);
    assert_eq!(env.fake.log()[..4], ["tap 201,437", "tap 40,175 hold 0.8", "type hello", "key 40"]);

    assert_eq!(view.input(json!({"kind": "tap", "x": 2, "y": 0.5})).await.status(), 400);
    let csrf = http()
        .post(format!("{}input", view.base))
        .header("cookie", &view.cookie)
        .json(&json!({"input": {"kind": "key", "code": 40}}))
        .send()
        .await
        .unwrap();
    assert_eq!(csrf.status(), 400, "input needs the X-Seiji-View header");

    let watcher = open(&env, &token, &lease, false).await;
    let refused = watcher.input(json!({"kind": "key", "code": 40})).await;
    assert_eq!(refused.status(), 403);

    // A run holds the device: input waits for it instead of interleaving with its steps.
    let c = client(&env, &token).await;
    let run = tokio::spawn(async move {
        call(&c, "run_actions", json!({"record_video": false, "actions": [{"sleep": {"seconds": 3}}]})).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = view.input(json!({"kind": "key", "code": 40})).await;
    assert_eq!(busy.status(), 409);
    let info: Value = view.get("info").await.json().await.unwrap();
    assert_eq!(info["busy"], true);
    assert!(!is_err(&run.await.unwrap()));
    assert_eq!(view.input(json!({"kind": "key", "code": 40})).await.status(), 204);
}

#[tokio::test]
async fn agents_share_a_view_only_link() {
    let env = setup(2).await;
    let (token, _) = device(&env, "alice").await;
    let c = client(&env, &token).await;
    let link = ok_json(&call(&c, "share_screen", json!({})).await);
    assert_eq!(link["control"], false);
    let opened = http().get(format!("{}{}", env.url, link["url"].as_str().unwrap())).send().await.unwrap();
    assert_eq!(opened.status(), 303);
}

#[tokio::test]
async fn paste_puts_text_on_pasteboard_and_respects_guards() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let ctrl = open(&env, &token, &lease, true).await;
    let watcher = open(&env, &token, &lease, false).await;

    // Paste: the device clipboard, then Cmd+V into the focused field.
    let pasted = ctrl.input(json!({"kind": "paste", "text": "olá clipboard"})).await;
    assert_eq!(pasted.status(), 200);
    assert_eq!(pasted.json::<serde_json::Value>().await.unwrap(), json!({"via": "pasted"}));
    let log = env.fake.log();
    let at = log.iter().position(|l| l.ends_with("pasteboard:olá clipboard")).expect("clipboard set");
    assert_eq!(log[at + 1], "key-combo [227]+25", "{log:?}");

    // Without a reachable pasteboard the text is typed, and the page is told so.
    env.fake.no_pasteboard.store(true, Ordering::SeqCst);
    let typed = ctrl.input(json!({"kind": "paste", "text": "olá"})).await;
    assert_eq!(typed.status(), 200);
    assert_eq!(typed.json::<serde_json::Value>().await.unwrap(), json!({"via": "typed"}));
    assert!(env.fake.log().iter().any(|l| l == "type olá"), "{:?}", env.fake.log());
    env.fake.no_pasteboard.store(false, Ordering::SeqCst);

    // Paste and type goes through the typing path.
    assert_eq!(ctrl.input(json!({"kind": "paste_and_type", "text": "typed text"})).await.status(), 204);
    assert!(env.fake.log().iter().any(|l| l == "type typed text"), "{:?}", env.fake.log());

    // View-only cannot paste.
    assert_eq!(watcher.input(json!({"kind": "paste", "text": "no"})).await.status(), 403);

    // Empty or over-limit text fails.
    assert_eq!(ctrl.input(json!({"kind": "paste", "text": ""})).await.status(), 400);
    let big = "x".repeat(10_001);
    assert_eq!(ctrl.input(json!({"kind": "paste", "text": big})).await.status(), 400);
}

#[tokio::test]
async fn stats_endpoint_returns_stream_data() {
    let env = setup_with(2, json!({"viewer": {"fps": 20}})).await;
    let (token, lease) = device(&env, "alice").await;
    let view = open(&env, &token, &lease, false).await;

    // Start the stream so the hub exists.
    let mut stream = view.get("stream").await;
    assert_eq!(stream.status(), 200);
    // Read at least one frame.
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.chunk()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let stats: Value = view.get("stats").await.json().await.unwrap();
    assert!(stats["stream"]["total_frames"].as_u64().unwrap() > 0, "{stats}");
    assert!(stats["stream"]["fps"].as_f64().unwrap() > 0.0, "{stats}");
    assert_eq!(stats["preset"], "balanced");
    assert!(stats["opts"]["fps"].as_u64().is_some());
    assert!(stats["opts"]["scale"].as_f64().is_some());

    // Without a session the endpoint fails.
    assert_eq!(http().get(format!("{}stats", view.base)).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn configure_device_sets_pasteboard() {
    let env = setup(1).await;
    let (token, _lease) = device(&env, "alice").await;
    let c = client(&env, &token).await;
    let r = call(&c, "configure_device", json!({"pasteboard": "agent text"})).await;
    assert!(!is_err(&r), "{}", text(&r));
    assert!(text(&r).contains("pasteboard"), "{}", text(&r));
    assert!(env.fake.log().iter().any(|l| l.contains("pasteboard:agent text")), "{:?}", env.fake.log());

    // Empty pasteboard fails.
    let r2 = call(&c, "configure_device", json!({"pasteboard": ""})).await;
    assert!(is_err(&r2));
}
