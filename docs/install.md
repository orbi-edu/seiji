# Install

This document tells you how to install Seiji on one Mac, an AWS EC2 Mac host for production, and how to run it on a laptop for development. You prepare the host (Xcode, AXe, optionally Java and Maestro, optionally the Android SDK), install the binary, create the daemon account, the config, the sudoers rule and the users, check the host with `seiji doctor`, start the launchd service, put TLS in front, and connect the agents. [configuration.md](configuration.md) is the reference of every key, [cli.md](cli.md) of every command, and [security.md](security.md) explains why the accounts and permissions are set up this way.

## Contents

1. [Requirements](#1-requirements)
2. [Prepare the host](#2-prepare-the-host)
3. [Build and install the binary](#3-build-and-install-the-binary)
4. [Create the daemon account](#4-create-the-daemon-account)
5. [Create the data and config directories](#5-create-the-data-and-config-directories)
6. [Write the config file](#6-write-the-config-file)
7. [Install the sudoers rule](#7-install-the-sudoers-rule)
8. [Add users](#8-add-users)
9. [Check the host with `seiji doctor`](#9-check-the-host-with-seiji-doctor)
10. [Run the daemon with launchd](#10-run-the-daemon-with-launchd)
11. [Put TLS in front](#11-put-tls-in-front)
12. [Connect agents](#12-connect-agents)
13. [Upgrade](#13-upgrade)
14. [Uninstall](#14-uninstall)
15. [Local development (shared mode)](#15-local-development-shared-mode)

> **Not tested:** These paths have not been verified on a real host. (1) Under `isolation.mode: "os_user"`, Seiji runs `simctl` for a tenant account that never logged in to the GUI, through `launchctl asuser`. (2) Under `os_user`, AXe and `simctl io recordVideo` run as a tenant account through `exec-as`. `seiji doctor <user>` is the check for both; `isolation.mode: "shared"` is the fallback ([step 9](#9-check-the-host-with-seiji-doctor)). (3) The exact JSON of `xcrun xcresulttool get test-results tests`. Seiji's parser is tolerant: an unknown shape gives a `run_xctest` run without per-test steps, but the `.xcresult` is still kept. (4) Android emulators on an EC2 Mac (Hypervisor.framework on bare metal). Run `seiji doctor <user> --platform android` before you offer Android.

## 1. Requirements

| Item | Notes |
|---|---|
| A Mac | An EC2 `mac-m4max.metal` instance is the target. Seiji uses only macOS tools, so another Apple-silicon Mac is set up the same way. |
| An admin account with `sudo` | `ec2-user` on EC2. You run the install as this account. |
| Xcode and an iOS simulator runtime | For iOS. Its command line tools give `xcrun simctl`, `xcodebuild` and `xcresulttool`. |
| AXe | For iOS screen input and the accessibility tree. `brew install cameroncooke/axe/axe`, at `/opt/homebrew/bin/axe`, readable and runnable by every account. |
| Java 17 | Only for Maestro. Homebrew's `openjdk@17` is used below. |
| Maestro CLI | Optional: only for `run_flow` and `inspect_screen` on Android. Under `/opt/maestro`, readable by every account. |
| Rust (1.89 or later) | To build Seiji. |
| Android SDK, emulator and AVDs | Only for Android. Under `/opt/android-sdk` and `/opt/android-avd`. |
| A DNS name and a TLS proxy | Agents connect over HTTPS. Caddy is used below. |
| Outbound HTTPS | For `cheat_sheet` and for `install_app` with a URL. |

**EC2 notes**

- An EC2 Mac runs on a dedicated host with a 24-hour minimum allocation (Apple's macOS license). You pay for the host whether devices run or not; `costPerDeviceHourUsd` only splits that fixed cost between users.
- Sizing is RAM-bound. The code's rule of thumb is about 1.5 GiB per iOS simulator and about 4 GiB per Android emulator. Choose `capacity.ios` and `capacity.android` so that `1.5 × capacity.ios + 4 × capacity.android` stays well below the instance's memory, with room for macOS, Xcode services and one Maestro JVM per running operation.
- Xcode, the iOS runtimes and the Android system images take tens of GiB. Run evidence takes up to `evidence.maxBytesPerUser` (5 GiB by default) per user, so plan `seats × 5 GiB` more ([configuration.md](configuration.md#evidence)). Make the root EBS volume large enough, and grow the APFS container after you enlarge it, before you start.

## 2. Prepare the host

Log in as the admin account (SSH as `ec2-user` on EC2).

### Xcode and the iOS runtime

1. Install Xcode from the App Store or from developer.apple.com.
2. Select it:
   ```sh
   sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
   ```
3. Accept the license:
   ```sh
   sudo xcodebuild -license accept
   ```
4. Install the system components:
   ```sh
   sudo xcodebuild -runFirstLaunch
   ```
5. Download the iOS simulator runtime:
   ```sh
   xcodebuild -downloadPlatform iOS
   ```
6. Note the runtime and device type identifiers for `ios.models`:
   ```sh
   xcrun simctl list runtimes
   ```
   ```sh
   xcrun simctl list devicetypes
   ```
7. Check that the command line tools Seiji runs for `run_xctest` are there:
   ```sh
   xcrun --find xcodebuild
   ```
   ```sh
   xcrun --find xcresulttool
   ```
   `run_xctest` runs `/usr/bin/xcrun xcodebuild test-without-building` and `xcrun xcresulttool get test-results`. If `xcrun` finds nothing, step 2 selected the wrong developer directory.

### AXe

[AXe](https://github.com/cameroncooke/AXe) (MIT) drives iOS simulators through Apple's private accessibility and HID frameworks: taps, typing, gestures and the accessibility tree. Seiji uses it for `run_actions` and for `inspect_screen` on iOS. Everything else on iOS is native (`xcrun simctl`, `xcodebuild`, `xcresulttool`). AXe supports Xcode 26 and 27. A new Xcode can break AXe until AXe ships a fix, so check AXe before you upgrade Xcode on the host.

1. Install it with Homebrew:
   ```sh
   brew install cameroncooke/axe/axe
   ```
2. Check it:
   ```sh
   /opt/homebrew/bin/axe --version
   ```
   This path is the default of `axe.bin` ([configuration.md](configuration.md#axe)).
3. Make it readable and runnable by every account. Under `os_user` each tenant account runs AXe (in shared mode, `seiji` does). Homebrew's files are normally readable by all; if they are not, run:
   ```sh
   sudo chmod -R a+rX "$(brew --prefix axe)/"
   ```
   `seiji doctor` runs `axe --version` and `axe describe-ui` as the user's account, which proves it ([step 9](#9-check-the-host-with-seiji-doctor)).

To update AXe later, run `brew upgrade axe`, then `seiji doctor` again.

### Java 17

Only for Maestro. Skip it if you skip Maestro.

1. Install it with Homebrew:
   ```sh
   brew install openjdk@17
   ```
   The JDK is at `/opt/homebrew/opt/openjdk@17`. This is the value of `maestro.javaHome`. Tenant accounts have no shell profile, so Seiji passes `JAVA_HOME` explicitly.

### Maestro under `/opt/maestro` (optional)

Maestro is needed only for `run_flow` and for `inspect_screen` on Android. Without it, remove the `maestro` section from the config ([step 6](#6-write-the-config-file)); those two tools then fail with ``Maestro is not configured on this host (`maestro` in seiji.json)``.

Every account that runs flows (each tenant account, or `seiji` in shared mode) must read and run Maestro, so it does not go in a home directory.

1. Download the CLI (or replace `latest/download` with `download/cli-<version>` to pin a version):
   ```sh
   curl -fsSL -o /tmp/maestro.zip https://github.com/mobile-dev-inc/maestro/releases/latest/download/maestro.zip
   ```
2. Unpack it. The archive has a top-level `maestro/` directory:
   ```sh
   sudo unzip -q /tmp/maestro.zip -d /opt
   ```
3. Make it readable by all:
   ```sh
   sudo chmod -R a+rX /opt/maestro
   ```
4. Check it:
   ```sh
   JAVA_HOME=/opt/homebrew/opt/openjdk@17 /opt/maestro/bin/maestro --version
   ```

### Android SDK and AVDs (optional)

Skip this part if you offer only iOS. Seiji runs `<sdkRoot>/emulator/emulator` and `<sdkRoot>/platform-tools/adb`, and the emulators read the AVDs with `-read-only`, so nobody writes `/opt/android-avd` after you create it.

1. Create the directories, owned by you while you install:
   ```sh
   sudo mkdir -p /opt/android-sdk/cmdline-tools /opt/android-avd
   ```
   ```sh
   sudo chown -R "$USER" /opt/android-sdk /opt/android-avd
   ```
2. Download the "Command line tools only" package for Mac from https://developer.android.com/studio#command-line-tools-only, then unpack it:
   ```sh
   unzip -q ~/Downloads/commandlinetools-mac-*_latest.zip -d /tmp/cmdline-tools
   ```
   ```sh
   mv /tmp/cmdline-tools/cmdline-tools /opt/android-sdk/cmdline-tools/latest
   ```
3. Point the SDK tools at Java 17:
   ```sh
   export JAVA_HOME=/opt/homebrew/opt/openjdk@17
   ```
4. Accept the licenses:
   ```sh
   /opt/android-sdk/cmdline-tools/latest/bin/sdkmanager --licenses
   ```
5. Install the platform tools, the emulator and a system image:
   ```sh
   /opt/android-sdk/cmdline-tools/latest/bin/sdkmanager "platform-tools" "emulator" "system-images;android-35;google_apis;arm64-v8a"
   ```
6. Create the AVD that the example config names (`pixel_8_api_35`):
   ```sh
   ANDROID_AVD_HOME=/opt/android-avd /opt/android-sdk/cmdline-tools/latest/bin/avdmanager create avd -n pixel_8_api_35 -k "system-images;android-35;google_apis;arm64-v8a" -d pixel_8
   ```
7. Make the SDK readable by all:
   ```sh
   sudo chmod -R a+rX /opt/android-sdk
   ```
8. Create the tenant group now, so you can give it the AVDs. `seiji tenant provision` runs the same command and skips it when the group exists:
   ```sh
   sudo dseditgroup -o create -r "Seiji tenants" seiji-tenants
   ```
9. Give the AVDs to root and the group, readable only by the group:
   ```sh
   sudo chown -R root:seiji-tenants /opt/android-avd
   ```
   ```sh
   sudo chmod -R u=rwX,g=rX,o= /opt/android-avd
   ```

In shared mode the emulators run as `seiji`, which is not in `seiji-tenants`. Use `sudo chmod -R a+rX /opt/android-avd` instead of step 9.

## 3. Build and install the binary

1. Install Rust:
   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
2. Clone the repository:
   ```sh
   git clone https://github.com/wiljafor1/seiji.git
   ```
3. Build it:
   ```sh
   cd seiji
   ```
   ```sh
   cargo build --release
   ```
4. Build the viewer's screen helper (Swift, with the Xcode toolchain):
   ```sh
   swiftc -O -swift-version 5 -o target/seiji-sim sim/*.swift -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework IOSurface -framework Accelerate
   ```
5. Install both owned by root and writable only by root:
   ```sh
   sudo install -o root -g wheel -m 755 target/release/seiji /usr/local/bin/seiji
   ```
   ```sh
   sudo install -o root -g wheel -m 755 target/seiji-sim /usr/local/bin/seiji-sim
   ```
6. Check that neither the files nor their directory are writable by anyone but root:
   ```sh
   ls -ld /usr/local/bin /usr/local/bin/seiji /usr/local/bin/seiji-sim
   ```

**Why root-owned:** sudoers lets the daemon account run `/usr/local/bin/seiji exec-as *` and `/usr/local/bin/seiji kill-as *` as root. If `seiji` could write the binary, or the directory that holds it, it could replace the binary and run anything as root. The binary is the only thing the root helpers trust: they never read the config, which the daemon account could change. The rest of the commands below stay in the clone directory, because they use files from `deploy/`.

## 4. Create the daemon account

The daemon runs as `seiji`, a hidden account without admin rights. Nobody logs in as it.

1. Create it with a random password:
   ```sh
   sudo sysadminctl -addUser seiji -fullName "Seiji daemon" -password "$(openssl rand -base64 24)" -home /Users/seiji
   ```
2. Hide it from the login window:
   ```sh
   sudo dscl . -create /Users/seiji IsHidden 1
   ```
3. Create its home (in shared mode, Maestro and CoreSimulator keep their files there):
   ```sh
   sudo createhomedir -c -u seiji
   ```
4. Check that it is not in `admin` (gid 80) and not in `seiji-tenants`:
   ```sh
   id seiji
   ```

## 5. Create the data and config directories

1. Create the directories:
   ```sh
   sudo mkdir -p /usr/local/var/seiji /usr/local/etc/seiji /usr/local/var/log
   ```
2. Give the data directory to the daemon account:
   ```sh
   sudo chown seiji /usr/local/var/seiji
   ```
3. Create the log file for the daemon account:
   ```sh
   sudo touch /usr/local/var/log/seiji.log
   ```
   ```sh
   sudo chown seiji /usr/local/var/log/seiji.log
   ```

Seiji sets the rest itself. `seiji serve` and `seiji doctor` set the data directory and its `run` directory to `0711` and `run/pids` to `1733` at each start, and every daemon-side command runs with umask `077`, so the database and the builds are private to `seiji`. Under `os_user` every command a tenant account runs writes a pid file to `/usr/local/var/seiji/run/pids` first, so the tenant accounts need search permission on the data directory **and its parents**. Directories that `sudo mkdir -p` creates have mode `755`, which is enough; a data directory inside a private home is not ([troubleshooting.md](troubleshooting.md#every-command-fails-with-permission-denied-under-os_user)). [configuration.md](configuration.md#the-data-directory) lists what lives in the data directory.

## 6. Write the config file

1. Install the example, owned by root and readable by all:
   ```sh
   sudo install -o root -g wheel -m 644 deploy/seiji.example.json /usr/local/etc/seiji/seiji.json
   ```
2. Edit it:
   ```sh
   sudo vi /usr/local/etc/seiji/seiji.json
   ```
   Change at least:
   - `listen.host` to `"127.0.0.1"`, because Caddy runs on the same host ([step 11](#11-put-tls-in-front)).
   - `listen.allowedHosts`: replace `seiji.example.com` with your hostname.
   - `seats`, `capacity`, `defaultQuota` and `costPerDeviceHourUsd` for your team and host.
   - `ios.models`: the device types and runtimes from `xcrun simctl list`.
   - `android`: remove the section if you skipped the Android SDK.
   - `maestro`: remove the section if you skipped Maestro. With the section, `seiji doctor` runs `maestro --version` and fails when Maestro is missing.
   - `listen.publicUrl`: add it with your public URL (`"https://seiji.example.com"`), so that the run links in tool results are absolute ([configuration.md](configuration.md#listen)).
   - `axe`, `evidence` and `xctest`: the defaults fit most hosts. Add them only to change a limit ([configuration.md](configuration.md#axe)).
3. Check the file and create the database. This prints an empty user table and `seats: 0/<seats>`:
   ```sh
   sudo -u seiji seiji user list
   ```

The daemon only reads the config. Keep it root-owned. [configuration.md](configuration.md) describes every key and the validation errors.

## 7. Install the sudoers rule

Only for `os_user`. In shared mode Seiji never calls sudo; skip this step.

[`deploy/sudoers.seiji`](../deploy/sudoers.seiji) lets the daemon account run exactly two commands as root, without a password:

```text
Defaults:seiji !requiretty, env_reset
seiji ALL=(root) NOPASSWD: /usr/local/bin/seiji exec-as *, /usr/local/bin/seiji kill-as *
```

1. Check the syntax:
   ```sh
   sudo visudo -cf deploy/sudoers.seiji
   ```
2. Install it:
   ```sh
   sudo install -m 440 deploy/sudoers.seiji /etc/sudoers.d/seiji
   ```
3. Check what `seiji` may run:
   ```sh
   sudo -l -U seiji
   ```

If you change `isolation.helper`, change both paths in the rule to the same value. Both helpers refuse any target account that is not in `seiji-tenants`, is an admin, or has a uid below 501. [cli.md](cli.md#root-helpers-internal) describes them.

## 8. Add users

Do this for each person. The examples use `alice`.

1. Create the tenant account (only for `os_user`). It creates the hidden account `seiji-alice` in `seiji-tenants`, with no password, no login shell and a private home. It refuses to change an existing account that is not already a tenant account ([cli.md](cli.md#seiji-tenant-provision)):
   ```sh
   sudo seiji tenant provision alice
   ```
2. Add the user. It prints the token once:
   ```sh
   sudo -u seiji seiji user add alice --monthly-minutes 1200 --max-concurrent 2
   ```
3. Give the token to the person over a private channel. Seiji stores only its hash. If it is lost, run `sudo -u seiji seiji user rotate-token alice`.

Omitted quota flags take their values from `defaultQuota`. [cli.md](cli.md#seiji-user) lists the flags and the other user commands.

## 9. Check the host with `seiji doctor`

`seiji doctor` boots, captures and deletes a throwaway device as the user's account, through the same sudo chain as the daemon. It does not use the user's budget. On iOS it also runs `axe --version`, reads the accessibility tree with `axe describe-ui`, and records 3 s of video and log, so it proves that the user's account can run AXe and `recordVideo` ([cli.md](cli.md#seiji-doctor) lists the steps). It runs `maestro --version` only when `maestro` is configured.

1. Check iOS:
   ```sh
   sudo -u seiji seiji doctor alice --platform ios
   ```
2. Check Android, if you configured it:
   ```sh
   sudo -u seiji seiji doctor alice --platform android
   ```

The last line is `OK: seiji-alice can boot, capture and delete ios devices`. A failed step prints its error; [troubleshooting.md](troubleshooting.md) has the fixes.

**If an AXe step or the video capture fails** (`axe --version`, `accessibility tree (axe describe-ui)` or `video and log capture (3 s)`), see [troubleshooting.md](troubleshooting.md#axe-does-not-start). Under `os_user`, if the same commands work for your admin account, shared mode is the fallback, as below.

**If `create` or `boot` fails under `os_user`** while the same `xcrun simctl` commands work for your admin account, you hit the untested path: simctl for an account that never logged in to the GUI, through `launchctl asuser`. Fall back to shared mode:

1. Set `"mode": "shared"` in `isolation`:
   ```sh
   sudo vi /usr/local/etc/seiji/seiji.json
   ```
2. Run doctor again:
   ```sh
   sudo -u seiji seiji doctor alice --platform ios
   ```

Users you added under `os_user` keep working under `shared`; their tenant accounts are no longer used. In shared mode every command runs as `seiji`, and only Seiji separates users: each token sees only its own leases and builds, and every lease gets a new device. Maestro flows run as `seiji` and can read what `seiji` can read. [security.md](security.md) explains the difference.

In shared mode `doctor` runs in your SSH session, not under launchd. If doctor passes but `acquire_device` fails, read the daemon log.

## 10. Run the daemon with launchd

[`deploy/dev.seiji.daemon.plist`](../deploy/dev.seiji.daemon.plist) runs `/usr/local/bin/seiji serve --config /usr/local/etc/seiji/seiji.json` as `seiji` (`UserName`), at boot (`RunAtLoad`), restarts it when it exits (`KeepAlive`), gives it 120 s to release devices on stop (`ExitTimeOut`), sets `RUST_LOG=seiji=info`, and writes stdout and stderr to `/usr/local/var/log/seiji.log`.

1. Install the plist:
   ```sh
   sudo install -m 644 deploy/dev.seiji.daemon.plist /Library/LaunchDaemons/
   ```
2. Load and start it:
   ```sh
   sudo launchctl bootstrap system /Library/LaunchDaemons/dev.seiji.daemon.plist
   ```
3. Check that it runs:
   ```sh
   sudo launchctl print system/dev.seiji.daemon
   ```
4. Check the health endpoint. It answers `ok`:
   ```sh
   curl http://127.0.0.1:4780/healthz
   ```
5. Check the log. It ends with `seiji listening on http://127.0.0.1:4780/mcp`:
   ```sh
   tail /usr/local/var/log/seiji.log
   ```

| Task | Command |
|---|---|
| Restart (graceful: devices are released with `server_shutdown`, then launchd starts it again) | `sudo launchctl kill SIGTERM system/dev.seiji.daemon` |
| Stop and unload | `sudo launchctl bootout system/dev.seiji.daemon` |
| Start again | `sudo launchctl bootstrap system /Library/LaunchDaemons/dev.seiji.daemon.plist` |

`seiji serve` reads the config only at start. Restart it after every config change. The log file grows without limit; rotate it with `newsyslog` if you need to.

## 11. Put TLS in front

Seiji serves plain HTTP. Put a TLS proxy in front and keep port 4780 closed to the outside. This example uses Caddy on the same host. An ALB or Tailscale works too.

1. Point a DNS name (here `seiji.example.com`) at the instance's public address. Open ports 443 and 80 (for the certificate challenge) in the security group.
2. Install Caddy:
   ```sh
   brew install caddy
   ```
3. Write `/opt/homebrew/etc/Caddyfile`:
   ```text
   seiji.example.com {
   	reverse_proxy 127.0.0.1:4780
   }
   ```
4. Start it as a service (as root, to bind ports 80 and 443):
   ```sh
   sudo brew services start caddy
   ```
5. Make sure `listen.allowedHosts` has `seiji.example.com` and `listen.host` is `127.0.0.1` ([step 6](#6-write-the-config-file)). Caddy forwards the original `Host` header, and `/mcp` refuses any host not in the list with `403 Forbidden: Host header is not allowed`.

Notes:

- Seiji caps uploads itself at `maxUploadBytes` ([configuration.md](configuration.md#maxuploadbytes)), so the proxy needs no body limit.
- Each tool call response is an SSE stream (`text/event-stream`) with keepalive comments every 15 s. The proxy needs only an idle read timeout above 15 s, not above the absolute operation time. Caddy has no response timeout by default. With another proxy (nginx, an ALB), set its idle or read timeout above 15 s.
- Run evidence is served at `/v1/runs/<run id>/…` on the same host and needs the same `Authorization` header. A video or an `evidence.zip` can be large; Caddy streams it with no size limit.

## 12. Connect agents

Each person uses their own token. The MCP endpoint is `https://<host>/mcp` (Streamable HTTP) with the header `Authorization: Bearer <token>`. [mcp.md](mcp.md) documents the tools and the upload endpoint.

**Claude Code:**

```sh
claude mcp add --transport http seiji https://seiji.example.com/mcp --header "Authorization: Bearer sj_..."
```

**Other clients** take an `mcpServers` entry with a URL and headers. Some clients also want `"type": "http"`:

```json
{
  "mcpServers": {
    "seiji": {
      "url": "https://seiji.example.com/mcp",
      "headers": { "Authorization": "Bearer sj_..." }
    }
  }
}
```

**Uploads:** The server's instructions tell the agent to upload builds with `curl -H "Authorization: Bearer $SEIJI_TOKEN" … <server>/v1/apps`. Set `SEIJI_TOKEN` in the agent's environment:

```sh
export SEIJI_TOKEN=sj_...
```

**Check the token:** A valid token gets `400` (the request has no `X-Filename`); a refused one gets `401`:

```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST https://seiji.example.com/v1/apps -H "Authorization: Bearer $SEIJI_TOKEN"
```

**Download evidence:** Each run result has an `evidence_zip` link (absolute with `listen.publicUrl`). The same token downloads it:

```sh
curl -fsS -H "Authorization: Bearer $SEIJI_TOKEN" -o evidence.zip https://seiji.example.com/v1/runs/<run id>/evidence.zip
```

Then ask the agent to call `budget_status`.

## 13. Upgrade

Stopping the daemon releases every device. Tell the users first.

1. Get the new code (in the clone):
   ```sh
   git pull
   ```
2. Build it, and the screen helper (step 3):
   ```sh
   cargo build --release
   ```
   ```sh
   swiftc -O -swift-version 5 -o target/seiji-sim sim/*.swift -framework VideoToolbox -framework CoreMedia -framework CoreVideo -framework IOSurface -framework Accelerate
   ```
3. Stop the daemon:
   ```sh
   sudo launchctl bootout system/dev.seiji.daemon
   ```
4. Install the binaries:
   ```sh
   sudo install -o root -g wheel -m 755 target/release/seiji /usr/local/bin/seiji
   ```
   ```sh
   sudo install -o root -g wheel -m 755 target/seiji-sim /usr/local/bin/seiji-sim
   ```
5. Compare the deploy files with the installed copies, and install the ones that changed (steps 7 and 10):
   ```sh
   diff deploy/sudoers.seiji /etc/sudoers.d/seiji
   ```
   ```sh
   diff deploy/dev.seiji.daemon.plist /Library/LaunchDaemons/dev.seiji.daemon.plist
   ```
6. Start the daemon:
   ```sh
   sudo launchctl bootstrap system /Library/LaunchDaemons/dev.seiji.daemon.plist
   ```

Seiji has no schema migrations: at start it creates missing tables (`CREATE TABLE IF NOT EXISTS`) and changes nothing else. Back up `seiji.db` before an upgrade ([configuration.md](configuration.md#the-data-directory)).

After an upgrade, run `sudo -u seiji seiji doctor <user> --platform ios`. It checks AXe, the screen helper and the capture as the user's account. If the host predates recorded runs, install AXe first ([AXe](#axe)); the `runs` table is created at the next start. If the config still has `viewer.quality` (from the JPEG viewer), remove it: the daemon does not start with it ([configuration.md](configuration.md#viewer)).

## 14. Uninstall

1. Stop and unload the daemon. This releases every device:
   ```sh
   sudo launchctl bootout system/dev.seiji.daemon
   ```
2. Remove the plist:
   ```sh
   sudo rm /Library/LaunchDaemons/dev.seiji.daemon.plist
   ```
3. Remove the sudoers rule:
   ```sh
   sudo rm /etc/sudoers.d/seiji
   ```
4. List the tenant accounts:
   ```sh
   dscl . -read /Groups/seiji-tenants GroupMembership
   ```
5. Delete each tenant account record:
   ```sh
   sudo dscl . -delete /Users/seiji-alice
   ```
6. Delete its home (its workspace and simulators):
   ```sh
   sudo rm -rf /Users/seiji-alice
   ```
7. Delete the tenant group:
   ```sh
   sudo dseditgroup -o delete seiji-tenants
   ```
8. Delete the daemon account:
   ```sh
   sudo sysadminctl -deleteUser seiji
   ```
9. Delete the data, the config and the log:
   ```sh
   sudo rm -rf /usr/local/var/seiji /usr/local/etc/seiji /usr/local/var/log/seiji.log
   ```
10. Delete the binaries:
   ```sh
   sudo rm /usr/local/bin/seiji /usr/local/bin/seiji-sim
   ```

Check for emulators that are still running with `ps -axo user,pid,command | grep -- '[-]avd '`. Remove `/opt/maestro`, `/opt/android-sdk` and `/opt/android-avd` if nothing else uses them, and AXe with `brew uninstall axe`.

## 15. Local development (shared mode)

On a laptop you run everything as yourself, with no daemon account, no sudoers rule and no tenant accounts.

1. Install Xcode with an iOS runtime, AXe and Rust (see [step 2](#2-prepare-the-host)). Java 17 and Maestro are optional; Maestro can stay in `~/.maestro`. Build the screen helper (step 3) and set `viewer.simBin` to `target/seiji-sim` in your clone, with its full path.
2. Write a config outside the repository, for example `~/seiji-dev/seiji.json`. Start from the smallest example in [configuration.md](configuration.md#full-example). To use `run_flow`, add `maestro`, with your paths:
   ```json
   "maestro": {
     "bin": "/Users/<you>/.maestro/bin/maestro",
     "javaHome": "/opt/homebrew/opt/openjdk@17"
   }
   ```
   The default `dataDir` (`./data`) resolves next to the config file. The run links are server-relative until you set `listen.publicUrl` (for example `"http://localhost:4780"`).
3. Point the commands at it:
   ```sh
   export SEIJI_CONFIG=~/seiji-dev/seiji.json
   ```
4. Add yourself (in the clone). It prints a token:
   ```sh
   cargo run -- user add me
   ```
5. Check the host:
   ```sh
   cargo run -- doctor me
   ```
6. Start the server on `127.0.0.1:4780`:
   ```sh
   cargo run -- serve
   ```
7. Connect your agent:
   ```sh
   claude mcp add --transport http seiji http://localhost:4780/mcp --header "Authorization: Bearer sj_..."
   ```

Keep these in mind:

- Every device, every AXe command, every Maestro flow and every XCTest bundle runs as you. They can read what you can read.
- At each start, `seiji serve` deletes every simulator in your device set whose name starts with `seiji-`. Do not give your own simulators that prefix.
- `cargo test` runs the end-to-end tests over real HTTP and MCP with a fake device layer (`FakeDevices` in `tests/common/mod.rs`) and a stub `maestro`. It needs no simulator and no AXe.
