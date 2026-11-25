//! The Tern plugin (`tern-plugin/`) in a headless Tern (`tern serve`), against a Seiji server
//! with fake devices and the real `seiji` CLI. Skips when `tern` is not installed.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::*;

/// Lines Tern logs when a plugin handler raises or a view is rejected.
const PLUGIN_ERRORS: [&str; 5] =
    ["plugin handler failed", "plugin failed", "stencil_plugins: ", "unknown kind", "duplicate child key"];
const LEAVES: [&str; 6] = [".sf-text", ".sf-item", ".sf-card-head", ".sf-badge", ".sf-tab", ".sf-kv"];

fn tern() -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| Path::new(d).join("tern")).find(|p| p.is_file())
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}

struct Tern {
    bin: PathBuf,
    sock: PathBuf,
    log: PathBuf,
    child: tokio::process::Child,
    _dir: tempfile::TempDir,
}

impl Drop for Tern {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("\n── Tern log on failure ({}) ──", self.log.display());
            eprintln!("{}", std::fs::read_to_string(&self.log).unwrap_or_else(|e| format!("(read error: {e})")));
        }
    }
}

impl Tern {
    async fn start(bin: PathBuf, env: &Env, token: &str) -> Self {
        // A short path: the control socket must fit in sockaddr_un.
        let dir = tempfile::Builder::new().prefix("seijitern-").tempdir_in("/tmp").unwrap();
        let app = dir.path().join("root/crates/app");
        std::fs::create_dir_all(&app).unwrap();
        copy_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tern-plugin"),
            &dir.path().join("root/crates/plugins/fixtures/seiji"),
        );
        let (sock, log) = (dir.path().join("ctl.sock"), dir.path().join("serve.log"));
        let child = tokio::process::Command::new(&bin)
            .args(["serve", "--control"])
            .arg(&sock)
            .arg("--out")
            .arg(dir.path().join("out"))
            .current_dir(dir.path())
            .env("TERN_CONFIG_DIR", dir.path().join("config"))
            .env("STENCIL_FIXTURE_ROOT", &app)
            .env("STENCIL_LOG", "warn,tern::plugin=info")
            .env("SEIJI_BIN", env!("CARGO_BIN_EXE_seiji"))
            .env("SEIJI_URL", &env.url)
            .env("SEIJI_TOKEN", token)
            .stdout(std::fs::File::create(&log).unwrap())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !sock.exists() {
            assert!(
                Instant::now() < deadline,
                "tern serve did not start: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let t = Self { bin, sock, log, child, _dir: dir };
        t.ctl(&["size", "1280 860"]).await;
        t.ctl(&["ready"]).await;
        t.ctl(&["plugins", "fixtures"]).await;
        t.ctl(&["widgets", "classic"]).await;
        t
    }

    async fn ctl(&self, words: &[&str]) -> Value {
        let out = tokio::process::Command::new(&self.bin)
            .args(["ctl", "--control"])
            .arg(&self.sock)
            .args(words)
            .output()
            .await
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let reply: Value = serde_json::from_str(stdout.lines().last().unwrap_or("{}")).unwrap();
        assert_eq!(reply["ok"], true, "ctl {words:?}: {reply}");
        reply
    }

    /// The visible text of the focused pane's `region`, one line per leaf node.
    async fn text(&self, region: &str) -> String {
        let selector = LEAVES.iter().map(|l| format!(".tn-pane.on .{region} {l}")).collect::<Vec<_>>().join(", ");
        let reply = self.ctl(&["tree", &selector]).await;
        reply["nodes"].as_array().unwrap().iter().filter_map(|n| n["text"].as_str()).collect::<Vec<_>>().join("\n")
    }

    async fn wait_for(&self, what: &str, selector: &str, ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let reply = self.ctl(&["tree", selector]).await;
            let text = reply["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|n| n["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if ok(&text) {
                return text;
            }
            assert!(Instant::now() < deadline, "{what} never showed; {selector} shows:\n{text}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn click_badge(&self, label: &str) {
        let reply = self.ctl(&["tree", ".tn-pane.on .sf-main .sf-badge"]).await;
        let node = reply["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["text"].as_str() == Some(label))
            .unwrap_or_else(|| panic!("badge {label:?} not visible: {reply}"));
        let rect = node["rect"].as_array().unwrap();
        let x = rect[0].as_f64().unwrap() + rect[2].as_f64().unwrap() / 2.0;
        let y = rect[1].as_f64().unwrap() + rect[3].as_f64().unwrap() / 2.0;
        self.ctl(&["click", &format!("{x} {y}")]).await;
    }

    async fn wait_text(&self, needle: &str, region: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let text = self.text(region).await;
            if text.contains(needle) {
                return text;
            }
            assert!(Instant::now() < deadline, "{needle:?} never appeared; {region} shows:\n{text}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn plugin_errors(&self) -> Vec<String> {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        log.lines().filter(|l| PLUGIN_ERRORS.iter().any(|m| l.contains(m))).map(String::from).collect()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_panel_opens_the_screen_beside_it_and_releases_the_device() {
    let Some(bin) = tern() else {
        eprintln!("skipped: tern is not on PATH");
        return;
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", QUOTA);
    let c = client(&env, &token).await;
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();

    let mut t = Tern::start(bin, &env, &token).await;
    t.ctl(&["plugins", "run", "plugin.seiji.devices"]).await;
    let main = t.wait_text(&lease_id, "sf-main").await;
    assert!(main.contains("iphone-16") && main.contains("alice"), "{main}");
    t.wait_for("the status widget", ".wg-status-text", |s| s.contains("Seiji") && s.contains("min left")).await;

    // `o` asks for a viewer link and opens it in Tern's browser beside the panel.
    t.ctl(&["key", "o"]).await;
    let opened = format!("{}/view/open/", env.url);
    t.wait_for("the browser beside the panel", ".tn-pane.web", |s| s.contains(&opened)).await;

    // `x` asks first; enter releases the device.
    t.ctl(&["click", ".tn-pane.dim .sf-main"]).await;
    t.ctl(&["key", "x"]).await;
    t.wait_text("Release iphone-16?", "sf-dock").await;
    t.ctl(&["key", "enter"]).await;
    t.wait_text("No devices", "sf-main").await;
    assert_eq!(*env.fake.destroyed.lock().unwrap(), [lease_id]);

    assert_eq!(t.plugin_errors(), Vec::<String>::new());
    let _ = t.ctl(&["quit"]).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_sees_users_tab_edits_and_saves_limits() {
    let Some(bin) = tern() else {
        eprintln!("skipped: tern is not on PATH");
        return;
    };
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let _bob_token = add_user(&env, "bob", QUOTA);

    let mut t = Tern::start(bin.clone(), &env, &alice_token).await;
    t.ctl(&["plugins", "run", "plugin.seiji.devices"]).await;
    t.wait_text("alice", "sf-main").await;

    // Navigate Devices → Runs → Users.
    t.ctl(&["key", "tab"]).await;
    // Wait for the Runs body to show (not just the tab label).
    t.wait_text("No runs yet", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    // Wait for the user list to load (both user names, not just the nav label).
    t.wait_for("user list loaded", ".tn-pane.on .sf-main .sf-item", |s| s.contains("alice") && s.contains("bob")).await;

    // The detail card shows limits of the first selected user.
    t.wait_for("user detail", ".tn-pane.on .sf-main .sf-kv", |s| {
        s.contains("Session limit") && s.contains("Max devices")
    })
    .await;

    // Select bob (↓), then edit.
    t.ctl(&["key", "down"]).await;
    t.wait_for("bob selected", ".tn-pane.on .sf-main .sf-card-head", |s| s.contains("bob")).await;
    t.ctl(&["key", "e"]).await;
    t.wait_text("Edit limits", "sf-main").await;

    // Increase max concurrent: click the + stepper.
    t.click_badge("+").await;
    t.wait_for("concurrency incremented", ".tn-pane.on .sf-main .sf-text", |s| {
        s.lines().any(|line| line.trim() == "2")
    })
    .await;

    // Toggle unlimited.
    t.click_badge("Unlimited monthly budget").await;
    t.wait_text("preserved for switching back", "sf-main").await;

    // Save.
    t.ctl(&["key", "enter"]).await;
    t.wait_text("Saved limits", "sf-dock").await;

    // Verify DB: bob's quota changed, alice's unchanged.
    let bob = env.pool.store.user_by_name("bob").unwrap().unwrap();
    assert_eq!(bob.quota.max_concurrent, 2, "bob max_concurrent");
    assert!(bob.quota.monthly_unlimited, "bob monthly_unlimited");
    assert_eq!(bob.quota.monthly_minutes, QUOTA.monthly_minutes, "bob monthly_minutes preserved");
    assert_eq!(bob.quota.max_lease_minutes, QUOTA.max_lease_minutes, "bob lease unchanged");
    assert_eq!(bob.quota.idle_minutes, QUOTA.idle_minutes, "bob idle unchanged");
    let alice = env.pool.store.user_by_name("alice").unwrap().unwrap();
    assert_eq!(alice.quota.max_concurrent, QUOTA.max_concurrent, "alice unchanged");
    assert!(!alice.quota.monthly_unlimited, "alice not unlimited");

    // Cancel: start edit again, change something, then cancel. Verify no change.
    t.ctl(&["key", "e"]).await;
    t.wait_text("Edit limits", "sf-main").await;
    t.click_badge("+").await;
    t.ctl(&["key", "escape"]).await;
    t.wait_for("edit dismissed after cancel", ".tn-pane.on .sf-main .sf-card-head", |s| !s.contains("Edit limits"))
        .await;
    assert_eq!(env.pool.store.user_by_name("bob").unwrap().unwrap().quota.max_concurrent, 2);

    // Refresh preserves is_admin and re-fetches users list.
    t.ctl(&["key", "r"]).await;
    t.wait_for("users re-fetched", ".tn-pane.on .sf-main .sf-item", |s| s.contains("alice") && s.contains("bob")).await;

    assert_eq!(t.plugin_errors(), Vec::<String>::new());
    let _ = t.ctl(&["quit"]).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_watches_another_users_device_from_the_users_tab() {
    let Some(bin) = tern() else {
        eprintln!("skipped: tern is not on PATH");
        return;
    };
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let bob_token = add_user(&env, "bob", QUOTA);
    let c = client(&env, &bob_token).await;
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let lease_id = lease["lease_id"].as_str().unwrap().to_string();

    let mut t = Tern::start(bin, &env, &alice_token).await;
    t.ctl(&["plugins", "run", "plugin.seiji.devices"]).await;
    t.wait_text("No devices", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    t.wait_text("No runs yet", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    t.wait_for("user list loaded", ".tn-pane.on .sf-main .sf-item", |s| s.contains("alice") && s.contains("bob")).await;
    t.wait_text("No devices.", "sf-main").await;

    // Bob's card lists his device; Watch opens a view-only screen beside the panel.
    t.ctl(&["key", "down"]).await;
    t.wait_text(&lease_id, "sf-main").await;
    t.click_badge("Watch").await;
    let opened = format!("{}/view/open/", env.url);
    t.wait_for("the browser beside the panel", ".tn-pane.web", |s| s.contains(&opened)).await;
    t.ctl(&["click", ".tn-pane.dim .sf-main"]).await;
    t.wait_text("Opening the screen of bob's iphone-16 (view only)", "sf-dock").await;

    // Copy link copies a view-only link.
    t.click_badge("Copy link").await;
    t.wait_text("Copied a view-only link to bob's iphone-16", "sf-dock").await;

    // Control opens a screen with control.
    t.click_badge("Control").await;
    t.wait_for("the screen taking focus", ".tn-pane.dim .sf-main", |s| !s.is_empty()).await;
    t.ctl(&["click", ".tn-pane.dim .sf-main"]).await;
    t.wait_text("Opening the screen of bob's iphone-16 beside this panel.", "sf-dock").await;

    assert_eq!(t.plugin_errors(), Vec::<String>::new());
    let _ = t.ctl(&["quit"]).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_changes_the_viewer_settings_from_the_users_tab() {
    let Some(bin) = tern() else {
        eprintln!("skipped: tern is not on PATH");
        return;
    };
    let webrtc = json!({"enabled": false, "bind": "127.0.0.1", "ports": [50990, 50999]});
    let env = setup_with(2, json!({ "adminUsers": ["alice"], "viewer": {"webrtc": webrtc} })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let settings = || async {
        let url = format!("{}/v1/admin/viewer", env.url);
        let resp = reqwest::Client::new().get(url).bearer_auth(&alice_token).send().await.unwrap();
        resp.json::<Value>().await.unwrap()["settings"].clone()
    };

    let mut t = Tern::start(bin, &env, &alice_token).await;
    t.ctl(&["plugins", "run", "plugin.seiji.devices"]).await;
    t.wait_text("No devices", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    t.wait_text("No runs yet", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    t.wait_text("Viewer settings", "sf-main").await;
    t.wait_text("80 ms", "sf-main").await;

    // The pacing cap steps by 20 ms, and the server keeps it.
    t.click_badge("+20").await;
    t.wait_text("Viewer settings saved", "sf-dock").await;
    t.wait_text("100 ms", "sf-main").await;
    assert_eq!(settings().await["pace_max_ms"], 100);

    // WebRTC turns on from the same card.
    t.click_badge("WebRTC off").await;
    t.wait_for("WebRTC on", ".tn-pane.on .sf-main .sf-badge", |s| s.lines().any(|l| l == "WebRTC on")).await;
    assert_eq!(settings().await["webrtc"], true);

    // So does the viewer trace; once there is one, the card offers its download.
    t.click_badge("Trace off").await;
    t.wait_for("Trace on", ".tn-pane.on .sf-main .sf-badge", |s| s.lines().any(|l| l == "Download trace")).await;
    assert_eq!(settings().await["trace"], true);

    // And the frame rate of every stream.
    t.click_badge("60 fps").await;
    t.wait_text("now 60 fps", "sf-main").await;
    assert_eq!(settings().await["fps"], 60);

    assert_eq!(t.plugin_errors(), Vec::<String>::new());
    let _ = t.ctl(&["quit"]).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn non_admin_does_not_see_users_tab_or_admin_data() {
    let Some(bin) = tern() else {
        eprintln!("skipped: tern is not on PATH");
        return;
    };
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let _alice_token = add_user(&env, "alice", QUOTA);
    let bob_token = add_user(&env, "bob", QUOTA);

    let mut t = Tern::start(bin, &env, &bob_token).await;
    t.ctl(&["plugins", "run", "plugin.seiji.devices"]).await;
    // Wait for the panel to fully load (budget strip appears).
    t.wait_for("budget loaded", ".tn-pane.on .sf-main", |s| s.contains("bob") && s.contains("min")).await;

    // The full main area should not contain a Users tab.
    let main = t.text("sf-main").await;
    assert!(!main.contains("Users"), "non-admin should not see Users tab: {main}");

    // Tab cycles Devices → Runs → Devices (no Users).
    t.ctl(&["key", "tab"]).await;
    t.wait_text("No runs yet", "sf-main").await;
    t.ctl(&["key", "tab"]).await;
    // Back on Devices: assert the Devices body content, not just the tab label.
    t.wait_text("No devices", "sf-main").await;

    assert_eq!(t.plugin_errors(), Vec::<String>::new());
    let _ = t.ctl(&["quit"]).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t.child.wait()).await;
}
