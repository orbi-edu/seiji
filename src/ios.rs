//! Command builders for iOS simulators: `simctl` (native) and AXe (accessibility input).
//! Pure functions, so the argument lists are reviewable and testable without a simulator.

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::actions::{Direction, HwButton};
use crate::config::Axe;
use crate::devices::{DeviceOp, UiInput};
use crate::exec::{CmdSpec, Output, tail};

#[derive(Debug)]
pub(crate) struct AutomationSessionPending(String);

impl std::fmt::Display for AutomationSessionPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AutomationSessionPending {}

pub const XCRUN: &str = "/usr/bin/xcrun";

pub fn simctl<I, S>(args: I) -> CmdSpec
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    CmdSpec::new(XCRUN).arg("simctl").args(args)
}

fn axe(cfg: &Axe, command: &[&str]) -> CmdSpec {
    CmdSpec::new(&cfg.bin).args(["--output", "json"]).args(command.iter().copied())
}

/// `--key=value`, so a value that starts with `-` is never read as a flag.
fn opt(key: &str, value: impl std::fmt::Display) -> String {
    format!("--{key}={value}")
}

pub fn axe_describe(cfg: &Axe, udid: &str) -> CmdSpec {
    axe(cfg, &["describe-ui"]).arg(opt("udid", udid))
}

pub fn axe_input(cfg: &Axe, udid: &str, input: &UiInput) -> CmdSpec {
    let mut spec = match input {
        UiInput::Tap(t) => {
            let mut s = axe(cfg, &["tap"]);
            match (&t.id, &t.label, &t.value) {
                (Some(id), _, _) => s = s.arg(opt("id", id)),
                (_, Some(label), _) => s = s.arg(opt("label", label)),
                (_, _, Some(value)) => s = s.arg(opt("value", value)),
                _ => {
                    s = s.arg(format!("-x={}", t.x.unwrap_or_default())).arg(format!("-y={}", t.y.unwrap_or_default()));
                }
            }
            if t.id.is_some() || t.label.is_some() || t.value.is_some() {
                s = s.arg(opt("wait-timeout", t.timeout_seconds.unwrap_or(5.0)));
                if let Some(ty) = &t.element_type {
                    s = s.arg(opt("element-type", ty));
                }
            }
            if let Some(c) = t.count {
                s = s.arg(opt("count", c));
            }
            if let Some(h) = t.hold_seconds {
                s = s.arg(opt("hold", h));
            }
            s
        }
        UiInput::Type { text_file } => axe(cfg, &["type", "--stdin"]).stdin_file(text_file),
        UiInput::Swipe(s) => {
            let mut spec = axe(cfg, &["swipe"])
                .arg(opt("start-x", s.from[0]))
                .arg(opt("start-y", s.from[1]))
                .arg(opt("end-x", s.to[0]))
                .arg(opt("end-y", s.to[1]));
            if let Some(d) = s.duration_seconds {
                spec = spec.arg(opt("duration", d));
            }
            spec
        }
        UiInput::Scroll(s) => {
            let preset = match s.direction {
                Direction::Up => "scroll-up",
                Direction::Down => "scroll-down",
                Direction::Left => "scroll-left",
                Direction::Right => "scroll-right",
            };
            let mut spec = axe(cfg, &["gesture", preset]);
            if let Some(u) = &s.until {
                if let Some(id) = &u.id {
                    spec = spec.arg(opt("until-id", id));
                } else if let Some(label) = &u.label {
                    spec = spec.arg(opt("until-label", label));
                }
                spec = spec.arg(opt("max-swipes", s.max_swipes.unwrap_or(10)));
            }
            spec
        }
        UiInput::Button(b) => {
            let name = match b {
                HwButton::Home => "home",
                HwButton::Lock => "lock",
                HwButton::SideButton => "side-button",
                HwButton::Siri => "siri",
            };
            axe(cfg, &["button", name])
        }
        UiInput::Key(k) => axe(cfg, &["key"]).arg(k.to_string()),
        UiInput::KeyCombo { modifiers, key } => axe(cfg, &["key-combo"])
            .arg(opt("modifiers", modifiers.iter().map(u8::to_string).collect::<Vec<_>>().join(",")))
            .arg(opt("key", key)),
    };
    spec = spec.arg(opt("udid", udid));
    spec
}

/// The `data.result` of an AXe `--output json` envelope, or its typed error.
pub fn parse_axe(out: &Output, what: &str) -> Result<Value> {
    let text = String::from_utf8_lossy(&out.stdout);
    let envelope: Option<Value> = text.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok());
    match envelope {
        Some(env) if env.get("didError").and_then(Value::as_bool) == Some(true) => {
            let code = env.pointer("/error/code").and_then(Value::as_str).unwrap_or("commandFailed");
            let message = env.pointer("/error/message").and_then(Value::as_str).unwrap_or("no message");
            if code == "commandFailed" && message == "Timed out creating the simulator remote automation session" {
                return Err(AutomationSessionPending(format!("{what} failed ({code}): {message}")).into());
            }
            bail!("{what} failed ({code}): {message}")
        }
        Some(env) if out.success() => Ok(env.pointer("/data/result").cloned().unwrap_or(Value::Null)),
        _ if out.timed_out => bail!("{what} timed out"),
        _ if out.cancelled => bail!("{what} was cancelled"),
        _ => bail!("{what} failed: {}", tail(&out.text(), 2000)),
    }
}

/// `axe stream-video` in its `raw` framing (a 4-byte big-endian length before each JPEG). It takes
/// no `--output` format, unlike the other AXe commands.
pub fn axe_stream(cfg: &Axe, udid: &str, opts: &crate::devices::StreamOpts) -> CmdSpec {
    CmdSpec::new(&cfg.bin)
        .arg("stream-video")
        .arg(opt("udid", udid))
        .arg(opt("format", "raw"))
        .arg(opt("fps", opts.fps))
        .arg(opt("quality", opts.quality))
        .arg(opt("scale", opts.scale))
}

pub fn screenshot(udid: &str, dest: &Path) -> CmdSpec {
    simctl(["io", udid, "screenshot", "--type=png", "--display=1"]).arg(dest)
}

pub fn record_video(udid: &str, dest: &Path) -> CmdSpec {
    simctl(["io", udid, "recordVideo", "--codec=h264", "--force", "--display=1"]).arg(dest)
}

pub fn log_stream(udid: &str, predicate: &str) -> CmdSpec {
    simctl(["spawn", udid, "log", "stream", "--style", "compact", "--level", "debug", "--predicate", predicate])
}

/// Default log filter for an app: its process when known, its subsystem otherwise.
pub fn app_log_predicate(bundle_id: &str, executable: Option<&str>) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    match executable {
        Some(exe) => format!("process == \"{}\" OR subsystem BEGINSWITH \"{}\"", esc(exe), esc(bundle_id)),
        None => format!("subsystem BEGINSWITH \"{}\"", esc(bundle_id)),
    }
}

pub fn device_op(cfg: &Axe, udid: &str, op: &DeviceOp) -> Result<CmdSpec> {
    Ok(match op {
        DeviceOp::OpenUrl(url) => simctl(["openurl", udid, url]),
        DeviceOp::Launch { bundle_id, arguments, restart } => {
            let mut s = simctl(["launch"]);
            if *restart {
                s = s.arg("--terminate-running-process");
            }
            s.args([udid, bundle_id.as_str()]).args(arguments)
        }
        DeviceOp::Terminate { bundle_id } => simctl(["terminate", udid, bundle_id]),
        DeviceOp::Uninstall { bundle_id } => simctl(["uninstall", udid, bundle_id]),
        DeviceOp::StatusBar(sb) => {
            let mut s = simctl(["status_bar", udid, "override"]);
            if let Some(t) = &sb.time {
                s = s.args(["--time", t.as_str()]);
            }
            if let Some(l) = sb.battery_level {
                s = s.args(["--batteryLevel".to_string(), l.to_string()]);
            }
            if let Some(st) = sb.battery_state {
                s = s.args(["--batteryState", st.as_str()]);
            }
            if let Some(w) = sb.wifi_bars {
                s = s.args(["--dataNetwork", "wifi", "--wifiMode", "active", "--wifiBars"]).arg(w.to_string());
            }
            if let Some(c) = sb.cellular_bars {
                s = s.args(["--cellularMode", "active", "--cellularBars"]).arg(c.to_string());
            }
            s
        }
        DeviceOp::ClearStatusBar => simctl(["status_bar", udid, "clear"]),
        DeviceOp::Appearance(a) => simctl(["ui", udid, "appearance", a.as_str()]),
        DeviceOp::Location(Some((lat, lon))) => simctl(["location", udid, "set"]).arg(format!("{lat},{lon}")),
        DeviceOp::Location(None) => simctl(["location", udid, "clear"]),
        DeviceOp::Privacy { action, service, bundle_id } => {
            simctl(["privacy", udid, action.as_str(), service.as_str(), bundle_id.as_str()])
        }
        DeviceOp::Push { bundle_id, payload_file } => simctl(["push", udid, bundle_id, "-"]).stdin_file(payload_file),
        DeviceOp::LogShow { last_seconds, predicate } => {
            let mut s =
                simctl(["spawn", udid, "log", "show", "--style", "compact", "--last"]).arg(format!("{last_seconds}s"));
            if let Some(p) = predicate {
                s = s.args(["--predicate", p.as_str()]);
            }
            s
        }
        DeviceOp::Bundler { bundle_id, location: Some(loc) } => {
            simctl(["spawn", udid, "defaults", "write", bundle_id, "RCT_jsLocation", "-string", loc])
        }
        DeviceOp::Bundler { bundle_id, location: None } => {
            simctl(["spawn", udid, "defaults", "delete", bundle_id, "RCT_jsLocation"])
        }
        // AXe writes the simulator's own clipboard and reads it back. `simctl pbcopy` goes
        // through the host pasteboard, which an `os_user` tenant without a login session lacks
        // (it crashes there).
        DeviceOp::Pasteboard { text_file } => {
            axe(cfg, &["clipboard", "copy", "--stdin"]).arg(opt("udid", udid)).stdin_file(text_file)
        }
    })
}

/// Installs an archived simulator `.app`; prints bundle id, executable and version, one per line.
/// `$1` archive, `$2` UDID.
pub const INSTALL_SCRIPT: &str = r#"set -e
cd "$(dirname "$1")"
case "$1" in
  *.zip) /usr/bin/ditto -x -k "$1" x ;;
  *.tar.gz|*.tgz) mkdir x && /usr/bin/tar -xzf "$1" -C x ;;
  *) echo "iOS apps must be a .zip or .tar.gz containing a simulator .app bundle" >&2; exit 2 ;;
esac
app=$(/usr/bin/find x -maxdepth 3 -type d -name '*.app' | head -n 1)
# Some archives (Expo Go's simulator build) hold the bundle's contents, not the bundle.
if [ -z "$app" ] && [ -f x/Info.plist ]; then mv x unpacked.app; app=unpacked.app; fi
[ -n "$app" ] || { echo "no .app bundle inside the archive" >&2; exit 2; }
/usr/bin/xcrun simctl install "$2" "$app"
plist="$app/Info.plist"
/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$plist"
/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$plist" 2>/dev/null || echo
/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$plist" 2>/dev/null || echo"#;

/// Runs a `build-for-testing` bundle against one simulator and leaves the evidence in `$2`.
/// `$1` archive, `$2` output dir, `$3` UDID, `$4` `.xctestrun` name or empty, then xcodebuild
/// filter arguments. Exits with xcodebuild's status; prints the log's tail.
pub const XCTEST_SCRIPT: &str = r#"set -eu
bundle="$1"; out="$2"; udid="$3"; want="$4"; shift 4
work="$(dirname "$bundle")/x"
rm -rf "$work"; mkdir -p "$work" "$out"
case "$bundle" in
  *.zip) /usr/bin/ditto -x -k "$bundle" "$work" ;;
  *.tar.gz|*.tgz) /usr/bin/tar -xzf "$bundle" -C "$work" ;;
  *) echo "XCTest bundles must be a .zip or .tar.gz of the build-for-testing Products folder" >&2; exit 2 ;;
esac
if [ -n "$want" ]; then
  run=$(/usr/bin/find "$work" -maxdepth 4 -type f -name "$want" | head -n 1)
else
  run=$(/usr/bin/find "$work" -maxdepth 4 -type f -name '*.xctestrun' | head -n 1)
fi
[ -n "$run" ] || { echo "no .xctestrun file in the bundle" >&2; exit 2; }
set +e
/usr/bin/xcrun xcodebuild test-without-building -xctestrun "$run" \
  -destination "platform=iOS Simulator,id=$udid" -resultBundlePath "$out/result.xcresult" "$@" \
  > "$out/xcodebuild.log" 2>&1
code=$?
if [ -d "$out/result.xcresult" ]; then
  /usr/bin/xcrun xcresulttool get test-results summary --path "$out/result.xcresult" > "$out/xcresult-summary.json" 2>/dev/null
  /usr/bin/xcrun xcresulttool get test-results tests --path "$out/result.xcresult" > "$out/xcresult-tests.json" 2>/dev/null
  /usr/bin/xcrun xcresulttool export attachments --path "$out/result.xcresult" --output-path "$out/attachments" > /dev/null 2>&1
  (cd "$out" && /usr/bin/ditto -c -k --keepParent result.xcresult result.xcresult.zip && /bin/rm -rf result.xcresult)
fi
/usr/bin/tail -n 40 "$out/xcodebuild.log"
exit $code"#;

/// Copies crash reports of `$2` written since `$1/.started` into `$1/crashes`; prints the count.
pub const CRASHES_SCRIPT: &str = r#"dest="$1/crashes"
/bin/mkdir -p "$dest"
src="$HOME/Library/Logs/DiagnosticReports"
[ -d "$src" ] && /usr/bin/find "$src" -maxdepth 1 -type f -name "$2-*.ips" -newer "$1/.started" -exec /bin/cp {} "$dest/" \;
/bin/ls "$dest" | /usr/bin/wc -l"#;

/// Filter values accepted by `-only-testing:` and `-skip-testing:`.
pub fn valid_test_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 512 || !id.chars().all(|c| c.is_ascii_alphanumeric() || "_./-".contains(c)) {
        bail!("test identifier {id:?} must be Target[/Class[/method]] of [A-Za-z0-9_./-]");
    }
    Ok(())
}

/// A process name safe to put in a `find -name` pattern.
pub fn valid_executable(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && name.chars().all(|c| c.is_ascii_alphanumeric() || " _.-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::TapArgs;

    fn args(spec: &CmdSpec) -> Vec<String> {
        spec.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn coordinate_taps_use_axes_short_options() {
        for (x, y, hold) in [(0.0, 0.0, None), (12.5, 34.25, Some(0.5))] {
            let tap: TapArgs = serde_json::from_value(serde_json::json!({
                "x": x, "y": y, "hold_seconds": hold
            }))
            .unwrap();
            let actual = args(&axe_input(&Axe::default(), "U1", &UiInput::Tap(tap)));
            let mut expected =
                vec!["--output".into(), "json".into(), "tap".into(), format!("-x={x}"), format!("-y={y}")];
            if let Some(hold) = hold {
                expected.push(format!("--hold={hold}"));
            }
            expected.push("--udid=U1".into());
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn tap_uses_equals_form_so_values_never_become_flags() {
        let tap = TapArgs {
            id: None,
            label: Some("--udid=OTHER".into()),
            value: None,
            element_type: Some("Button".into()),
            x: None,
            y: None,
            count: Some(2),
            hold_seconds: None,
            timeout_seconds: None,
        };
        let a = args(&axe_input(&Axe::default(), "U1", &UiInput::Tap(tap)));
        assert_eq!(
            a,
            [
                "--output",
                "json",
                "tap",
                "--label=--udid=OTHER",
                "--wait-timeout=5",
                "--element-type=Button",
                "--count=2",
                "--udid=U1"
            ]
        );
    }

    #[test]
    fn axe_errors_surface_their_code() {
        let out = Output {
            code: Some(1),
            stdout: br#"{"command":"tap","data":{},"didError":true,"error":{"code":"elementNotFound","message":"No accessibility element matched --label 'X'."}}"#.to_vec(),
            stderr: Vec::new(),
            timed_out: false,
            cancelled: false,
        };
        let err = parse_axe(&out, "tap").unwrap_err().to_string();
        assert!(err.contains("elementNotFound") && err.contains("matched --label 'X'"), "{err}");
    }

    #[test]
    fn only_remote_automation_startup_timeouts_are_pending() {
        let timeout = "Timed out creating the simulator remote automation session";
        for (code, message, pending) in [
            ("commandFailed", timeout, true),
            ("permissionDenied", timeout, false),
            ("commandFailed", "Connection refused", false),
        ] {
            let out = Output {
                code: Some(1),
                stdout: serde_json::to_vec(&serde_json::json!({
                    "didError": true,
                    "error": { "code": code, "message": message }
                }))
                .unwrap(),
                stderr: Vec::new(),
                timed_out: false,
                cancelled: false,
            };
            let error = parse_axe(&out, "describe-ui during boot").unwrap_err();
            assert_eq!(error.is::<AutomationSessionPending>(), pending, "{code}: {message}");
        }
    }

    #[test]
    fn log_predicate_escapes_quotes() {
        assert_eq!(
            app_log_predicate("com.x", Some("My \"App\"")),
            r#"process == "My \"App\"" OR subsystem BEGINSWITH "com.x""#
        );
    }
}
