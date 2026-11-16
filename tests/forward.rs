//! `seiji forward`: the tunnel from a lease's host loopback to a port on the developer's machine.

mod common;

use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use common::*;

/// A stand-in for Metro on the developer's machine: echoes every connection back.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
                let _ = w.shutdown().await;
            });
        }
    });
    port
}

/// A stand-in for one app's Metro: answers every connection with its name, then the bytes.
async fn named_server(name: &'static str) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut got = Vec::new();
                let _ = s.read_to_end(&mut got).await;
                let _ = s.write_all(format!("{name}:").as_bytes()).await;
                let _ = s.write_all(&got).await;
                let _ = s.shutdown().await;
            });
        }
    });
    port
}

/// A free local port with nothing listening on it.
async fn dead_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0)).await.unwrap().local_addr().unwrap().port()
}

async fn device(env: &Env, user: &str) -> (String, String) {
    let token = add_user(env, user, QUOTA);
    let c = client(env, &token).await;
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    (token, lease["lease_id"].as_str().unwrap().to_string())
}

fn ws_url(env: &Env, lease: &str) -> String {
    format!("{}/v1/leases/{lease}/forward", env.url.replacen("http://", "ws://", 1))
}

/// Opens a tunnel; returns the device address, the stop token and the client task.
async fn tunnel(
    env: &Env,
    token: &str,
    lease: &str,
    local: u16,
) -> (String, CancellationToken, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let stop = CancellationToken::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (url, token, s) = (ws_url(env, lease), token.to_string(), stop.clone());
    let task = tokio::spawn(async move {
        seiji::forward::connect(&url, &token, local, s, move |h| {
            let _ = tx.send(h.device_address.clone());
        })
        .await
    });
    let address = tokio::time::timeout(Duration::from_secs(5), rx).await.expect("hello in time").expect("hello");
    (address, stop, task)
}

async fn round_trip(address: &str, payload: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(address).await.unwrap();
    s.write_all(payload).await.unwrap();
    s.shutdown().await.unwrap();
    let mut back = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut back)).await.unwrap().unwrap();
    back
}

#[tokio::test(flavor = "multi_thread")]
async fn the_device_reaches_a_local_port_until_the_lease_ends() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "ana").await;
    let local = echo_server().await;
    let (address, _stop, task) = tunnel(&env, &token, &lease, local).await;
    assert!(address.starts_with("127.0.0.1:"), "{address}");

    // Several connections at once, each with its own bytes, one larger than a frame.
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let (a, b, c) = tokio::join!(
        round_trip(&address, b"GET /index.bundle"),
        round_trip(&address, &big),
        round_trip(&address, b"hmr")
    );
    assert_eq!(a, b"GET /index.bundle");
    assert_eq!(b, big);
    assert_eq!(c, b"hmr");

    // Releasing the device closes the tunnel and its port.
    let c = client(&env, &token).await;
    assert!(!is_err(&call(&c, "release_device", json!({"lease_id": lease})).await));
    tokio::time::timeout(Duration::from_secs(5), task).await.expect("the client returns").unwrap().unwrap();
    assert!(TcpStream::connect(&address).await.is_err(), "the port should be closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_closes_when_nothing_listens_locally() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "ana").await;
    let (address, stop, task) = tunnel(&env, &token, &lease, dead_port().await).await;
    let mut s = TcpStream::connect(&address).await.unwrap();
    let mut buf = [0u8; 8];
    let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut buf)).await.unwrap().unwrap_or(0);
    assert_eq!(n, 0, "the device sees the connection close");
    stop.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnels_are_refused_for_other_leases_past_the_limit_and_when_off() {
    let env = setup_with(3, json!({"forward": {"maxPerLease": 1}})).await;
    let (token, lease) = device(&env, "ana").await;
    let (bob, _) = device(&env, "bob").await;
    let local = echo_server().await;
    let refused = |url: String, token: String| async move {
        let r = seiji::forward::connect(&url, &token, local, CancellationToken::new(), |_| {}).await;
        format!("{:#}", r.expect_err("refused"))
    };

    let theirs = refused(ws_url(&env, &lease), bob.clone()).await;
    assert!(theirs.contains("404"), "{theirs}");

    let (_address, stop, task) = tunnel(&env, &token, &lease, local).await;
    let full = refused(ws_url(&env, &lease), token.clone()).await;
    assert!(full.contains("429") && full.contains("1 tunnels"), "{full}");
    stop.cancel();
    task.await.unwrap().unwrap();

    let off = setup_with(1, json!({"forward": {"enabled": false}})).await;
    let (token, lease) = device(&off, "ana").await;
    let refused_off = refused(ws_url(&off, &lease), token).await;
    assert!(refused_off.contains("403"), "{refused_off}");
}

/// Runs `seiji forward` until it has pointed the app, then stops it with Ctrl-C.
async fn cli_forward(env: &Env, token: &str, args: &[&str], wait_for: impl Fn(&[String]) -> bool) -> String {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_seiji"))
        .arg("forward")
        .args(args)
        .env("SEIJI_URL", &env.url)
        .env("SEIJI_TOKEN", token)
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !wait_for(&env.fake.log()) {
        assert!(tokio::time::Instant::now() < deadline, "the app was never pointed; log: {:?}", env.fake.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    unsafe { libc::kill(child.id().unwrap() as i32, libc::SIGINT) };
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), child.wait()).await.unwrap().unwrap().success());
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn each_app_gets_its_own_tunnel_to_its_own_metro() {
    let env = setup(2).await;
    let (token, lease) = device(&env, "ana").await;
    let (shop, admin) = (named_server("shop").await, named_server("admin").await);
    let (a, _stop_a, _ta) = tunnel(&env, &token, &lease, shop).await;
    let (b, _stop_b, _tb) = tunnel(&env, &token, &lease, admin).await;
    assert_ne!(a, b, "two tunnels, two device addresses");
    let (from_a, from_b) = tokio::join!(round_trip(&a, b"/index.bundle"), round_trip(&b, b"/index.bundle"));
    assert_eq!(from_a, b"shop:/index.bundle");
    assert_eq!(from_b, b"admin:/index.bundle");

    // Each React Native app is pointed at its own tunnel.
    let (p_shop, p_admin) = (shop.to_string(), admin.to_string());
    let shop_args = [p_shop.as_str(), "--react-native", "com.example.shop"];
    let admin_args = [p_admin.as_str(), "--react-native", "com.example.admin"];
    let launched = |bundle: &'static str| {
        move |log: &[String]| log.iter().any(|l| l.starts_with(&format!("op launch_app {bundle}")))
    };
    tokio::join!(
        cli_forward(&env, &token, &shop_args, launched("com.example.shop")),
        cli_forward(&env, &token, &admin_args, launched("com.example.admin")),
    );
    let location = |bundle: &str| {
        let log = env.fake.log();
        let line = log.iter().find(|l| l.contains("react_native_bundler") && l.contains(bundle)).cloned().unwrap();
        line.split("127.0.0.1:").nth(1).unwrap().chars().take_while(char::is_ascii_digit).collect::<String>()
    };
    assert_ne!(location("com.example.shop"), location("com.example.admin"), "{:?}", env.fake.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_finds_the_lease_and_points_the_app_at_the_tunnel() {
    let env = setup(2).await;
    let (token, _lease) = device(&env, "ana").await;
    let local = echo_server().await.to_string();

    let out =
        cli_forward(&env, &token, &[&local, "--expo-go"], |log| log.iter().any(|l| l.contains("exp://127.0.0.1:")))
            .await;
    assert!(out.contains("the device reaches localhost:") && out.contains("opened exp://127.0.0.1:"), "{out}");

    let out = cli_forward(&env, &token, &[&local, "--dev-client", "myapp"], |log| {
        log.iter().any(|l| l.contains("myapp://expo-development-client/?url=http%3A%2F%2F127.0.0.1%3A"))
    })
    .await;
    assert!(out.contains("opened myapp://"), "{out}");

    let out = cli_forward(&env, &token, &[&local, "--react-native", "com.example.demo"], |log| {
        log.iter().any(|l| l.starts_with("op launch_app com.example.demo"))
    })
    .await;
    assert!(out.contains("set RCT_jsLocation to 127.0.0.1:"), "{out}");
    let log = env.fake.log();
    let set = log
        .iter()
        .position(|l| l.contains("react_native_bundler") && l.contains("com.example.demo"))
        .expect("bundler set");
    let launched = log.iter().position(|l| l.starts_with("op launch_app com.example.demo")).unwrap();
    assert!(set < launched, "the bundler is set before the relaunch: {log:?}");
}
