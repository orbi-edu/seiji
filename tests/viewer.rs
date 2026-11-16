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
    let opened = http().get(format!("{}{}", env.url, l["url"].as_str().unwrap())).send().await.unwrap();
    assert_eq!(opened.status(), 303);
    let location = opened.headers()["location"].to_str().unwrap().to_string();
    let set_cookie = opened.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"), "{set_cookie}");
    assert!(set_cookie.contains(&format!("Path={}", location.trim_end_matches('/'))), "{set_cookie} {location}");
    View { base: format!("{}{location}", env.url), cookie: set_cookie.split(';').next().unwrap().to_string() }
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
    assert!(
        first.starts_with(b"--seijiframe\r\nContent-Type: image/jpeg\r\nContent-Length: 5\r\n\r\n\xff\xd8"),
        "{first:?}"
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
