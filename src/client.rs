//! Client side of the CLI: `seiji login`, `seiji logout`, `seiji token print`.
//!
//! The token lives in the macOS keychain (service `seiji`, account = the server URL); the
//! current server URL and user name live in `~/Library/Application Support/seiji/client.json`.
//! Tern's plugin reads the credential through `seiji token print --json`, never from a file.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const SERVICE: &str = "seiji";

#[derive(Debug, Serialize, Deserialize)]
struct ClientFile {
    url: String,
    user: Option<String>,
}

fn client_file() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/seiji/client.json"))
}

fn normalize(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid url {url:?}"))?;
    if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
        bail!("the url must be http(s)://host[:port]");
    }
    Ok(url.to_string())
}

/// Reads a token from stdin; on a terminal, prompts without echo.
fn read_token() -> Result<String> {
    let stdin = std::io::stdin();
    let token = if stdin.is_terminal() {
        eprint!("Token: ");
        std::io::stderr().flush()?;
        let fd = libc::STDIN_FILENO;
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        let saved = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
        if saved {
            let mut quiet = term;
            quiet.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
        }
        let mut line = String::new();
        let read = stdin.read_line(&mut line);
        if saved {
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
        }
        eprintln!();
        read?;
        line
    } else {
        let mut all = String::new();
        stdin.lock().take(64 * 1024).read_to_string(&mut all)?;
        all
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("no token given");
    }
    Ok(token)
}

async fn whoami(url: &str, token: &str) -> Result<String> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(format!("{url}/v1/me"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("reaching {url}"))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the server refused the token");
    }
    let doc: serde_json::Value = serde_json::from_slice(&resp.error_for_status()?.bytes().await?)?;
    Ok(doc["user"].as_str().unwrap_or("?").to_string())
}

pub async fn login(url: &str) -> Result<()> {
    let url = normalize(url)?;
    let token = read_token()?;
    let user = whoami(&url, &token).await?;
    security_framework::passwords::set_generic_password(SERVICE, &url, token.as_bytes())
        .context("storing the token in the keychain")?;
    let path = client_file()?;
    std::fs::create_dir_all(path.parent().expect("has a parent"))?;
    std::fs::write(&path, serde_json::to_vec_pretty(&ClientFile { url: url.clone(), user: Some(user.clone()) })?)?;
    println!("signed in to {url} as {user}");
    Ok(())
}

pub fn logout() -> Result<()> {
    let path = client_file()?;
    let Ok(bytes) = std::fs::read(&path) else {
        println!("not signed in");
        return Ok(());
    };
    let file: ClientFile = serde_json::from_slice(&bytes).context("reading client.json")?;
    let _ = security_framework::passwords::delete_generic_password(SERVICE, &file.url);
    std::fs::remove_file(&path)?;
    println!("signed out of {}", file.url);
    Ok(())
}

#[derive(Serialize)]
struct Credential {
    url: String,
    token: String,
    user: Option<String>,
}

fn credential() -> std::result::Result<Credential, (&'static str, String)> {
    if let (Ok(url), Ok(token)) = (std::env::var("SEIJI_URL"), std::env::var("SEIJI_TOKEN"))
        && !url.is_empty()
        && !token.is_empty()
    {
        return Ok(Credential { url: url.trim_end_matches('/').to_string(), token, user: None });
    }
    let not_in = || ("not_logged_in", "You are not signed in. Run `seiji login --url <server>`.".to_string());
    let path = client_file().map_err(|e| ("error", format!("{e:#}")))?;
    let bytes = std::fs::read(&path).map_err(|_| not_in())?;
    let file: ClientFile = serde_json::from_slice(&bytes).map_err(|e| ("error", format!("client.json: {e}")))?;
    let token = security_framework::passwords::get_generic_password(SERVICE, &file.url).map_err(|_| not_in())?;
    let token = String::from_utf8(token).map_err(|_| ("error", "the stored token is not text".to_string()))?;
    Ok(Credential { url: file.url, token, user: file.user })
}

/// Prints the credential (`--json`: `{"url","token","user"}`, or `{"error":{"code","message"}}`
/// and exit 1). `SEIJI_URL` and `SEIJI_TOKEN` override the keychain.
pub fn token_print(json: bool) -> Result<()> {
    match credential() {
        Ok(c) if json => println!("{}", serde_json::to_string(&c)?),
        Ok(c) => println!("{}", c.token),
        Err((code, message)) => {
            if json {
                println!("{}", serde_json::json!({ "error": { "code": code, "message": message } }));
                std::process::exit(1);
            }
            bail!("{message}");
        }
    }
    Ok(())
}

/// Calls MCP tool `name` and returns its text, decoded as JSON when it is JSON.
async fn tool(c: &Credential, name: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?
        .post(format!("{}/mcp", c.url))
        .bearer_auth(&c.token)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", "2025-11-25")
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&body)?)
        .send()
        .await
        .with_context(|| format!("reaching {}", c.url))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the server refused the token; run `seiji login --url {}`", c.url);
    }
    let doc: serde_json::Value = serde_json::from_slice(&resp.error_for_status()?.bytes().await?)?;
    if let Some(e) = doc.get("error") {
        bail!("{name}: {}", e["message"].as_str().unwrap_or("MCP error"));
    }
    let text = doc["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string();
    if doc["result"]["isError"].as_bool() == Some(true) {
        bail!("{name}: {text}");
    }
    Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
}

/// What `seiji forward` points the app at once the tunnel is up.
#[derive(Debug, Clone)]
pub enum Point {
    /// `exp://<address>` in Expo Go.
    ExpoGo,
    /// An Expo development build with this URL scheme.
    DevClient(String),
    /// A React Native debug build (`RCT_jsLocation`), relaunched; `None`: the app installed last.
    ReactNative(Option<String>),
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The deep link that opens `address` in Expo Go or a development build.
pub fn expo_url(point: &Point, address: &str) -> Option<String> {
    match point {
        Point::ExpoGo => Some(format!("exp://{address}")),
        Point::DevClient(scheme) => {
            Some(format!("{scheme}://expo-development-client/?url={}", percent_encode(&format!("http://{address}"))))
        }
        Point::ReactNative(_) => None,
    }
}

async fn point_app(c: &Credential, lease: &str, point: &Point, address: &str) -> Result<String> {
    if let Some(url) = expo_url(point, address) {
        tool(c, "open_url", serde_json::json!({ "lease_id": lease, "url": url })).await?;
        return Ok(format!("opened {url}"));
    }
    let Point::ReactNative(bundle_id) = point else { unreachable!() };
    tool(
        c,
        "configure_device",
        serde_json::json!({ "lease_id": lease,
            "react_native_bundler": { "bundle_id": bundle_id, "location": address } }),
    )
    .await?;
    tool(c, "launch_app", serde_json::json!({ "lease_id": lease, "bundle_id": bundle_id, "restart": true })).await?;
    Ok(format!("set RCT_jsLocation to {address} and relaunched the app"))
}

/// `seiji forward [PORT]`: carries connections the device makes to its tunnel address to
/// `localhost:PORT` here, until Ctrl-C or the lease ends.
pub async fn forward(port: u16, lease: Option<String>, point: Option<Point>) -> Result<()> {
    let c = credential().map_err(|(_, message)| anyhow::anyhow!(message))?;
    let lease = match lease {
        Some(l) => l,
        None => {
            let budget = tool(&c, "budget_status", serde_json::json!({})).await?;
            let ready: Vec<String> = budget["active_leases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|l| l["status"] == "ready")
                .filter_map(|l| l["lease_id"].as_str().map(String::from))
                .collect();
            match ready.as_slice() {
                [one] => one.clone(),
                [] => bail!("you hold no ready device; acquire one first (acquire_device, or n in Tern)"),
                many => bail!("you hold {} devices; pick one with --lease ({})", many.len(), many.join(", ")),
            }
        }
    };
    let ws = match c.url.split_once("://") {
        Some(("https", rest)) => format!("wss://{rest}"),
        Some(("http", rest)) => format!("ws://{rest}"),
        _ => bail!("the server URL must be http(s): {}", c.url),
    };
    let ws = format!("{ws}/v1/leases/{lease}/forward");

    let stop = tokio_util::sync::CancellationToken::new();
    let on_signal = stop.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        on_signal.cancel();
    });
    let (hello_tx, hello_rx) = tokio::sync::oneshot::channel();
    let tunnel = crate::forward::connect(&ws, &c.token, port, stop.clone(), move |h| {
        let _ = hello_tx.send(h.clone());
    });
    let announce = async {
        let Ok(h) = hello_rx.await else { return };
        let a = &h.device_address;
        println!("Forwarding {lease}: the device reaches localhost:{port} at {a}");
        println!("  Expo Go:      exp://{a}");
        println!("  Dev build:    <scheme>://expo-development-client/?url=http%3A%2F%2F{}", a.replace(':', "%3A"));
        println!("  React Native: seiji forward {port} --react-native [BUNDLE_ID]");
        println!("Ctrl-C stops.");
        if let Some(point) = &point {
            match point_app(&c, &lease, point, a).await {
                Ok(done) => println!("{done}"),
                Err(e) => eprintln!("seiji forward: could not point the app: {e:#}"),
            }
        }
    };
    let (result, ()) = tokio::join!(tunnel, announce);
    result?;
    if !stop.is_cancelled() {
        println!("the tunnel closed (the lease ended, or the server stopped)");
    }
    Ok(())
}
