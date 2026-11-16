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
