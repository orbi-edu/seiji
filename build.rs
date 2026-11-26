//! Records the git commit Seiji is built from: `SEIJI_COMMIT` (the full SHA, unset outside a git
//! checkout) and `SEIJI_VERSION` (`<version> (<short SHA>)`, for `seiji --version`). The server
//! reports the commit in `GET /v1/me`, and `seiji update` brings the CLI and the Tern plugin to it.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Build again when HEAD moves: a checkout, a commit, a pull.
    let head_ref = git(&["symbolic-ref", "-q", "HEAD"]);
    for name in ["HEAD", "packed-refs"].into_iter().chain(head_ref.as_deref()) {
        if let Some(path) = git(&["rev-parse", "--git-path", name])
            && Path::new(&path).exists()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    match git(&["rev-parse", "HEAD"]) {
        Some(commit) => {
            println!("cargo:rustc-env=SEIJI_COMMIT={commit}");
            println!("cargo:rustc-env=SEIJI_VERSION={version} ({})", &commit[..commit.len().min(10)]);
        }
        None => println!("cargo:rustc-env=SEIJI_VERSION={version}"),
    }
}
