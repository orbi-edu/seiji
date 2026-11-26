//! Client side of the CLI: `seiji login`, `seiji logout`, `seiji token print`, `seiji update`.
//!
//! The token lives in the macOS keychain (service `seiji`, account = the server URL); the
//! current server URL and user name live in `~/Library/Application Support/seiji/client.json`.
//! Tern's plugin reads the credential through `seiji token print --json`, never from a file.

use std::borrow::Cow;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        .user_agent(reqwest::header::HeaderValue::from_static(crate::USER_AGENT))
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

/// An error for `--json` output: a code and a message for a person.
type Fail = (&'static str, String);

fn failed(what: &str) -> impl FnOnce(anyhow::Error) -> Fail + '_ {
    move |e| ("failed", format!("{what}: {e:#}"))
}

/// One part `seiji update` looks after: the CLI or the Tern plugin.
#[derive(Serialize)]
struct Part {
    /// The commit it is built from, when known.
    commit: Option<String>,
    /// `current`, `outdated`, `unmanaged` (not where `cargo install` or Tern put it, so left
    /// alone) or `updated`.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl Part {
    fn new(commit: Option<String>, server: &str, managed: Result<(), String>) -> Self {
        let (status, reason) = if commit.as_deref() == Some(server) {
            ("current", None)
        } else {
            match managed {
                Ok(()) => ("outdated", None),
                Err(reason) => ("unmanaged", Some(reason)),
            }
        };
        Part { commit, status, reason }
    }

    fn outdated(&self) -> bool {
        self.status == "outdated"
    }
}

#[derive(Serialize)]
struct Report {
    server: String,
    cli: Part,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugin: Option<Part>,
}

fn home() -> Result<PathBuf, Fail> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or(("failed", "HOME is not set".to_string()))
}

fn cargo_home() -> Result<PathBuf, Fail> {
    match std::env::var_os("CARGO_HOME") {
        Some(dir) if !dir.is_empty() => Ok(PathBuf::from(dir)),
        _ => Ok(home()?.join(".cargo")),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// The CLI is managed when it runs from `$CARGO_HOME/bin/seiji`, where `cargo install` puts it.
/// A build in a clone (`target/…`) is left alone.
fn cli_managed() -> Result<(), String> {
    let installed = cargo_home().map_err(|(_, m)| m)?.join("bin/seiji");
    let exe = std::env::current_exe().map_err(|e| format!("this CLI's path: {e}"))?;
    if same_file(&exe, &installed) {
        Ok(())
    } else {
        Err(format!("{} is not {}, where cargo install puts it", exe.display(), installed.display()))
    }
}

/// Tern: `TERN_EXE` (set in Tern's processes), the app bundle, or `tern` on `PATH`.
fn tern_bin() -> PathBuf {
    match std::env::var_os("TERN_EXE") {
        Some(exe) if !exe.is_empty() => PathBuf::from(exe),
        _ => {
            let app = PathBuf::from("/Applications/Tern.app/Contents/MacOS/tern");
            if app.exists() { app } else { PathBuf::from("tern") }
        }
    }
}

/// The plugin is managed when it sits in Tern's plugins directory. A linked copy (`tern plugin
/// link`) is read where it is, in a clone, and is left alone.
fn plugin_managed(dir: &Path) -> Result<(), String> {
    let out = Command::new(tern_bin())
        .args(["plugin", "dir"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("tern plugin dir: {e}"))?;
    let plugins = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if !out.status.success() || plugins.as_os_str().is_empty() {
        return Err("tern plugin dir failed".to_string());
    }
    match dir.parent() {
        Some(parent) if same_file(parent, &plugins) => Ok(()),
        _ => Err(format!("{} is not in Tern's plugins directory {}", dir.display(), plugins.display())),
    }
}

/// The commit the plugin was installed from: the `commit` file `seiji update` writes into it.
fn plugin_commit(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("commit")).ok()?;
    let commit = text.trim();
    (!commit.is_empty()).then(|| commit.to_string())
}

async fn server_commit(c: &Credential) -> Result<Option<String>> {
    let resp = reqwest::Client::builder()
        .user_agent(reqwest::header::HeaderValue::from_static(crate::USER_AGENT))
        .timeout(std::time::Duration::from_secs(15))
        .build()?
        .get(format!("{}/v1/me", c.url))
        .bearer_auth(&c.token)
        .send()
        .await
        .with_context(|| format!("reaching {}", c.url))?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("the server refused the token");
    }
    let doc: serde_json::Value = serde_json::from_slice(&resp.error_for_status()?.bytes().await?)?;
    Ok(doc["server"]["commit"].as_str().map(str::to_string))
}

/// Runs a step with its output on stderr, so `--json` keeps stdout to one object.
fn step(what: &str, cmd: &mut Command) -> Result<(), Fail> {
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::io::stderr()))
        .status()
        .map_err(|e| ("failed", format!("{what}: {e}")))?;
    if status.success() { Ok(()) } else { Err(("failed", format!("{what} failed ({status})"))) }
}

/// Puts the repository at `commit` in `<cache>/source`, a shallow checkout kept between updates.
fn fetch_source(cache: &Path, commit: &str) -> Result<PathBuf, Fail> {
    let src = cache.join("source");
    if !src.join(".git").exists() {
        std::fs::create_dir_all(&src).map_err(|e| failed("creating the source directory")(e.into()))?;
        step("git init", Command::new("git").arg("init").arg("-q").arg(&src))?;
    }
    let git = || {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&src);
        cmd
    };
    let repo = env!("CARGO_PKG_REPOSITORY");
    step(&format!("git fetch {repo} {commit}"), git().args(["fetch", "-q", "--depth", "1", repo, commit]))?;
    step("git checkout", git().args(["checkout", "-q", "--force", "--detach", "FETCH_HEAD"]))?;
    step("git clean", git().args(["clean", "-q", "-f", "-d", "-x"]))?;
    Ok(src)
}

async fn run_update(check: bool, tern_plugin: Option<&Path>) -> Result<Report, Fail> {
    let c = credential()?;
    let server = server_commit(&c).await.map_err(|e| ("unreachable", format!("{e:#}")))?.ok_or((
        "no_commit",
        "The server does not report its commit: it runs a Seiji older than `seiji update`.".to_string(),
    ))?;
    let mut cli = Part::new(crate::COMMIT.map(str::to_string), &server, cli_managed());
    let mut plugin = tern_plugin.map(|dir| Part::new(plugin_commit(dir), &server, plugin_managed(dir)));
    if check || !(cli.outdated() || plugin.as_ref().is_some_and(Part::outdated)) {
        return Ok(Report { server, cli, plugin });
    }

    let cache = home()?.join("Library/Caches/seiji");
    std::fs::create_dir_all(&cache).map_err(|e| failed("creating the cache directory")(e.into()))?;
    let lock = std::fs::File::create(cache.join("update.lock")).map_err(|e| failed("update.lock")(e.into()))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(("busy", "Another seiji update is running.".to_string())),
        Err(std::fs::TryLockError::Error(e)) => return Err(failed("locking update.lock")(e.into())),
    }
    let src = fetch_source(&cache, &server)?;

    // The CLI first: installing the plugin reloads Tern's plugins, which may end this process
    // when the plugin started it.
    if cli.outdated() {
        let cargo_home = cargo_home()?;
        let cargo = cargo_home.join("bin/cargo");
        let cargo = if cargo.exists() { cargo } else { PathBuf::from("cargo") };
        // Tern started from the Dock has no ~/.cargo/bin on PATH; cargo needs rustc from there.
        let path = std::env::join_paths(
            std::iter::once(cargo_home.join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .map_err(|e| failed("PATH")(e.into()))?;
        step(
            "cargo install",
            Command::new(cargo)
                .env("PATH", path)
                .args(["install", "--locked", "--force", "--path"])
                .arg(&src)
                .arg("--root")
                .arg(&cargo_home)
                .arg("--target-dir")
                .arg(cache.join("target")),
        )?;
        cli = Part { commit: Some(server.clone()), status: "updated", reason: None };
    }
    if let Some(part) = plugin.as_mut()
        && part.outdated()
    {
        let package = src.join("tern-plugin");
        std::fs::write(package.join("commit"), format!("{server}\n"))
            .map_err(|e| failed("writing the plugin's commit")(e.into()))?;
        step("tern plugin install", Command::new(tern_bin()).args(["plugin", "install", "--force"]).arg(&package))?;
        *part = Part { commit: Some(server.clone()), status: "updated", reason: None };
    }
    Ok(Report { server, cli, plugin })
}

/// `seiji update`: brings this CLI (when `cargo install` put it in `$CARGO_HOME/bin`) and, with
/// `--tern-plugin`, the plugin in Tern's plugins directory to the commit the server runs.
pub async fn update(check: bool, json: bool, tern_plugin: Option<&Path>) -> Result<()> {
    match run_update(check, tern_plugin).await {
        Ok(report) if json => println!("{}", serde_json::to_string(&report)?),
        Ok(report) => {
            let short = |c: &Option<String>| c.as_deref().map_or("unknown", |c| &c[..c.len().min(10)]).to_string();
            println!("server  {}", &report.server[..report.server.len().min(10)]);
            for (name, part) in [("cli", Some(&report.cli)), ("plugin", report.plugin.as_ref())] {
                if let Some(p) = part {
                    let reason = p.reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
                    println!("{name:<7} {} {}{reason}", short(&p.commit), p.status);
                }
            }
        }
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

/// Extracts the JSON-RPC response matching `id` from an SSE event stream body.
/// Ignores comments, priming events, notifications and unrelated ids.  Rejects
/// malformed JSON events and streams not terminated by a blank line.
fn extract_sse_response(body: &[u8], id: &serde_json::Value) -> Result<serde_json::Value> {
    let text = std::str::from_utf8(body).context("SSE response is not UTF-8")?;
    let bytes = text.as_bytes();
    let mut data: Cow<'_, str> = Cow::Borrowed("");
    let mut has_data = false;
    let mut pos = 0;
    while pos < bytes.len() {
        // Scan to end-of-line content (stops at CR or LF).
        let start = pos;
        while pos < bytes.len() && bytes[pos] != b'\r' && bytes[pos] != b'\n' {
            pos += 1;
        }
        let line = &text[start..pos];
        // Consume the line ending: CR, LF, or CRLF.
        if pos < bytes.len() && bytes[pos] == b'\r' {
            pos += 1;
        }
        if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }
        if line.is_empty() {
            // Blank line: SSE event boundary.
            if has_data {
                if data.trim().is_empty() {
                    // Priming event (empty data); skip.
                } else {
                    let doc: serde_json::Value = serde_json::from_str(&data).context("malformed JSON in SSE event")?;
                    // Accept only JSON-RPC responses (result or error), not server requests.
                    if doc.get("id") == Some(id)
                        && doc.get("method").is_none()
                        && (doc.get("result").is_some() || doc.get("error").is_some())
                    {
                        return Ok(doc);
                    }
                }
                data = Cow::Borrowed("");
                has_data = false;
            }
        } else if line.starts_with(':') {
            // SSE comment (keep-alive heartbeat); skip.
        } else if let Some(value) = line.strip_prefix("data:") {
            let value = value.strip_prefix(' ').unwrap_or(value);
            if has_data {
                // Multiline data: allocate only here.
                let buf = data.to_mut();
                buf.push('\n');
                buf.push_str(value);
            } else {
                data = Cow::Borrowed(value);
                has_data = true;
            }
        }
        // event:, id:, retry: fields are not needed; skip.
    }
    if has_data {
        bail!("SSE stream truncated: event not terminated by a blank line");
    }
    bail!("no JSON-RPC response with the expected id in the SSE stream");
}

/// Calls MCP tool `name` and returns its text, decoded as JSON when it is JSON.
async fn tool(c: &Credential, name: &str, args: serde_json::Value) -> Result<serde_json::Value> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": args },
    });
    let resp = reqwest::Client::builder()
        .user_agent(reqwest::header::HeaderValue::from_static(crate::USER_AGENT))
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
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    let bytes = resp.error_for_status()?.bytes().await?;
    let doc =
        if is_sse { extract_sse_response(&bytes, &serde_json::json!(1))? } else { serde_json::from_slice(&bytes)? };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Mixed CRLF/CR/LF, comments, priming, notification, wrong id, server request with
    /// our id, and a multiline matching result: everything a real stream can contain.
    #[test]
    fn sse_extracts_result_through_noise_and_mixed_line_endings() {
        let body = concat!(
            "retry: 3000\r\ndata: \r\n\r\n", // priming (CRLF)
            ": keep-alive\r\r",              // comment (bare CR)
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"sampling/createMessage\"}\n\n",
            "data:{\"jsonrpc\":\"2.0\",\r\ndata: \"id\":1,\"result\":{\"found\":true}}\r\n\r\n",
        );
        let doc = extract_sse_response(body.as_bytes(), &json!(1)).unwrap();
        assert_eq!(doc["result"]["found"], true);
    }

    #[test]
    fn sse_extracts_error_response() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32600,\"message\":\"bad\"}}\n\n";
        let doc = extract_sse_response(body, &json!(1)).unwrap();
        assert_eq!(doc["error"]["message"], "bad");
    }

    #[test]
    fn sse_rejects_malformed_json_event() {
        let body = b"data: {not valid json}\n\n";
        let err = extract_sse_response(body, &json!(1)).unwrap_err();
        assert!(err.to_string().contains("malformed"), "{err}");
    }

    #[test]
    fn sse_rejects_truncated_stream() {
        // Data line present but no terminating blank line.
        let body = b": keep-alive\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}";
        let err = extract_sse_response(body, &json!(1)).unwrap_err();
        assert!(err.to_string().contains("truncated"), "{err}");
    }

    #[test]
    fn sse_fails_when_no_response_matches() {
        let body = b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n";
        let err = extract_sse_response(body, &json!(1)).unwrap_err();
        assert!(err.to_string().contains("no JSON-RPC response"), "{err}");
    }
}
