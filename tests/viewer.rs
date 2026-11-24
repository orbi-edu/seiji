//! The live screen viewer: links, sessions, the shared H.264 stream over a WebSocket, input and
//! its guards.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};

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

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What the page receives.
#[derive(Debug)]
enum Msg {
    Text(Value),
    Frame { key: bool, seq: u32 },
    Closed(u16, String),
}

impl View {
    async fn get(&self, path: &str) -> reqwest::Response {
        http().get(format!("{}{path}", self.base)).header("cookie", &self.cookie).send().await.unwrap()
    }

    /// The page's WebSocket, from `origin` (the page's own origin when `None`).
    async fn connect(&self, origin: Option<&str>) -> Result<Socket, u16> {
        let mut req = format!("{}ws", self.base.replacen("http", "ws", 1)).into_client_request().unwrap();
        let own = self.base.split("/view/").next().unwrap().to_string();
        req.headers_mut().insert("cookie", self.cookie.parse().unwrap());
        req.headers_mut().insert("origin", origin.unwrap_or(&own).parse().unwrap());
        match tokio_tungstenite::connect_async(req).await {
            Ok((ws, _)) => Ok(ws),
            Err(tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
            Err(e) => panic!("{e}"),
        }
    }

    async fn ws(&self) -> Socket {
        self.connect(None).await.unwrap()
    }
}

async fn send(ws: &mut Socket, v: Value) {
    ws.send(tungstenite::Message::text(v.to_string())).await.unwrap();
}

async fn next(ws: &mut Socket) -> Msg {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(5), ws.next()).await.expect("nothing within 5 s");
        match m {
            Some(Ok(tungstenite::Message::Text(t))) => return Msg::Text(serde_json::from_str(&t).unwrap()),
            Some(Ok(tungstenite::Message::Binary(b))) => {
                return Msg::Frame { key: b[0] & 1 == 1, seq: u32::from_be_bytes(b[1..5].try_into().unwrap()) };
            }
            Some(Ok(tungstenite::Message::Close(f))) => {
                return f.map_or(Msg::Closed(1005, String::new()), |f| {
                    Msg::Closed(u16::from(f.code), f.reason.to_string())
                });
            }
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return Msg::Closed(1006, String::new()),
        }
    }
}

/// One deadline for a whole wait, however many unrelated messages arrive meanwhile.
async fn within<T>(what: &str, wait: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), wait).await.unwrap_or_else(|_| panic!("no {what} within 10 s"))
}

/// The next text message that is not a decoder setup, acknowledging frames on the way.
async fn text(ws: &mut Socket) -> Value {
    within("reply", async {
        loop {
            match next(ws).await {
                Msg::Text(v) if v["t"] == "format" => {}
                Msg::Text(v) => return v,
                Msg::Frame { seq, .. } => send(ws, json!({"t": "ack", "seq": seq})).await,
                Msg::Closed(code, why) => panic!("closed {code}: {why}"),
            }
        }
    })
    .await
}

/// The next decoder setup.
async fn format(ws: &mut Socket) -> Value {
    within("format", async {
        loop {
            match next(ws).await {
                Msg::Text(v) if v["t"] == "format" => return v,
                Msg::Closed(code, why) => panic!("closed {code}: {why}"),
                _ => {}
            }
        }
    })
    .await
}

/// The next frame, acknowledged; text messages on the way are skipped.
async fn frame(ws: &mut Socket) -> (bool, u32) {
    within("frame", async {
        loop {
            match next(ws).await {
                Msg::Frame { key, seq } => {
                    send(ws, json!({"t": "ack", "seq": seq})).await;
                    return (key, seq);
                }
                Msg::Text(_) => {}
                Msg::Closed(code, why) => panic!("closed {code}: {why}"),
            }
        }
    })
    .await
}

/// Waits until the socket closes; returns the close code.
async fn closed(ws: &mut Socket) -> u16 {
    within("close", async {
        loop {
            if let Msg::Closed(code, _) = next(ws).await {
                return code;
            }
        }
    })
    .await
}

/// Waits until the fake device's log has `line`.
async fn logged(env: &Env, line: &str) {
    for _ in 0..100 {
        if env.fake.log().iter().any(|l| l == line) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{line:?} never logged: {:?}", env.fake.log());
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
    let csp = page.headers()["content-security-policy"].to_str().unwrap().to_string();
    assert!(csp.contains("default-src 'none'") && csp.contains("connect-src 'self' wss://127.0.0.1:"), "{csp}");
    assert!(page.text().await.unwrap().contains("/view/assets/viewer.js"));

    let anonymous = http().get(&view.base).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);
    let forged = http().get(&view.base).header("cookie", "seiji_view=00").send().await.unwrap();
    assert_eq!(forged.status(), 401);
    let no_cookie = View { base: view.base.clone(), cookie: "seiji_view=00".into() };
    assert_eq!(no_cookie.connect(None).await.err(), Some(401));
    assert_eq!(view.connect(Some("https://evil.example")).await.err(), Some(403), "a page on another site");

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

    // A disabled owner's lease, or one still booting, gets no admin link.
    env.pool.store.set_disabled("bob", true).unwrap();
    assert_eq!(admin_link(&env, &alice, &lease, None).await.status(), 404);
    env.pool.store.set_disabled("bob", false).unwrap();
    add_user(&env, "carol", QUOTA);
    let carol = env.pool.store.user_by_name("carol").unwrap().unwrap();
    env.fake.hang_boot.store(true, Ordering::SeqCst);
    let pool = env.pool.clone();
    let acquire = tokio::spawn(async move { pool.acquire(&carol, seiji::config::Platform::Ios, None).await });
    env.fake.boot_entered.notified().await;
    let booting = env.pool.store.active_leases().unwrap().into_iter().find(|l| l.id != lease).unwrap().id;
    let resp = admin_link(&env, &alice, &booting, None).await;
    assert_eq!(resp.status(), 404);
    let refused: Value = resp.json().await.unwrap();
    assert!(refused["error"].as_str().unwrap().contains("still booting"), "{refused}");
    assert!(env.pool.release(&booting, "released").await.unwrap());
    assert!(acquire.await.unwrap().is_err());
    env.fake.hang_boot.store(false, Ordering::SeqCst);

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
    let mut watching = watcher.ws().await;
    send(&mut watching, json!({"t": "key", "usage": 40})).await;
    assert_eq!(text(&mut watching).await["code"], "view_only");

    // With control, the admin's input reaches bob's device.
    let l: Value = admin_link(&env, &alice, &lease, Some(json!({"control": true}))).await.json().await.unwrap();
    assert_eq!(l["control"], true);
    let driver = open_link(&env, &l).await;
    let mut driving = driver.ws().await;
    send(&mut driving, json!({"t": "key", "usage": 40})).await;
    logged(&env, "hid key 40 down").await;

    // Disabling the admin ends the admin's sessions; the owner's own session keeps working.
    let own = open(&env, &bob, &lease, false).await;
    env.pool.store.set_disabled("alice", true).unwrap();
    assert_eq!(watcher.get("info").await.status(), 401);
    assert_eq!(closed(&mut driving).await, 4401, "the admin's socket ends within the recheck");
    assert_eq!(own.get("info").await.status(), 200);
}

#[tokio::test]
async fn viewers_share_one_stream_until_the_lease_ends() {
    let env = setup_with(2, json!({"viewer": {"maxViewersPerLease": 2, "fps": 20}})).await;
    let (token, lease) = device(&env, "alice").await;
    let (a, b, c) = (
        open(&env, &token, &lease, false).await,
        open(&env, &token, &lease, false).await,
        open(&env, &token, &lease, false).await,
    );

    let mut s1 = a.ws().await;
    let setup = format(&mut s1).await;
    assert_eq!(setup["codec"], "avc1.64001f", "{setup}");
    assert_eq!((setup["width"].as_u64(), setup["height"].as_u64()), (Some(402), Some(874)));
    assert!(frame(&mut s1).await.0, "a viewer starts at a keyframe");
    let mut s2 = b.ws().await;
    assert_eq!(format(&mut s2).await["codec"], "avc1.64001f");
    assert!(frame(&mut s2).await.0, "a joining viewer starts at a keyframe too");
    assert_eq!(c.connect(None).await.err(), Some(429), "maxViewersPerLease");
    assert_eq!(env.fake.streams_started.load(Ordering::SeqCst), 1, "one helper for every viewer");

    let c2 = client(&env, &token).await;
    assert!(!is_err(&call(&c2, "release_device", json!({"lease_id": lease})).await));
    assert_eq!(closed(&mut s1).await, 4410, "the stream ends with the lease");
    assert_eq!(a.get("info").await.status(), 410);
}

#[tokio::test]
async fn a_still_screen_reaches_a_viewer_that_joins_later() {
    // A static screen produces no frames; a viewer that joins asks for a keyframe and gets the
    // current screen at once.
    let env = setup_with(2, json!({"viewer": {"maxViewersPerLease": 2}})).await;
    env.fake.still.store(true, Ordering::SeqCst);
    let (token, lease) = device(&env, "alice").await;
    let mut s1 = open(&env, &token, &lease, false).await.ws().await;
    assert!(frame(&mut s1).await.0);

    let mut s2 = open(&env, &token, &lease, false).await.ws().await;
    let joined = tokio::time::timeout(Duration::from_millis(500), frame(&mut s2)).await;
    assert!(matches!(joined, Ok((true, _))), "a later viewer gets a keyframe at once: {joined:?}");

    // A page that asks again right away (its decoder dropped frames) is inside the throttle; the
    // request waits and still goes out, though the screen does not change.
    send(&mut s2, json!({"t": "keyframe"})).await;
    let again = tokio::time::timeout(Duration::from_secs(2), frame(&mut s2)).await;
    assert!(matches!(again, Ok((true, _))), "a throttled keyframe request is retried: {again:?}");
}

#[tokio::test]
async fn a_slow_viewer_gets_no_stale_frames_and_resumes_at_a_keyframe() {
    let env = setup_with(2, json!({"viewer": {"fps": 60}})).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, false).await.ws().await;
    format(&mut ws).await;

    // Acknowledge nothing: frames queue up on the way, and past half a second the server stops
    // sending instead of queueing more.
    let started = std::time::Instant::now();
    let mut frames = Vec::new();
    let quiet = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            if let Msg::Frame { seq, .. } = next(&mut ws).await {
                frames.push((seq, started.elapsed()));
            }
        }
    });
    let _ = quiet.await;
    let (last, last_at) = *frames.last().expect("frames before the stall");
    assert!(frames.len() < 60, "{} frames for 1.5 s at 60 fps: they were not held back", frames.len());
    assert!(last_at < Duration::from_millis(800), "a frame went out {last_at:?} into the stall");
    // A congested viewer lowers the shared bitrate.
    assert!(env.fake.log().iter().any(|l| l.starts_with("encode 60 0.5 1750000")), "{:?}", env.fake.log());

    // Acknowledging catches up: the stream resumes at a keyframe.
    send(&mut ws, json!({"t": "ack", "seq": last})).await;
    let (key, seq) = frame(&mut ws).await;
    assert!(key, "after a gap the next frame is a keyframe");
    assert!(seq > last + 1, "frames were skipped, not queued");
}

#[tokio::test]
async fn touches_keys_and_buttons_reach_the_device_in_order() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let view = open(&env, &token, &lease, true).await;
    let mut ws = view.ws().await;

    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.5, "y": 0.98, "edge": "bottom"})).await;
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.6})).await;
    // The viewer's own finger holds the device; the page does not show that as busy.
    logged(&env, "touch move 0.50,0.60").await;
    let info: Value = view.get("info").await.json().await.unwrap();
    assert_eq!(info["busy"], false);
    send(&mut ws, json!({"t": "touch", "phase": "up", "x": 0.5, "y": 0.4})).await;
    send(&mut ws, json!({"t": "key", "usage": 4, "mods": [225]})).await;
    send(&mut ws, json!({"t": "button", "name": "home"})).await;
    send(&mut ws, json!({"t": "type", "id": 7, "text": "hello"})).await;
    let done = text(&mut ws).await;
    assert_eq!(done, json!({"t": "done", "id": 7}));
    let log: Vec<String> = env.fake.log().into_iter().filter(|l| !l.starts_with("encode")).collect();
    assert_eq!(
        log,
        [
            "touch down 0.50,0.98 edge bottom",
            "touch move 0.50,0.60",
            "touch up 0.50,0.40",
            "hid key 225 down",
            "hid key 4 down",
            "hid key 4 up",
            "hid key 225 up",
            "hid button home down",
            "hid button home up",
            "type hello",
        ]
    );

    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 2, "y": 0.5})).await;
    assert_eq!(text(&mut ws).await["code"], "bad_request");
    send(&mut ws, json!({"t": "key", "usage": 4, "mods": [4]})).await;
    assert_eq!(text(&mut ws).await["code"], "bad_request", "mods are modifier usages only");

    let mut watcher = open(&env, &token, &lease, false).await.ws().await;
    send(&mut watcher, json!({"t": "touch", "phase": "down", "x": 0.5, "y": 0.5})).await;
    send(&mut watcher, json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.6})).await;
    send(&mut watcher, json!({"t": "button", "name": "home"})).await;
    assert_eq!(text(&mut watcher).await["code"], "view_only");
    assert_eq!(text(&mut watcher).await["code"], "view_only", "once for the gesture, once for the button");

    // A run holds the device: input waits for it instead of interleaving with its steps.
    let c = client(&env, &token).await;
    let run = tokio::spawn(async move {
        call(&c, "run_actions", json!({"record_video": false, "actions": [{"sleep": {"seconds": 3}}]})).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.5, "y": 0.5})).await;
    assert_eq!(text(&mut ws).await["code"], "busy");
    let info: Value = view.get("info").await.json().await.unwrap();
    assert_eq!(info["busy"], true);
    assert!(!is_err(&run.await.unwrap()));
    send(&mut ws, json!({"t": "key", "usage": 40})).await;
    logged(&env, "hid key 40 up").await;
}

#[tokio::test]
async fn a_finger_left_down_is_lifted_when_the_page_goes_away() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.3, "y": 0.3})).await;
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.3, "y": 0.7})).await;
    logged(&env, "touch move 0.30,0.70").await;
    drop(ws);
    logged(&env, "touch up 0.30,0.70").await;

    // And the device is free for the agent again.
    let c = client(&env, &token).await;
    assert!(!is_err(&call(&c, "inspect_screen", json!({})).await));
}

/// Frames of 1 MiB at 60 fps: a page that stops reading fills its socket within a second.
async fn heavy_stream() -> Env {
    let env = setup_with(2, json!({"viewer": {"fps": 60}})).await;
    env.fake.frame_bytes.store(1 << 20, Ordering::SeqCst);
    env
}

#[tokio::test]
async fn input_reaches_the_device_while_a_frame_is_stuck_on_a_slow_link() {
    let env = heavy_stream().await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;
    // The page reads nothing: the socket fills and the server's write waits.
    tokio::time::sleep(Duration::from_millis(500)).await;

    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.2, "y": 0.5})).await;
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.5})).await;
    send(&mut ws, json!({"t": "touch", "phase": "up", "x": 0.8, "y": 0.5})).await;
    send(&mut ws, json!({"t": "type", "id": 9, "text": "hi"})).await;
    logged(&env, "type hi").await;
    let log: Vec<String> = env.fake.log().into_iter().filter(|l| !l.starts_with("encode")).collect();
    assert_eq!(log, ["touch down 0.20,0.50", "touch move 0.50,0.50", "touch up 0.80,0.50", "type hi"]);

    // Once the link drains, the reply arrives.
    assert_eq!(text(&mut ws).await, json!({"t": "done", "id": 9}));
}

#[tokio::test]
async fn a_page_on_a_dead_link_is_closed_and_lets_the_device_go() {
    let env = heavy_stream().await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;
    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.3, "y": 0.3})).await;
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.3, "y": 0.7})).await;
    logged(&env, "touch move 0.30,0.70").await;

    // The socket stays open but nothing reads it. After the 5 s send timeout, not the 10 s
    // gesture timeout, the finger is lifted and the device is free.
    let started = std::time::Instant::now();
    while !env.fake.log().iter().any(|l| l == "touch up 0.30,0.70") {
        assert!(started.elapsed() < Duration::from_secs(8), "the finger is still down: {:?}", env.fake.log());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let c = client(&env, &token).await;
    assert!(!is_err(&call(&c, "inspect_screen", json!({})).await));
    drop(ws);
}

#[tokio::test]
async fn a_long_steady_round_trip_keeps_the_frame_rate_and_bitrate() {
    let env = setup_with(2, json!({"viewer": {"fps": 60}})).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, false).await.ws().await;
    format(&mut ws).await;
    let (mut tx, mut rx) = ws.split();

    // Each ack leaves 400 ms after its frame arrived, as over a 400 ms round trip.
    let (acks, mut pending) = tokio::sync::mpsc::unbounded_channel::<(u32, tokio::time::Instant)>();
    let acker = tokio::spawn(async move {
        while let Some((seq, at)) = pending.recv().await {
            tokio::time::sleep_until(at + Duration::from_millis(400)).await;
            if tx.send(tungstenite::Message::text(json!({"t": "ack", "seq": seq}).to_string())).await.is_err() {
                return;
            }
        }
    });
    let mut arrivals = Vec::new();
    let window = tokio::time::Instant::now() + Duration::from_secs(3);
    while let Ok(Some(Ok(m))) = tokio::time::timeout_at(window, rx.next()).await {
        if let tungstenite::Message::Binary(b) = m {
            let now = tokio::time::Instant::now();
            acks.send((u32::from_be_bytes(b[1..5].try_into().unwrap()), now)).unwrap();
            arrivals.push(now);
        }
    }
    acker.abort();

    assert!(arrivals.len() >= 120, "{} frames in 3 s at 60 fps", arrivals.len());
    let gap = arrivals.windows(2).map(|w| w[1] - w[0]).max().unwrap();
    assert!(gap < Duration::from_millis(300), "the stream stalled for {gap:?}");
    let lowered: Vec<String> = env.fake.log().into_iter().filter(|l| l.starts_with("encode")).collect();
    assert!(lowered.is_empty(), "a steady link lowered the bitrate: {lowered:?}");
}

#[tokio::test]
async fn a_flapping_link_resumes_at_a_keyframe_and_never_leaves_a_finger_down() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let view = open(&env, &token, &lease, true).await;
    for round in 0..3 {
        let mut ws = view.ws().await;
        format(&mut ws).await;
        let (key, _) = frame(&mut ws).await;
        assert!(key, "round {round}: a new socket starts at a keyframe");
        let y = 0.1 + 0.2 * f64::from(round);
        send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.5, "y": y})).await;
        send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.6, "y": y})).await;
        logged(&env, &format!("touch move 0.60,{y:.2}")).await;
        // The connection drops mid-gesture.
        drop(ws);
        logged(&env, &format!("touch up 0.60,{y:.2}")).await;
    }
    let downs = env.fake.log().iter().filter(|l| l.starts_with("touch down")).count();
    let ups = env.fake.log().iter().filter(|l| l.starts_with("touch up")).count();
    assert_eq!((downs, ups), (3, 3));
}

#[tokio::test]
async fn a_burst_of_input_after_a_hiccup_arrives_whole_and_in_order() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;

    // A link that held the page's messages back delivers them at once.
    let mut burst = vec![json!({"t": "touch", "phase": "down", "x": 0.0, "y": 0.5})];
    burst.extend((1..=300).map(|i| json!({"t": "touch", "phase": "move", "x": f64::from(i) / 300.0, "y": 0.5})));
    burst.push(json!({"t": "touch", "phase": "up", "x": 1.0, "y": 0.5}));
    burst.push(json!({"t": "type", "id": 3, "text": "ok"}));
    for m in burst {
        ws.feed(tungstenite::Message::text(m.to_string())).await.unwrap();
    }
    ws.flush().await.unwrap();

    assert_eq!(text(&mut ws).await, json!({"t": "done", "id": 3}), "no input was refused as busy");
    let log: Vec<String> =
        env.fake.log().into_iter().filter(|l| l.starts_with("touch") || l.starts_with("type")).collect();
    let mut want = vec!["touch down 0.00,0.50".to_string()];
    want.extend((1..=300).map(|i| format!("touch move {:.2},0.50", f64::from(i) / 300.0)));
    want.extend(["touch up 1.00,0.50".to_string(), "type ok".to_string()]);
    assert_eq!(log, want, "every message, in order");
}

/// When each touch reached the device, by its log line.
fn touch_times(env: &Env) -> Vec<(std::time::Instant, String)> {
    env.fake.timed_log().into_iter().filter(|(_, l)| l.starts_with("touch")).collect()
}

fn gaps_ms(times: &[(std::time::Instant, String)]) -> Vec<u128> {
    times.windows(2).map(|w| (w[1].0 - w[0].0).as_millis()).collect()
}

#[tokio::test]
async fn a_gesture_that_arrives_in_a_burst_keeps_its_own_timing() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;

    // A swipe the page drew over 400 ms, delivered all at once by a link that held it back.
    let mut burst = vec![json!({"t": "touch", "phase": "down", "x": 0.5, "y": 0.8, "ts": 1000.0})];
    burst.extend((1..=3).map(|i| json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.8 - 0.1 * f64::from(i), "ts": 1000.0 + 100.0 * f64::from(i)})));
    burst.push(json!({"t": "touch", "phase": "up", "x": 0.5, "y": 0.4, "ts": 1400.0}));
    for m in burst {
        ws.feed(tungstenite::Message::text(m.to_string())).await.unwrap();
    }
    ws.flush().await.unwrap();
    logged(&env, "touch up 0.50,0.40").await;

    let times = touch_times(&env);
    assert_eq!(times.len(), 5, "{times:?}");
    for gap in gaps_ms(&times) {
        assert!((80..250).contains(&gap), "the device got the points {:?} ms apart, not 100", gaps_ms(&times));
    }
}

#[tokio::test]
async fn pacing_sends_a_late_point_at_once_and_holds_none_past_250_ms() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;

    send(&mut ws, json!({"t": "touch", "phase": "down", "x": 0.5, "y": 0.5, "ts": 0.0})).await;
    logged(&env, "touch down 0.50,0.50").await;
    // The page drew this 100 ms after the down; it arrives 400 ms after it.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let sent = std::time::Instant::now();
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.6, "ts": 100.0})).await;
    // Then a point the page says came 5 s later: it waits at most 250 ms.
    send(&mut ws, json!({"t": "touch", "phase": "move", "x": 0.5, "y": 0.7, "ts": 5100.0})).await;
    send(&mut ws, json!({"t": "touch", "phase": "up", "x": 0.5, "y": 0.7, "ts": 5101.0})).await;
    logged(&env, "touch up 0.50,0.70").await;

    let times = touch_times(&env);
    assert_eq!(times.len(), 4, "{times:?}");
    assert!((times[1].0 - sent).as_millis() < 100, "the late point waited {:?}", times[1].0 - sent);
    let held = (times[2].0 - times[1].0).as_millis();
    assert!((200..400).contains(&held), "a far point waited {held} ms");
    assert!((times[3].0 - times[2].0).as_millis() < 100, "the lift kept its 1 ms spacing");
}

#[tokio::test]
async fn a_page_that_closes_on_a_stuck_link_runs_none_of_its_queued_input() {
    let env = heavy_stream().await;
    let (token, lease) = device(&env, "alice").await;
    let mut ws = open(&env, &token, &lease, true).await.ws().await;
    format(&mut ws).await;
    // The page reads nothing, so the server's frame write waits. It queues 50 button presses
    // (about 60 ms each) and then closes.
    for _ in 0..50 {
        ws.feed(tungstenite::Message::text(json!({"t": "button", "name": "home"}).to_string())).await.unwrap();
    }
    ws.feed(tungstenite::Message::Close(None)).await.unwrap();
    ws.flush().await.unwrap();
    let presses = || env.fake.log().iter().filter(|l| *l == "hid button home down").count();

    // The presses that were still queued at the close never run, though the write still waits.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let at_close = presses();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(presses(), at_close, "queued input ran after the page closed");
    assert!(at_close < 50, "all {at_close} presses ran before the close was read");
    drop(ws);
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
    let mut ctrl = open(&env, &token, &lease, true).await.ws().await;
    let mut watcher = open(&env, &token, &lease, false).await.ws().await;

    // Paste: the device clipboard, then Cmd+V into the focused field.
    send(&mut ctrl, json!({"t": "paste", "id": 1, "text": "olá clipboard"})).await;
    assert_eq!(text(&mut ctrl).await, json!({"t": "done", "id": 1, "via": "pasted"}));
    let log = env.fake.log();
    let at = log.iter().position(|l| l.ends_with("pasteboard:olá clipboard")).expect("clipboard set");
    assert_eq!(log[at + 1..at + 5], ["hid key 227 down", "hid key 25 down", "hid key 25 up", "hid key 227 up"]);

    // Without a reachable pasteboard the text is typed, and the page is told so.
    env.fake.no_pasteboard.store(true, Ordering::SeqCst);
    send(&mut ctrl, json!({"t": "paste", "id": 2, "text": "olá"})).await;
    assert_eq!(text(&mut ctrl).await, json!({"t": "done", "id": 2, "via": "typed"}));
    assert!(env.fake.log().iter().any(|l| l == "type olá"), "{:?}", env.fake.log());
    env.fake.no_pasteboard.store(false, Ordering::SeqCst);

    // Paste and type goes through the typing path.
    send(&mut ctrl, json!({"t": "paste_and_type", "id": 3, "text": "typed text"})).await;
    assert_eq!(text(&mut ctrl).await, json!({"t": "done", "id": 3}));
    assert!(env.fake.log().iter().any(|l| l == "type typed text"), "{:?}", env.fake.log());

    // View-only cannot paste.
    send(&mut watcher, json!({"t": "paste", "id": 4, "text": "no"})).await;
    assert_eq!(text(&mut watcher).await["code"], "view_only");

    // Empty or over-limit text fails, and the error names the input.
    send(&mut ctrl, json!({"t": "paste", "id": 5, "text": ""})).await;
    let e = text(&mut ctrl).await;
    assert_eq!((e["code"].as_str(), e["id"].as_u64()), (Some("bad_request"), Some(5)));
    send(&mut ctrl, json!({"t": "paste", "id": 6, "text": "x".repeat(10_001)})).await;
    assert_eq!(text(&mut ctrl).await["code"], "bad_request");
}

#[tokio::test]
async fn presets_and_stats_describe_the_shared_stream() {
    let env = setup_with(2, json!({"viewer": {"fps": 20}})).await;
    let (token, lease) = device(&env, "alice").await;
    let view = open(&env, &token, &lease, true).await;
    let mut ws = view.ws().await;
    frame(&mut ws).await;
    frame(&mut ws).await;

    let stats: Value = view.get("stats").await.json().await.unwrap();
    assert!(stats["stream"]["total_frames"].as_u64().unwrap() > 0, "{stats}");
    assert_eq!(stats["preset"], "balanced");
    assert_eq!(stats["opts"], json!({"fps": 20, "scale": 0.5, "bitrate": 2_500_000}));

    send(&mut ws, json!({"t": "preset", "name": "sharp"})).await;
    let reply = text(&mut ws).await;
    assert_eq!(reply["preset"], "sharp");
    logged(&env, "encode 20 0.8 5000000").await;
    let info: Value = view.get("info").await.json().await.unwrap();
    assert_eq!(info["preset"], "sharp");

    let mut watcher = open(&env, &token, &lease, false).await.ws().await;
    send(&mut watcher, json!({"t": "preset", "name": "smooth"})).await;
    assert_eq!(text(&mut watcher).await["code"], "view_only");

    // Without a session the endpoint fails.
    assert_eq!(http().get(format!("{}stats", view.base)).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn configure_device_sets_pasteboard() {
    let env = setup(1).await;
    let (token, _lease) = device(&env, "alice").await;
    let c = client(&env, &token).await;
    let r = call(&c, "configure_device", json!({"pasteboard": "agent text"})).await;
    assert!(!is_err(&r), "{}", common::text(&r));
    assert!(common::text(&r).contains("pasteboard"), "{}", common::text(&r));
    assert!(env.fake.log().iter().any(|l| l.contains("pasteboard:agent text")), "{:?}", env.fake.log());

    // Empty pasteboard fails.
    let r2 = call(&c, "configure_device", json!({"pasteboard": ""})).await;
    assert!(is_err(&r2));
}
