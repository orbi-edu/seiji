//! Evidence of a run: files copied out of the tenant's run dir, a manifest with a SHA-256 for
//! every file, and a Markdown summary for pull requests.
//!
//! Layout: `<dataDir>/runs/<user id>/<run id>/` holds `manifest.json` and the run's files under
//! stable relative paths (`steps/…`, `video.mp4`, `app.log`, `crashes/…`, `xctest/…`, `maestro/…`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Platform};
use crate::exec::{Account, CmdSpec, Runner};

pub const MANIFEST: &str = "manifest.json";
pub const SCHEMA: &str = "seiji.run.v1";
const COPY_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_DEPTH: usize = 8;

pub fn run_dir(config: &Config, user_id: i64, run_id: &str) -> PathBuf {
    config.data_dir.join("runs").join(user_id.to_string()).join(run_id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub platform: Platform,
    pub model: String,
    pub udid: Option<String>,
    pub device_type: Option<String>,
    pub runtime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub run_id: String,
    /// `actions`, `flow` or `xctest`.
    pub kind: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    /// `passed`, `failed`, `error` or `cancelled`.
    pub status: String,
    /// One line: what happened.
    pub verdict: String,
    pub user: String,
    pub lease_id: String,
    pub device: DeviceInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<serde_json::Value>,
    pub started_at: i64,
    pub ended_at: i64,
    pub seiji_version: String,
    /// What the run executed: actions, flow YAML or XCTest options.
    pub input: serde_json::Value,
    /// Per-step records (`run_actions`), per-command records (`run_flow`) or per-test results (`run_xctest`).
    pub steps: serde_json::Value,
    /// Files in the evidence, with their SHA-256. `manifest.json` itself is not listed.
    pub files: Vec<FileEntry>,
    /// Files that were not kept because the run hit `evidence.maxRunBytes` or the user's quota.
    pub dropped: Vec<String>,
    /// Problems while collecting evidence that did not fail the run.
    pub warnings: Vec<String>,
}

/// One path component, made safe for the daemon's filesystem.
fn clean_component(c: &str) -> Option<String> {
    if c.is_empty() || c == "." || c == ".." {
        return None;
    }
    let cleaned: String = c
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') { ch } else { '_' })
        .take(128)
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A relative path from the tenant, cleaned component by component.
pub fn clean_relative(rel: &str) -> Option<String> {
    let parts: Vec<String> = rel.split('/').map(clean_component).collect::<Option<_>>()?;
    (!parts.is_empty() && parts.len() <= MAX_DEPTH).then(|| parts.join("/"))
}

pub struct CopyReport {
    pub bytes: u64,
    pub dropped: Vec<String>,
    pub warnings: Vec<String>,
}

/// Copies every regular file under `tenant_dir` into `daemon_dir`, reading each one as the
/// tenant (so a symlink can only reach what the tenant can read anyway). Stops at `budget` bytes.
pub async fn copy_out(
    runner: &Runner,
    account: &Account,
    tenant_dir: &Path,
    daemon_dir: &Path,
    budget: u64,
) -> Result<CopyReport> {
    let mut report = CopyReport { bytes: 0, dropped: Vec::new(), warnings: Vec::new() };
    let list = CmdSpec::new("/usr/bin/find").arg(tenant_dir).args(["-type", "f", "-print0"]);
    let out = runner.run(account, list, Duration::from_secs(60), &CancellationToken::new()).await?;
    if !out.success() {
        // An empty or missing run dir is not an error; anything else is reported.
        report.warnings.push(format!("listing the run's files: {}", crate::exec::tail(&out.text(), 500)));
        return Ok(report);
    }
    let prefix = format!("{}/", tenant_dir.to_string_lossy().trim_end_matches('/'));
    let mut paths: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| String::from_utf8(p.to_vec()).ok())
        .collect();
    paths.sort();
    for full in paths {
        let Some(rel) = full.strip_prefix(&prefix) else { continue };
        if rel == ".started" {
            continue;
        }
        let Some(clean) = clean_relative(rel) else {
            report.warnings.push(format!("skipped a file with an unusable name: {rel:?}"));
            continue;
        };
        let room = budget.saturating_sub(report.bytes);
        if room == 0 {
            report.dropped.push(clean);
            continue;
        }
        let dest = daemon_dir.join(&clean);
        // Seiji's own files (app.log, trees, outputs) and an earlier name that cleaned the same
        // way keep their place; the tenant cannot replace them.
        if dest.exists() {
            report.warnings.push(format!("skipped {rel:?}: {clean} already exists in the evidence"));
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let copy = runner.run_to_file(account, CmdSpec::new("/bin/cat").arg(&full), &dest, room, COPY_TIMEOUT).await;
        match copy {
            Ok(c) if c.truncated => {
                let _ = std::fs::remove_file(&dest);
                report.dropped.push(clean);
            }
            Ok(c) if c.code == Some(0) => report.bytes += c.bytes,
            Ok(c) => {
                let _ = std::fs::remove_file(&dest);
                report.warnings.push(format!("copying {clean}: {}", c.stderr.trim()));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                report.warnings.push(format!("copying {clean}: {e:#}"));
            }
        }
    }
    Ok(report)
}

/// The last `max` bytes of a file as text, without reading the rest.
pub fn read_tail(path: &Path, max: u64) -> Result<String> {
    use std::io::{Seek, SeekFrom};
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::with_capacity(max.min(len) as usize);
    file.take(max).read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((bytes, hex::encode(hasher.finalize())))
}

/// Every regular file under `dir` except the manifest, with sizes and hashes, sorted by path.
pub fn hash_tree(dir: &Path) -> Result<Vec<FileEntry>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<FileEntry>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            if ty.is_dir() {
                walk(root, &path, out)?;
            } else if ty.is_file() {
                let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
                if rel == MANIFEST {
                    continue;
                }
                let (bytes, sha256) = sha256_file(&path)?;
                out.push(FileEntry { path: rel, bytes, sha256 });
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub fn write_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let tmp = dir.join(".manifest.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(manifest)?)?;
    std::fs::rename(&tmp, dir.join(MANIFEST))?;
    Ok(())
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    let bytes = std::fs::read(dir.join(MANIFEST)).context("the run has no manifest yet")?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The manifest's file entry for `rel`, so only listed files are ever served.
pub fn listed<'a>(manifest: &'a Manifest, rel: &str) -> Result<&'a FileEntry> {
    manifest.files.iter().find(|f| f.path == rel).with_context(|| format!("the run has no file {rel:?}"))
}

pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default().to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "mp4" => "video/mp4",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "yaml" | "yml" => "application/yaml",
        "txt" | "log" | "ips" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn status_mark(status: &str) -> &'static str {
    match status {
        "passed" => "✅",
        "failed" => "❌",
        "cancelled" => "⏹️",
        _ => "⚠️",
    }
}

fn step_line(step: &serde_json::Value) -> Option<String> {
    let index = step.get("index").and_then(|v| v.as_u64())?;
    let status = step.get("status").and_then(|v| v.as_str()).unwrap_or("?");
    let what = match step.get("action") {
        Some(a) => serde_json::to_string(a).ok()?,
        None => step.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
    };
    let what = what.replace('|', "\\|");
    let what = if what.len() > 120 { format!("{}…", crate::exec::head(&what, 117)) } else { what };
    let ms = step.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
    let error = step.get("error").and_then(|v| v.as_str()).unwrap_or("").replace('|', "\\|").replace('\n', " ");
    Some(format!(
        "| {} | {} `{what}` | {} | {:.1}s | {error} |",
        index + 1,
        status_mark(status),
        status,
        ms as f64 / 1000.0
    ))
}

/// A summary to paste into a pull request. `base` prefixes the file links.
pub fn markdown(manifest: &Manifest, base: &str, manifest_sha256: &str) -> String {
    let mut s = String::new();
    let title = manifest.name.clone().unwrap_or_else(|| format!("{} run", manifest.kind));
    s.push_str(&format!("### {} {title}\n\n", status_mark(&manifest.status)));
    s.push_str(&format!("**{}**: {}\n\n", manifest.status, manifest.verdict));
    let device = &manifest.device;
    s.push_str(&format!(
        "- Device: {} ({}{})\n",
        device.model,
        device.platform.as_str(),
        device.runtime.as_deref().map(|r| format!(", {r}")).unwrap_or_default()
    ));
    if let Some(app) = &manifest.app {
        let id = app.get("bundle_id").and_then(|v| v.as_str()).unwrap_or("?");
        let version = app.get("version").and_then(|v| v.as_str()).map(|v| format!(" {v}")).unwrap_or_default();
        s.push_str(&format!("- App: `{id}`{version}\n"));
    }
    s.push_str(&format!(
        "- Run: `{}` ({}s), lease `{}`\n",
        manifest.run_id,
        manifest.ended_at - manifest.started_at,
        manifest.lease_id
    ));
    if !manifest.tags.is_empty() {
        s.push_str(&format!("- Tags: {}\n", manifest.tags.join(", ")));
    }
    if let Some(steps) = manifest.steps.as_array().filter(|a| !a.is_empty()) {
        s.push_str("\n| # | Step | Status | Time | Error |\n|---|---|---|---|---|\n");
        for line in steps.iter().filter_map(step_line) {
            s.push_str(&line);
            s.push('\n');
        }
    }
    let notable: Vec<&FileEntry> = manifest
        .files
        .iter()
        .filter(|f| {
            f.path == "video.mp4"
                || f.path == "app.log"
                || f.path.starts_with("crashes/")
                || f.path.ends_with("-failure.png")
                || f.path.ends_with(".xcresult.zip")
        })
        .collect();
    if !notable.is_empty() {
        s.push_str("\nFiles:\n");
        for f in notable {
            s.push_str(&format!("- [{}]({base}/files/{}) ({} bytes)\n", f.path, f.path, f.bytes));
        }
    }
    s.push_str(&format!(
        "\nEvidence: [evidence.zip]({base}/evidence.zip) · manifest SHA-256 `{manifest_sha256}` · {} files\n",
        manifest.files.len()
    ));
    if !manifest.dropped.is_empty() {
        s.push_str(&format!(
            "\n> {} file(s) were not kept: the run hit its evidence size limit.\n",
            manifest.dropped.len()
        ));
    }
    s
}

/// Parses Maestro's per-command debug files (`commands-*.json`) into step records.
pub fn maestro_steps(debug_dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(debug_dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("commands") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    let mut steps = Vec::new();
    for file in files {
        let Ok(bytes) = std::fs::read(&file) else { continue };
        let Ok(serde_json::Value::Array(commands)) = serde_json::from_slice(&bytes) else { continue };
        for c in commands {
            let meta = c.get("metadata");
            let status = meta.and_then(|m| m.get("status")).and_then(|v| v.as_str()).unwrap_or("UNKNOWN");
            let status = match status {
                "COMPLETED" => "passed",
                "FAILED" => "failed",
                "SKIPPED" | "PENDING" => "skipped",
                other => other,
            };
            steps.push(serde_json::json!({
                "index": steps.len(),
                "action": c.get("command").cloned().unwrap_or_default(),
                "status": status,
                "duration_ms": meta.and_then(|m| m.get("duration")).and_then(|v| v.as_u64()).unwrap_or(0),
                "error": meta.and_then(|m| m.pointer("/error/message")).and_then(|v| v.as_str()),
            }));
        }
    }
    steps
}

/// Per-test results from `xcresulttool get test-results tests` (leaf nodes of type `Test Case`).
pub fn xctest_steps(tests_json: &Path) -> Vec<serde_json::Value> {
    fn walk(node: &serde_json::Value, path: &mut Vec<String>, out: &mut Vec<serde_json::Value>) {
        let name = node.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let kind = node.get("nodeType").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "Test Case" {
            let result = node.get("result").and_then(|v| v.as_str()).unwrap_or("unknown");
            let status = match result {
                "Passed" => "passed",
                "Failed" => "failed",
                "Skipped" => "skipped",
                "Expected Failure" => "passed",
                other => other,
            };
            let failure = node
                .get("children")
                .and_then(|c| c.as_array())
                .and_then(|c| c.iter().find(|n| n.get("nodeType").and_then(|v| v.as_str()) == Some("Failure Message")))
                .and_then(|n| n.get("name"))
                .and_then(|v| v.as_str());
            let seconds = node.get("durationInSeconds").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let full_name = if path.is_empty() { name } else { format!("{}/{name}", path.join("/")) };
            out.push(serde_json::json!({
                "index": out.len(),
                "name": full_name,
                "status": status,
                "duration_ms": (seconds * 1000.0) as u64,
                "error": failure,
            }));
            return;
        }
        let pushed = !name.is_empty() && matches!(kind, "Unit test bundle" | "UI test bundle" | "Test Suite");
        if pushed {
            path.push(name);
        }
        for child in node.get("children").and_then(|c| c.as_array()).into_iter().flatten() {
            walk(child, path, out);
        }
        if pushed {
            path.pop();
        }
    }
    let Ok(bytes) = std::fs::read(tests_json) else { return Vec::new() };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return Vec::new() };
    let mut out = Vec::new();
    for root in value.get("testNodes").and_then(|t| t.as_array()).into_iter().flatten() {
        walk(root, &mut Vec::new(), &mut out);
    }
    out
}

/// A JPEG of a run image, longest side <= 2000 px, made with `sips` on the daemon's own copy.
pub async fn preview_jpeg(src: &Path, config: &Config) -> Result<Vec<u8>> {
    let dest = config.data_dir.join("run").join(format!("{}.jpg", uuid::Uuid::new_v4().simple()));
    let out = tokio::process::Command::new("/usr/bin/sips")
        .args(["-Z", "2000", "-s", "format", "jpeg", "-s", "formatOptions", "85"])
        .arg(src)
        .arg("--out")
        .arg(&dest)
        .output()
        .await
        .context("running sips")?;
    let bytes = if out.status.success() { std::fs::read(&dest).ok() } else { None };
    let _ = std::fs::remove_file(&dest);
    bytes.with_context(|| format!("sips could not convert the image: {}", String::from_utf8_lossy(&out.stderr).trim()))
}

/// Builds `evidence.zip` of a run dir into `dest` with `ditto` (keeps the run id as top folder).
pub async fn zip(dir: &Path, dest: &Path) -> Result<()> {
    let out = tokio::process::Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--keepParent"])
        .arg(dir)
        .arg(dest)
        .output()
        .await
        .context("running ditto")?;
    if !out.status.success() {
        bail!("ditto failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_paths_are_cleaned_and_bounded() {
        assert_eq!(clean_relative("steps/001-tap.png").as_deref(), Some("steps/001-tap.png"));
        assert_eq!(clean_relative("attachments/Login screen ✓.png").as_deref(), Some("attachments/Login_screen__.png"));
        assert_eq!(clean_relative("../etc/passwd"), None);
        assert_eq!(clean_relative("a//b"), None);
        assert_eq!(clean_relative(".hidden").as_deref(), Some("hidden"));
        assert_eq!(clean_relative("d/".repeat(9).trim_end_matches('/')), None);
    }

    #[test]
    fn xcresult_tests_become_steps() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tests.json");
        std::fs::write(
            &file,
            serde_json::json!({"testNodes": [{"nodeType": "Test Plan", "name": "Plan", "children": [
                {"nodeType": "UI test bundle", "name": "AppUITests", "children": [
                    {"nodeType": "Test Suite", "name": "LoginTests", "children": [
                        {"nodeType": "Test Case", "name": "testLogin()", "result": "Passed", "durationInSeconds": 3.5},
                        {"nodeType": "Test Case", "name": "testBadPassword()", "result": "Failed", "durationInSeconds": 1.0,
                         "children": [{"nodeType": "Failure Message", "name": "XCTAssertTrue failed"}]}
                    ]}
                ]}
            ]}]})
            .to_string(),
        )
        .unwrap();
        let steps = xctest_steps(&file);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["name"], "AppUITests/LoginTests/testLogin()");
        assert_eq!(steps[0]["duration_ms"], 3500);
        assert_eq!(steps[1]["status"], "failed");
        assert_eq!(steps[1]["error"], "XCTAssertTrue failed");
    }
}
