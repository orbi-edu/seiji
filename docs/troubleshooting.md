# Troubleshooting

Each section gives a symptom, the checks to do, and the fix. The quoted texts are the exact messages Seiji prints; `<…>` marks the parts that vary. An MCP tool returns its error as text in the tool result, so the agent sees the same message as the log. [cli.md](cli.md) lists the commands, and [configuration.md](configuration.md) the keys.

## Contents

- [Read the logs](#read-the-logs)
- [The daemon does not start](#the-daemon-does-not-start)
- [Tool calls get 401](#tool-calls-get-401)
- [The Host header is rejected](#the-host-header-is-rejected)
- [Long tool calls fail at the proxy](#long-tool-calls-fail-at-the-proxy)
- [`acquire_device` is refused](#acquire_device-is-refused)
- [A device fails to start](#a-device-fails-to-start)
- [Android emulator problems](#android-emulator-problems)
- [`sudo: a password is required`](#sudo-a-password-is-required)
- [Tenant account errors](#tenant-account-errors)
- [Every command fails with `Permission denied` under `os_user`](#every-command-fails-with-permission-denied-under-os_user)
- [Maestro does not start](#maestro-does-not-start)
- [Flows time out](#flows-time-out)
- [`<what> supports iOS simulators only for now`](#what-supports-ios-simulators-only-for-now)
- [AXe does not start](#axe-does-not-start)
- [A step fails with an AXe error](#a-step-fails-with-an-axe-error)
- [A run is refused before it starts](#a-run-is-refused-before-it-starts)
- [A run ends `failed`, `error` or `cancelled`](#a-run-ends-failed-error-or-cancelled)
- [`run_xctest` problems](#run_xctest-problems)
- [Evidence is missing from a run](#evidence-is-missing-from-a-run)
- [Run files cannot be downloaded](#run-files-cannot-be-downloaded)
- [`get_logs` errors](#get_logs-errors)
- [A lease ends during an operation](#a-lease-ends-during-an-operation)
- [App upload and install errors](#app-upload-and-install-errors)
- [Screenshots](#screenshots)
- [The viewer](#the-viewer)
- [Tunnels](#tunnels)
- [Leftover devices after a crash](#leftover-devices-after-a-crash)
- [`cheat_sheet` fails](#cheat_sheet-fails)

## Read the logs

| What | Where |
|---|---|
| Daemon log (stdout and stderr) | `/usr/local/var/log/seiji.log`, from `StandardOutPath` and `StandardErrorPath` in [`deploy/dev.seiji.daemon.plist`](../deploy/dev.seiji.daemon.plist) |
| Service state and last exit code | `sudo launchctl print system/dev.seiji.daemon` |
| Emulator output | `<dataDir>/run/emulator-<lease id>.log`, while the lease exists. Seiji deletes it at release. A boot failure quotes its last 3000 bytes. |
| Maestro output | In the run's evidence: `maestro/output.txt`, `maestro/report.xml`, `maestro/debug/` and `maestro/output/`. Read them with `get_run` and `get_run_artifact`. |
| Run evidence | `<dataDir>/runs/<user id>/<run id>/`, with `manifest.json`. Its `warnings` list the problems that did not fail the run, and `dropped` the files that did not fit. |
| `seiji doctor` | Its own stdout and stderr |

```sh
tail -f /usr/local/var/log/seiji.log
```

`RUST_LOG` sets the log filter (default `seiji=info`). To change it, edit the `RUST_LOG` value in `/Library/LaunchDaemons/dev.seiji.daemon.plist`, then restart the service ([install.md](install.md#10-run-the-daemon-with-launchd)). `seiji=debug` adds detail. `seiji=info,rmcp=warn` also shows the MCP library's warnings, such as rejected `Host` headers.

Useful log lines:

| Line | Meaning |
|---|---|
| `seiji listening on http://<host>:<port>/mcp` | Start-up finished. |
| `lease ready` (`user`, `lease`, `device`) | A device booted. |
| `lease released` (`user`, `lease`, `reason`) | A lease ended. `reason` is the end reason. |
| `boot did not complete: <cause>` (`user`, `lease`, `reason`) | A device did not become ready. With `reason` `boot_failed` the boot failed and is not billed; any other `reason` means the lease was released while it booted. |
| `release (<reason>) failed: <cause>` | The reaper could not release a lease. |
| `deleting orphaned simulator seiji-<…>` | Start-up recovery deleted a leftover simulator. |
| `orphan scan: <cause>` | Start-up recovery could not list a user's simulators. |
| `destroy: <cause>` | Deleting a device or its lease dir failed. |
| `deleting expired build: <cause>` | The reaper could not delete a build directory. |
| `run recorded` (`run`, `lease`, `status`, `bytes`) | A run ended and its manifest was written. |
| `run interrupted by the restart is marked as an error` (`run`) | Start-up recovery found a run still recorded as `running` and closed it ([`server restarted during the run`](#server-restarted-during-the-run)). |
| `writing its manifest: <cause>` (`run`) | Start-up recovery could not write the manifest of an interrupted run. |
| `closing a failed run: <cause>` (`run`) | Seiji could not write the `error` manifest of a run whose evidence recording failed. |
| `deleting expired evidence: <cause>` (`run`) | The reaper could not delete a run directory. |
| `shutting down: releasing devices` | Graceful stop. |

Seiji has no command that lists active leases. Read them from the database:

```sh
sudo -u seiji sqlite3 /usr/local/var/seiji/seiji.db "SELECT l.id, u.name, l.platform, l.status, datetime(l.started_at, 'unixepoch') FROM leases l JOIN users u ON u.id = l.user_id WHERE l.status != 'ended'"
```

Recent runs, with their status and verdict:

```sh
sudo -u seiji sqlite3 /usr/local/var/seiji/seiji.db "SELECT r.id, u.name, r.kind, r.status, r.bytes, r.summary FROM runs r JOIN users u ON u.id = r.user_id ORDER BY r.started_at DESC LIMIT 20"
```

## The daemon does not start

**Checks**

1. Run `sudo launchctl print system/dev.seiji.daemon`. Look at the state and the last exit code.
2. Read the end of `/usr/local/var/log/seiji.log`. launchd restarts the daemon (`KeepAlive`), so the same error repeats.
3. Run the same command by hand to see the error at once:
   ```sh
   sudo -u seiji /usr/local/bin/seiji serve -c /usr/local/etc/seiji/seiji.json
   ```

| Error | Cause | Fix |
|---|---|---|
| `reading /usr/local/etc/seiji/seiji.json` | The file is missing or the daemon account cannot read it. | Install it with mode `644` ([install.md](install.md#6-write-the-config-file)). |
| `parsing config` | Bad JSON, a missing required key, a wrong type, or an unknown key. The cause names it. | Fix the key ([configuration.md](configuration.md)). |
| `ios.defaultModel "<name>" is not in ios.models`, `android.defaultModel "<name>" is not in android.models` | The default model is not a key of `models`. | Use one of the keys. |
| `android.consolePortRange must be [even, larger] within 5554-5682` | The first port is odd, the last is not larger, or a port is outside the emulator's console ports. | For example `[5600, 5640]`. |
| `android.bootTimeoutSeconds must be > 0`, `ios.bootTimeoutSeconds must be > 0` | A boot timeout is 0. | Remove the key for the default, or set a positive value. |
| `seats must be >= 1` | `seats` is 0 or negative. | Set at least 1. |
| `capacity values must be >= 0` | `capacity.ios` or `capacity.android` is negative. | Use 0 or more. |
| `defaultQuota: monthlyMinutes must be >= 0 and the other limits >= 1` | A `defaultQuota` value is out of range. | Fix it ([configuration.md](configuration.md#defaultquota)). |
| `listen.allowedHosts must not be empty` | `listen.allowedHosts` is `[]`. | List the hostnames agents use, or remove the key for the default. |
| `maestro.flowTimeoutSeconds, appRetentionHours and maxBuildsPerUser must be > 0` | One of these keys is 0 (or `appRetentionHours` is negative). | Remove the key for the default, or set a positive value. |
| `evidence.retentionHours, maxRunSeconds, maxSteps, maxRunBytes and maxBytesPerUser must be > 0` | One of these `evidence` keys is 0 (or `retentionHours` is negative). | Remove the key for the default, or set a positive value ([configuration.md](configuration.md#evidence)). |
| `axe.stepTimeoutSeconds and xctest.timeoutSeconds must be > 0` | One of these keys is 0. | Remove the key for the default, or set a positive value. |
| `listen.publicUrl must start with https:// or http://` | `listen.publicUrl` has no scheme, for example `"seiji.example.com"`. | Write the full URL: `"https://seiji.example.com"`. |
| `creating <dataDir>/run/pids`, `opening <dataDir>/seiji.db`, or `Operation not permitted` | The daemon account does not own the data directory, often after a `seiji` command ran as root. At each start Seiji sets the mode of `<dataDir>`, `run` and `run/pids`, which only their owner may do. | `sudo chown -R seiji /usr/local/var/seiji`. Then run the `user` and `usage` commands with `sudo -u seiji`. |
| `binding <host>:<port>` | Another process uses the port, or the address is not on this host. | Find it with `sudo lsof -nP -iTCP:4780 -sTCP:LISTEN`. Change `listen.port` or stop the other process. |

An SQLite error that says the database is read-only has the same cause as `Operation not permitted`: a root-owned `seiji.db`, `seiji.db-wal` or `seiji.db-shm`.

## Tool calls get 401

**Sign:** `/mcp` or `POST /v1/apps` answers `401` with the body `missing, invalid or disabled bearer token`. The agent's client shows that it cannot connect or authorize.

**Checks**

1. Send a request with the token. A valid token gets `400` (no `X-Filename`), a refused one gets `401`:
   ```sh
   curl -s -o /dev/null -w '%{http_code}\n' -X POST https://seiji.example.com/v1/apps -H "Authorization: Bearer $SEIJI_TOKEN"
   ```
2. Run `sudo -u seiji seiji user list`. Look for the user and its status.

| Cause | Fix |
|---|---|
| The header is missing or malformed. Seiji accepts only `Authorization: Bearer <token>`, with `Bearer` capitalized and one space. | Fix the client's header ([install.md](install.md#12-connect-agents)). |
| The token was rotated. | Use the token from the last `seiji user rotate-token`. |
| The token was lost or mistyped. Seiji keeps only its hash. | `sudo -u seiji seiji user rotate-token <name>`. |
| The user is disabled. | `sudo -u seiji seiji user enable <name>`. |
| The user was removed. | Add the user again. |
| A proxy drops the `Authorization` header. | Forward it. Caddy's `reverse_proxy` forwards it by default. |

Within Seiji, `401` comes before the `Host` check. Its `403` with `Forbidden: Host header is not allowed` means that the token was accepted. A `403` generated by a proxy does not prove that authentication reached Seiji.

## The Host header is rejected

**Sign:** `/mcp` answers `403` with `Forbidden: Host header is not allowed`. This text comes from the `rmcp` library. With `RUST_LOG=seiji=info,rmcp=warn`, the log shows `rejected request with disallowed Host header (possible DNS rebinding attempt)`.

**Cause:** The `Host` the client sends is not in `listen.allowedHosts`. The default list has only `localhost` and `127.0.0.1`. A proxy like Caddy forwards the public hostname as `Host`.

**Fix:** Add the hostname to `listen.allowedHosts` and restart the daemon. An entry without a port matches every port. A request with no `Host` at all gets `400` with `Bad Request: missing Host header`.

Cloudflare `403` with `error code: 1010` instead indicates a Browser Integrity Check rejection, before Seiji. The CLI identifies HTTP calls and forward WebSocket handshakes with `User-Agent: seiji/<version>`; Tern requests use `seiji-tern/1`. Other programmatic clients should send their own truthful product identifier. Check the response body before changing `allowedHosts`; do not impersonate a browser or disable the zone-wide browser check to diagnose this error.

## Long tool calls fail at the proxy

**Sign:** `acquire_device`, `run_actions`, `run_flow`, `run_xctest` or `install_app` fails after a fixed time (often 60 s) with a gateway error, while the daemon log shows the operation finishing (for a run, `run recorded`).

**Cause:** Seiji streams each tool call response as SSE with keepalive comments every 15 s. If the proxy's idle read timeout is below 15 s, it cuts the connection between two keepalives. This is rare: most proxies default to 60 s or more. Caddy's `reverse_proxy` has no response timeout by default.

**Fix:** Set the proxy's idle or read timeout above 15 s. nginx: `proxy_read_timeout 60s;` (the default). AWS ALB: idle timeout 60 s (the default). If the proxy cut a run, the run still finishes and is recorded: find it with `list_runs`.

## `acquire_device` is refused

Admission checks, in this order: the user exists and is not disabled, the monthly budget, the user's concurrency, the host's capacity, then the ports. Nothing is created and nothing is billed when it refuses.

| Message | Cause | Fix |
|---|---|---|
| `monthly budget exhausted: <used> of <quota> device-minutes used in <YYYY-MM>` | The user used its `monthlyMinutes` in this UTC month. Boot time counts. | Wait for the next UTC month, or raise the quota: `sudo -u seiji seiji user set <name> --monthly-minutes <N>`. It applies at once. Check with `sudo -u seiji seiji usage`. |
| `you already hold <N> device(s), your limit is <M>; release one first` | The user's `booting` and `ready` leases reach `maxConcurrent`. | Call `release_device`. `budget_status` lists the user's leases. A lease an agent forgot ends after `idleMinutes`. Or raise `--max-concurrent`. |
| `host is full: <in use>/<capacity> <platform> devices in use; try again later` | All users together hold `capacity.<platform>` devices. There is no queue. | Try again later. `list_device_models` shows `in_use` and `capacity`. Shorter `idleMinutes` and `maxLeaseMinutes` free devices sooner. Raise `capacity` only if the host has memory for it ([configuration.md](configuration.md#capacity)). |
| `no free emulator console port in android.consolePortRange` | See [Android emulator problems](#android-emulator-problems). | |
| `ios is not available on this host` / `android is not available on this host` | No `ios` or `android` section in the config. | Add the section, or use the other platform. |
| `unknown <platform> model "<model>"; available: [<models>]` | The `model` is not a key of `<platform>.models`. | Use a listed model, or omit `model` for the default. |

When the budget runs out while the user holds devices, the reaper releases them with `budget_exhausted` within 15 s, and a running flow is killed.

## A device fails to start

**Sign:** `acquire_device` fails with `device failed to start (not billed): <cause>`. The log shows `boot did not complete: <cause>` with `reason` `boot_failed`. The lease ended with `boot_failed`, the device was deleted, and no minute is billed.

**Checks:** Run `sudo -u seiji seiji doctor <user>` (or `--platform android`). It runs the same steps and prints each one.

| Cause in the message | Meaning | Fix |
|---|---|---|
| `creating lease dir exited 1: sudo: a password is required` | The sudoers rule is missing or does not match. | See [`sudo: a password is required`](#sudo-a-password-is-required). |
| `creating lease dir exited 1: Error: exec /bin/mkdir` (with `Permission denied`) | The tenant account cannot reach the pid directory under the data directory. | See [Every command fails with `Permission denied`](#every-command-fails-with-permission-denied-under-os_user). |
| `creating lease dir exited 1: Error: <account error>` | The root helper refused the tenant account. | See [Tenant account errors](#tenant-account-errors). |
| `simctl create exited <code>: <simctl output>` | simctl cannot create the device. Usually the `deviceType` or `runtime` does not exist, the runtime is not installed, or Xcode's first launch was not done. | Compare `ios.models` with `xcrun simctl list devicetypes` and `xcrun simctl list runtimes`. Run `sudo xcodebuild -runFirstLaunch` and `xcodebuild -downloadPlatform iOS`. |
| `simctl create returned "<text>"` | simctl printed something that is not one UDID. | Read the text. Run the same `xcrun simctl create` by hand. |
| `simctl bootstatus timed out: <output>` | The simulator did not finish booting in `ios.bootTimeoutSeconds` (180 s). | Raise `ios.bootTimeoutSeconds`. If many devices run, the host may be short of memory: lower `capacity.ios`. |
| `simctl bootstatus exited <code>: <output>` | The boot failed. | Read the output. |
| `emulator exited during boot: <log tail>` or `emulator did not finish booting in <N>s` | See [Android emulator problems](#android-emulator-problems). | |
| `spawning <path>` | The program does not exist (shared mode). | Install it, or fix its path in the config. |

When the lease is released while it boots (by `release_device`, the reaper, `seiji user disable` or a shutdown), `acquire_device` fails with `the lease was released while the device booted` instead. That lease ends with the releaser's end reason and **is** billed for the time it held the host, so acquiring and releasing during boot is not free.

Under `os_user`: if `simctl create` or `simctl bootstatus` fails for the tenant account while the same command works for your admin account, you hit the path that has **not been tested** on a real host: simctl for an account that never logged in to the GUI, through `launchctl asuser`. Switch to `isolation.mode: "shared"` ([configuration.md](configuration.md#isolation)) and read what that changes in [security.md](security.md).

## Android emulator problems

> **Not tested:** Android emulators on an EC2 Mac (Hypervisor.framework on bare metal) have not been verified.

| Message | Cause | Fix |
|---|---|---|
| `emulator exited during boot: <log tail>` | The emulator stopped. The tail of its log says why: an AVD it cannot find or read, no hardware acceleration, or a port in use. | Check that `android.models.<name>.avd` names an AVD in `android.avdHome` and that the `seiji-tenants` group can read it (`ls -l /opt/android-avd`). Check acceleration with `/opt/android-sdk/emulator/emulator -accel-check`. |
| `emulator did not finish booting in <N>s` | `sys.boot_completed` did not become `1` within `android.bootTimeoutSeconds` (240 s). The default `-gpu swiftshader_indirect` renders on the CPU and is slow. | Raise `android.bootTimeoutSeconds`. Lower `capacity.android` if the host is busy. |
| `no free emulator console port in android.consolePortRange` | Every even port in the range is used by an active lease, or that port or the next one is bound by another process, often an emulator left over after a crash. | Make the range give at least `capacity.android` slots ([configuration.md](configuration.md#android)). Find leftover emulators ([Leftover devices after a crash](#leftover-devices-after-a-crash)). Check a port with `lsof -nP -iTCP:5600`. |
| `adb install exited <code>: <output>` | adb could not install the APK. | Read adb's output. |

All emulators share one adb server on port 5037, because Maestro hardcodes that port. `adb devices` lists every user's emulators.

## `sudo: a password is required`

**Sign:** Under `os_user`, a command error ends with `sudo: a password is required`. For example `creating lease dir exited 1: sudo: a password is required`, or in `seiji doctor`: `id exited 1: sudo: a password is required`. At start-up, `orphan scan:` lines with the same text.

**Cause:** The daemon runs `sudo -n <isolation.helper> exec-as …` and `sudo -n <isolation.helper> kill-as …`. `-n` never asks for a password, so sudo fails when no rule allows the exact command.

**Checks**

1. `sudo visudo -cf /etc/sudoers.d/seiji` (the file exists and parses).
2. `sudo -l -U seiji` (it lists `/usr/local/bin/seiji exec-as *` and `/usr/local/bin/seiji kill-as *`).
3. `isolation.helper` in the config is the same path as in the sudoers rule.
4. The command runs as the daemon account `seiji`, the name the rule uses: the plist's `UserName`, or `sudo -u seiji` for `doctor`.

**Fix:** Install the rule ([install.md](install.md#7-install-the-sudoers-rule)):

```sh
sudo visudo -cf deploy/sudoers.seiji
```

```sh
sudo install -m 440 deploy/sudoers.seiji /etc/sudoers.d/seiji
```

If `kill-as` fails, Seiji cannot stop that process group, and an emulator or a Maestro process can outlive its lease. The daemon log shows it (fields `pgid` and `sig`):

| Log line | Cause | Fix |
|---|---|---|
| `kill-as refused: <stderr>` | sudo or the helper refused the call. `sudo: a password is required` means the rule does not allow it; a tenant account error comes from the helper's checks. | Fix the sudoers rule as above, or the tenant account ([Tenant account errors](#tenant-account-errors)). |
| `kill-as did not start: <error>` | The daemon could not run `/usr/bin/sudo` at all. | Check that `/usr/bin/sudo` exists and that the daemon can run it. |

## Tenant account errors

These come from `seiji user add`, `seiji tenant provision`, or a root helper (inside a command error, for example `creating lease dir exited 1: Error: …`).

| Message | Cause | Fix |
|---|---|---|
| ``run `sudo seiji tenant provision <name>` first`` (caused by `no macOS account "<account>"`) | `seiji user add` under `os_user` before the tenant account exists. | `sudo seiji tenant provision <name>`, then add the user again. |
| ``macOS account "<account>" is missing; run `sudo seiji tenant provision` `` | The user exists in Seiji, but its tenant account was deleted. | `sudo seiji tenant provision <name>`. |
| `"<account>" is not in "seiji-tenants"` | The account's group membership was removed, or the account was made by hand. | Only for an account you mean as a tenant: `sudo dseditgroup -o edit -a <account> -t user seiji-tenants`. `seiji tenant provision` does not add an existing account to the group. |
| `"<account>" already exists and is not a Seiji tenant account; refusing to change it` | `seiji tenant provision` found an account with that name that is not a tenant account. It never changes such an account. | Choose another user name or `isolation.userPrefix`, or add the account to the group as above if it really is meant as a tenant. |
| `"<account>" is an admin; refusing` | The account is in the `admin` group. The helpers never act as an admin. | `sudo dseditgroup -o edit -d <account> -t user admin` |
| `"<account>" is a system account` | The account's uid is below 501. | Use another user name or `isolation.userPrefix`. |
| `no group "seiji-tenants"` | The group does not exist. | `sudo seiji tenant provision <name>` creates it. |
| `must run as root (through sudo)` | `seiji tenant provision` (or a root helper) ran without root. | Run it with `sudo`. |
| `user "<name>" has no macOS account` | The user was added under `shared`, and the mode is now `os_user`. | Remove the user and add it again under `os_user`. |
| `account names may only use [A-Za-z0-9_-]` | `isolation.userPrefix` has other characters. | Fix the prefix. |
| `user names must be 1-32 chars of [a-z0-9-]` | The user name is not valid. | Use lower-case letters, digits and `-`. |

Since the account is resolved during admission, a missing account fails `acquire_device` before any lease exists.

## Every command fails with `Permission denied` under `os_user`

**Sign:** Command errors such as `creating lease dir exited 1: Error: exec /bin/mkdir` with `Caused by: Permission denied (os error 13)`. `seiji doctor` fails at `identity`.

**Cause:** Under `os_user`, `exec-drop` writes the process-group id of every command to `<dataDir>/run/pids/<uuid>.pid`, as the tenant account, before it runs the command. The tenant account needs search permission on the data directory and every parent of it. Seiji sets `<dataDir>` and `run` to `0711` and `run/pids` to `1733` at each start of `serve` and `doctor`, but it does not touch the parents. A data directory inside a private home (mode `700`) fails this way.

**Checks:** List the modes of each directory on the path:

```sh
ls -ld /usr /usr/local /usr/local/var /usr/local/var/seiji
```

**Fix:** Keep `dataDir` at `/usr/local/var/seiji` ([install.md](install.md#5-create-the-data-and-config-directories)), or give every parent the search bit for others (`o+x`).

## Maestro does not start

**Sign:** A `run_flow` run ends `failed` with the verdict `the flow failed (exit 1): <last line>`, where the last line is an error from the launcher, not from a flow step. Or the run ends `error` with `spawning <path>` in shared mode. `inspect_screen` on Android fails with `maestro hierarchy exited 1: <output>`. `seiji doctor` fails at `maestro --version`. Maestro's whole output is in the run's `maestro/output.txt`.

| Output | Cause | Fix |
|---|---|---|
| ``Maestro is not configured on this host (`maestro` in seiji.json)`` | `run_flow`, or `inspect_screen` on Android, on a host without the `maestro` section. | Install Maestro and add the section ([install.md](install.md#maestro-under-optmaestro-optional)), or use `run_actions` on iOS, which does not need Maestro. |
| `spawning /opt/maestro/bin/maestro` with `No such file or directory` (shared) or `Error: exec /opt/maestro/bin/maestro` with `No such file or directory` (`os_user`) | Maestro is not at `maestro.bin`. | Install it under `/opt/maestro` ([install.md](install.md#2-prepare-the-host)) or set `maestro.bin`. |
| `Error: exec /opt/maestro/bin/maestro` with `Permission denied` | The tenant account cannot read or run Maestro. | `sudo chmod -R a+rX /opt/maestro` |
| The launcher says that `JAVA_HOME` is not set or invalid, or that it cannot find Java | Tenant accounts have no shell profile and a fixed `PATH`, so Maestro does not find Java 17. | Set `maestro.javaHome` to a JDK 17 home that every account can read, for example `/opt/homebrew/opt/openjdk@17`. Check it with `sudo -u seiji-alice /opt/homebrew/opt/openjdk@17/bin/java -version`. |

## Flows time out

**Sign:** A `run_flow` run ends `failed` with the verdict `the flow timed out (maestro.flowTimeoutSeconds)`. `inspect_screen` on Android fails with `maestro hierarchy timed out: <output>`.

**Cause:** One flow can run for `maestro.flowTimeoutSeconds` (600 s by default); `maestro hierarchy` for 120 s (fixed). On timeout Seiji kills Maestro's whole process group. The common causes: the flow waits for an element that never appears, the first flow on a new device spends time setting up Maestro's driver, or the host is short of CPU or memory.

**Fix:**

- Call `inspect_screen` and use the exact texts and ids it shows in selectors.
- Split long flows.
- Raise `maestro.flowTimeoutSeconds`, then restart the daemon. Raise the proxy timeout with it ([Long tool calls fail at the proxy](#long-tool-calls-fail-at-the-proxy)).
- Lower `capacity` if the host is overloaded.

## `<what> supports iOS simulators only for now`

**Sign:** A tool fails with `<what> supports iOS simulators only for now`. `<what>` names the tool or the operation: `run_actions`, `run_xctest`, `launch_app`, `terminate_app`, `uninstall_app`, `open_url`, `push_notification` or `get_logs`. `configure_device` puts the operation first, for example `status_bar: status_bar supports iOS simulators only for now` (also `appearance`, `location`, `permissions`, `react_native_bundler`).

**Cause:** The lease holds an Android emulator. These tools use `xcrun simctl`, AXe or `xcodebuild`, which exist only for iOS simulators. Nothing ran.

**Fix:** Acquire an iOS device (`acquire_device` with `platform: "ios"`). On Android, use `run_flow` (it needs Maestro), `inspect_screen`, `take_screenshot` and `install_app`.

`configure_device` is not atomic: it applies its operations in order and stops at the first error, and the operations before it stay applied.

## AXe does not start

**Sign:** `seiji doctor` fails at `axe --version` or `accessibility tree (axe describe-ui)`. Or every `run_actions` run fails at its first screen step, and `inspect_screen` on iOS fails, with `<what> failed: <output>` (for example `tap failed: <output>` or `describe-ui failed: <output>`): AXe printed no JSON result, so Seiji quotes the last 2000 bytes of its output.

| Output | Cause | Fix |
|---|---|---|
| `spawning /opt/homebrew/bin/axe` with `No such file or directory` (shared) or `Error: exec /opt/homebrew/bin/axe` with `No such file or directory` (`os_user`) | AXe is not at `axe.bin`. | `brew install cameroncooke/axe/axe` ([install.md](install.md#axe)), or set `axe.bin` to its path. |
| `Error: exec /opt/homebrew/bin/axe` with `Permission denied` | The tenant account cannot read or run AXe. | `sudo chmod -R a+rX "$(brew --prefix axe)/"`. Check that every parent of the path is searchable by others (`ls -ld /opt/homebrew /opt/homebrew/bin`). |
| AXe starts but fails for every command after an Xcode update | AXe drives the simulator through Apple's private accessibility and HID frameworks. A new Xcode can break it until AXe ships a fix. | `brew upgrade axe`. If no fix exists yet, select the previous Xcode with `sudo xcode-select -s <path>`. Run `seiji doctor` again. |
| AXe works for your admin account but fails for the tenant account | The tenant's runtime or automation service is not ready, or its user context differs. | Run `sudo -u seiji seiji doctor <user>` for each tenant and inspect the exact runtime build and native diagnostics. Preserve `os_user` isolation; changing to a shared account does not fix the tenant failure. |
| `describe-ui timed out`, `axe --version exited <code>: <output>` | AXe hung, or exited with an error. | Read the output. A command that hangs ends after `axe.stepTimeoutSeconds` (60 s); `axe --version` in `doctor` after 30 s. |

## A step fails with an AXe error

**Sign:** A `run_actions` run ends `failed` with the verdict `step <n> (<kind>) failed: <error>`, and `(+<N> more)` when other steps failed too (with `continue_on_error`). The step's `error` has one of these forms. `<what>` is the AXe command: `tap`, `type`, `swipe`, `scroll`, `button`, `key` or `describe-ui`.

| Error | Cause | Fix |
|---|---|---|
| `<what> failed (<code>): <message>` | AXe ran and returned a typed error. `<code>` is AXe's error code, `commandFailed` when AXe gives none. For example `tap failed (elementNotFound): No accessibility element matched --label 'X'.` | Call `inspect_screen` and use the exact `label`, `id` or `value` it shows. Look at the failed step's files: `steps/<NNN>-<kind>-failure.png` and the tree `steps/<NNN>-<kind>-tree.txt`. |
| `<what> timed out` | One AXe command took longer than `axe.stepTimeoutSeconds` (60 s). | A `tap` with a selector waits for its element with `timeout_seconds` (default 5 s) inside that limit. Raise `axe.stepTimeoutSeconds` only if the host is slow. |
| `<what> was cancelled` | The lease was released during the step. | See [A run ends `failed`, `error` or `cancelled`](#a-run-ends-failed-error-or-cancelled). |
| `<what> failed: <output>` | AXe printed no JSON result. | See [AXe does not start](#axe-does-not-start). |
| `<selector> is not on screen after <N>s`, `<selector> is still on screen after <N>s` | A `wait_for`, `assert_visible` or `assert_not_visible` step did not see the expected state. `<selector>` lists the criteria, for example `label="Sign in" type="Button"`. | Check the tree as above. `assert_visible` and `assert_not_visible` wait 2 s; `wait_for` waits `timeout_seconds` (default 10 s). |
| `no bundle_id given and no app installed on this lease` | A `launch_app` or `terminate_app` step has no `bundle_id`, and no app was installed with `install_app` on this lease. | Pass `bundle_id`, or install the app first. |

After a failed step the run stops, and the later steps are `skipped` with `an earlier step failed`, unless `continue_on_error` is set.

## A run is refused before it starts

These errors come back from `run_actions`, `run_flow` or `run_xctest` instead of a `run_id`. Nothing was recorded.

| Message | Cause | Fix |
|---|---|---|
| `evidence quota full: <held> of <max> bytes kept or reserved by running runs; runs expire <hours>h after they end` | The user's kept runs, plus the byte budget that the user's running runs reserve, reach `evidence.maxBytesPerUser`. | Wait for the running runs to end (a reservation then shrinks to the run's real size), or until old runs expire after `evidence.retentionHours`; there is no tool to delete a run earlier. Or raise `evidence.maxBytesPerUser` and restart the daemon. Smaller runs help: `record_video: false`, `screenshot_each_step: false`. |
| `run_actions takes 1-<maxSteps> actions` | The `actions` list is empty or longer than `evidence.maxSteps` (200). | Split the run. |
| `action <n> (<kind>): <reason>` | Action `<n>` (from 1) is not valid, for example `action 2 (tap): tap needs exactly one of id, label or value, or both x and y`, or for `scroll`: `scroll: until needs exactly one of id or label, and nothing else`, `scroll: max_swipes needs until`, `scroll: max_swipes must be 1-50`. | Fix that action. |
| `name must be 1-200 characters`, `at most 10 tags`, `log_predicate must be 1-2000 characters` | A run option is out of range. | Fix the option. Tags use `[A-Za-z0-9_-]`, 1 to 64 characters. |
| ``Maestro is not configured on this host (`maestro` in seiji.json)`` | `run_flow` on a host without Maestro. | See [Maestro does not start](#maestro-does-not-start). |
| `env key "<key>" must match [A-Za-z0-9_]+` | A `run_flow` `env` key has other characters, is empty, or is longer than 128 characters. | Rename the key. |
| `run_actions supports iOS simulators only for now`, `run_xctest supports iOS simulators only for now` | The lease is Android. | See [`<what> supports iOS simulators only for now`](#what-supports-ios-simulators-only-for-now). |
| `run_xctest is turned off on this host (xctest.enabled)` | The host sets `xctest.enabled` to `false`. | Ask the operator, or use `run_actions`. |
| `no uploaded app "<app_id>"` | `run_xctest` with a wrong or expired `app_id`. | Upload the bundle again (`POST /v1/apps`). |
| `xctestrun must be a file name ending in .xctestrun`, `test identifier "<id>" must be Target[/Class[/method]] of [A-Za-z0-9_./-]`, `test_configuration must be 1-200 characters on one line` | A `run_xctest` option is not valid. | Fix the option. |
| `preparing the run exited <code>: <output>` | The tenant account could not create its run directory in the lease dir. | See [Every command fails with `Permission denied`](#every-command-fails-with-permission-denied-under-os_user). |

## A run ends `failed`, `error` or `cancelled`

A run that does not pass is an error result (`isError`) with a screenshot, but it is still recorded: its `run_id`, `status`, `verdict` and evidence are there. The `verdict` says why. Every run ends with a manifest; no run stays `running`.

| Status | Verdict | Meaning |
|---|---|---|
| `passed` | `<N> of <N> steps passed`, `the flow passed`, or `<N> tests: <p> passed, <f> failed, <s> skipped` | Every step or test passed. |
| `failed` | `step <n> (<kind>) failed: <error>` | A `run_actions` step failed ([A step fails with an AXe error](#a-step-fails-with-an-axe-error)). |
| `failed` | `the flow failed (exit <code>): <last line>`, `the flow timed out (maestro.flowTimeoutSeconds)` | The Maestro flow failed or timed out. |
| `failed` | `<N> tests: <p> passed, <f> failed, <s> skipped`, `xcodebuild timed out (xctest.timeoutSeconds)` | Tests failed (`xcodebuild` exited 65), or the run timed out ([`run_xctest` problems](#run_xctest-problems)). |
| `failed` | `<P> of <N> steps ran: the run reached evidence.maxRunSeconds` | The time limit fell between two `run_actions` steps ([below](#the-run-reached-evidencemaxrunseconds)). |
| `error` | The error, for example `spawning <path>` or `xcodebuild exited <code>: <output>` | The engine could not run. |
| `error` | The error, with the warning `recording the run failed` | The run executed, but copying, hashing or writing its evidence failed. The tool call fails with `run <run id> failed while recording its evidence: <cause>`. The manifest lists the files that exist. Read the cause; it is often a full disk. |
| `error` | `server restarted during the run` | See below. |
| `cancelled` | `the lease was released during the run: <verdict>`, `the lease was released`, or `<p> of <N> steps ran` | The lease ended during the run (`release_device`, the reaper, `seiji user disable`, or a shutdown); `cancelled` means nothing else. The evidence up to that point is kept. |

### `the run reached evidence.maxRunSeconds`

**Sign:** A `run_actions` step fails with `the run reached evidence.maxRunSeconds`, and the later steps are `skipped` with the same text. When the limit falls between two steps, no step fails; the run is `failed` with the verdict `<P> of <N> steps ran: the run reached evidence.maxRunSeconds`.

**Cause:** The whole `run_actions` call ran longer than `evidence.maxRunSeconds` (1800 s by default). Long `wait_for`, `sleep` and `scroll` steps add up.

**Fix:** Split the steps into several runs, or shorten the waits. To raise `evidence.maxRunSeconds`, restart the daemon after the change, and raise the proxy timeout with it ([Long tool calls fail at the proxy](#long-tool-calls-fail-at-the-proxy)).

### `server restarted during the run`

**Sign:** `list_runs` shows a run with status `error` and the verdict `server restarted during the run`. Its manifest has the warning `the server restarted during the run; only files written before that are kept`. The log has `run interrupted by the restart is marked as an error`.

**Cause:** The daemon stopped while the run executed: a crash, `SIGKILL`, a power loss, or launchd killing it after `ExitTimeOut` (120 s) during a shutdown that was still collecting evidence. At the next start, recovery closes every `running` run: it writes an `error` manifest that lists the files already in `<dataDir>/runs/<user id>/<run id>/`, counts their bytes against the user's evidence quota, and ends the lease with `server_restart`. Files the tenant had not yet copied out (the video, screenshots, crash reports) are lost with the lease dir.

**Fix:** Read what was kept with `get_run`, then acquire a device and run it again.

## `run_xctest` problems

`run_xctest` copies the bundle into the lease dir, unpacks it, finds the `.xctestrun` file (within four levels), and runs `/usr/bin/xcrun xcodebuild test-without-building -xctestrun <file> -destination "platform=iOS Simulator,id=<udid>"` as the user's account. Then it exports the results with `xcrun xcresulttool`. The status comes from xcodebuild's exit code:

| Exit code | Status | Verdict |
|---|---|---|
| `0` | `passed` | `<N> tests: <p> passed, <f> failed, <s> skipped` |
| `65` | `failed` | The same count. xcodebuild exits 65 when tests fail. |
| Any other code, or a signal | `error` | `xcodebuild exited <code>: <last 400 characters of the output>` or `xcodebuild exited by signal: <output>` |

Every outcome keeps `xctest/xcodebuild.log` and `xctest/output-tail.txt` (the last 40 lines of the log). Read them with `get_run_artifact`.

| Message | Cause | Fix |
|---|---|---|
| `xcodebuild exited 2: no .xctestrun file in the bundle` | The archive has no `*.xctestrun` within four levels, or none with the name given in `xctestrun`. | Upload a `.zip` or `.tar.gz` of the `build-for-testing` Products folder (`Build/Products`, with the `.xctestrun` file and the `Debug-iphonesimulator` folder). |
| `xcodebuild exited 2: XCTest bundles must be a .zip or .tar.gz of the build-for-testing Products folder` | The file name does not end with `.zip`, `.tar.gz` or `.tgz`. | Upload it with one of these extensions. |
| `xcodebuild exited <code>: <output>`, another code | xcodebuild could not run the tests: for example a device build instead of a simulator build, or a bundle built with another Xcode. | Read `xctest/xcodebuild.log`. Build with `-sdk iphonesimulator`, on the same Xcode as the host. |
| `xcodebuild timed out (xctest.timeoutSeconds)` | The tests ran longer than `xctest.timeoutSeconds` (3600 s). | Split the tests with `only_testing`, or raise `xctest.timeoutSeconds` and the proxy timeout. |
| `copying app exited <code>: <output>` | The bundle could not be copied into the lease dir. | Read the output. |
| The verdict says `0 tests` although tests ran | Seiji could not read `xctest/xcresult-tests.json`: `xcresulttool` failed, or its JSON has a shape Seiji does not know. The exact shape of `xcresulttool get test-results tests` has **not been verified** on a real host. | The status still comes from the exit code, and `xctest/result.xcresult.zip` holds the full results. Check that `xcrun xcresulttool get test-results --help` works on the host. |

An XCTest bundle runs any code it contains as the user's account; in `shared` isolation that is the daemon account ([security.md](security.md)). Set `xctest.enabled` to `false` to turn `run_xctest` off ([configuration.md](configuration.md#xctest)).

## Evidence is missing from a run

The run result and the manifest list what was kept: `video` (true or false), `crash_reports`, `dropped_files` and `warnings`. Each warning names the part that failed. None of them fails the run.

| Symptom or warning | Cause | Fix |
|---|---|---|
| No `video.mp4` (`"video": false`), with the warning `video: <cause>` | `xcrun simctl io <udid> recordVideo` could not start. | Read the cause and recorder output. Seiji waits up to 60 seconds for `Recording started` before running actions; an early exit or startup timeout fails capture and terminates the recorder. Run `seiji doctor <user>`: its three-second capture starts only after that readiness signal. |
| No `video.mp4` and no `video:` warning | The run had `record_video: false`, the lease is Android (Seiji records only iOS), or the file did not fit and is listed under `dropped`. A warning `copying video.mp4: <cause>` means the copy failed. | Look at `dropped` and the other warnings. |
| `app.log was cut to <N> bytes` | The log stream grew past half of the run's byte budget ([configuration.md](configuration.md#evidence)). Seiji kept its first `<N>` bytes. | Pass a narrower `log_predicate`, or raise `evidence.maxRunBytes`. When the user is close to `evidence.maxBytesPerUser`, the budget is smaller. |
| `logs: no app is installed on this lease; pass log_predicate` | `capture_logs` is on, no `log_predicate` was given, and no app was installed on this lease with `install_app` (or it was uninstalled). The default predicate needs the app's bundle id. There is no `app.log`. | Install the app first, pass `log_predicate`, or set `capture_logs: false`. |
| `logs: <cause>` | `xcrun simctl spawn <udid> log stream` could not start. | Read the cause. |
| `crash reports: <cause>` | Seiji could not copy the crash reports, for example `crash reports: executable name "<name>" is not safe to search for`. | Read the cause. Seiji looks for `<executable>-*.ips` in the account's `~/Library/Logs/DiagnosticReports` written during the run, only when the installed app has an executable. Under `shared` that folder is the daemon account's, shared by every user, so a report from another user's run of the same app in the same time can appear. |
| `dropped_files` above 0; the Markdown summary says `<N> file(s) were not kept: the run hit its evidence size limit.` | The files did not fit in the run's byte budget, the smaller of `evidence.maxRunBytes` and what is left of `evidence.maxBytesPerUser`. | Raise those keys, or record less: `screenshot_each_step: false`, `record_video: false`. |
| `copying <path>: <cause>`, `listing the run's files: <output>`, `skipped a file with an unusable name: "<name>"` | Seiji could not read a file as the tenant account, could not list the run directory, or could not clean a path (more than 8 levels deep, or a `..` component). | Read the cause. |
| `skipped "<rel>": <clean> already exists in the evidence` | A tenant file cleans to a path that is already in the evidence: one of Seiji's own files (`app.log`, `maestro/output.txt`, …) or an earlier tenant file with a similar name. Seiji keeps the first one. | Give the files distinct names with `[A-Za-z0-9._-]`. |
| `removing the tenant's copy: <cause>` | The tenant's run directory could not be deleted. It goes with the lease dir at release. | None needed. |

## Run files cannot be downloaded

`GET /v1/runs/<run id>/files/<path>`, `/evidence.zip` and `/report.md` need the same bearer token as `/mcp` (`401` otherwise), and serve only the token's own runs. `get_run` and `get_run_artifact` fail with the same texts.

| Message | Status | Cause and fix |
|---|---|---|
| `no run "<run id>" for you` | `404` | The id is wrong, the run belongs to another user, or the reaper deleted it after `evidence.retentionHours`. |
| `run <run id> is still running` | `404` | Wait for the tool call that started it to return. |
| `the run has no manifest yet` | `404` | Seiji could not write the run's manifest (the log has `writing its manifest:` or `closing a failed run:`). Check the free disk space. |
| `status must be running, passed, failed, error or cancelled` | (`list_runs`) | Pass one of these values as `status`. |
| `the run has no file "<path>"` | `404` | Seiji serves only the files the manifest lists, plus `manifest.json`. Use the `path` exactly as `get_run` lists it. A file under `dropped` was never kept. |
| `ditto failed: <cause>` | `500` | `evidence.zip` could not be built in `<dataDir>/run/`. Seiji builds it again for every request. Check the free disk space. |

Links in tool results are server-relative (`/v1/runs/<run id>/…`) unless `listen.publicUrl` is set ([configuration.md](configuration.md#listen)). A client that cannot follow a relative link needs `listen.publicUrl`.

## `get_logs` errors

| Message | Cause | Fix |
|---|---|---|
| `last_seconds must be 1-3600` | `last_seconds` is out of range. | Use 1 to 3600. |
| `get_logs supports iOS simulators only for now` | The lease is Android. | See [`<what> supports iOS simulators only for now`](#what-supports-ios-simulators-only-for-now). |
| `simctl get_logs exited <code>: <output>` | `xcrun simctl spawn <udid> log show` failed, often on a predicate it cannot parse. | Fix `predicate`. |
| `no app is installed on this lease; pass predicate` | No `predicate`, and no app was installed on this lease with `install_app`. | Pass `predicate`, or install the app first. |

Without `predicate`, `get_logs` uses the installed app's process and subsystem. The result keeps the last 200,000 bytes.

## A lease ends during an operation

| Message | Cause |
|---|---|
| `lease <lease id> was released during the operation (<end reason>)` | The lease ended while the tool ran: `budget_exhausted`, `max_lease_duration`, `user_disabled` or `server_shutdown`. The operation was cancelled. |
| `lease <lease id> was released` | The lease ended just before the operation started. |
| `lease is being released` | Release is in progress. |
| `lease <lease id> is still booting` | `acquire_device` has not returned yet. |
| `you hold no device; call acquire_device first` | No active lease. A lease released for `idle` also ends here: `idleMinutes` passed with no tool call. |
| `you hold <N> devices; pass lease_id` | The user holds more than one device, so `lease_id` is required. |
| `lease <lease id> was already released` | `release_device` on a lease that another release ended at the same time. |
| `no active lease "<lease id>" for you` | The lease ended, or belongs to another user. |

[concepts.md](concepts.md) explains the end reasons.

## App upload and install errors

| Message | Where | Cause and fix |
|---|---|---|
| `X-Filename header is required (e.g. MyApp.zip or app-debug.apk)` | `POST /v1/apps`, `400` | Send the file name in `X-Filename`. |
| `empty upload` | `POST /v1/apps`, `400` | The body was empty. Use `curl --data-binary @file`, not `-d`. |
| `upload failed: <cause>` | `POST /v1/apps`, `400` | Reading the body failed, for example when the client closed the connection. |
| `upload exceeds maxUploadBytes (<N> bytes)` | `POST /v1/apps`, `413` | The build is larger than `maxUploadBytes`. Seiji deletes the partial file. |
| `you already keep <N> builds, the limit is <M>; they expire <H>h after upload` | `POST /v1/apps` (`429`) and `install_app` with `url` | The user keeps `maxBuildsPerUser` builds. Wait until the oldest ones expire after `appRetentionHours`, or raise `maxBuildsPerUser`. |
| `pass exactly one of app_id or url` | `install_app` | Pass `app_id` or `url`, not both and not neither. |
| `no uploaded app "<app_id>"` | `install_app` | The id is wrong, belongs to another user, or the build expired after `appRetentionHours`. Upload it again. |
| `only https URLs are accepted`, `invalid url`, `url has no file name` | `install_app` with `url` | Use an `https` URL whose path ends with the file name. |
| `download exceeds <N> bytes` | `install_app` with `url` | The build is larger than `maxUploadBytes`. |
| An error that mentions `redirect to a non-https URL` or `too many redirects` | `install_app` with `url` | Seiji follows at most 5 redirects, all `https`. Use the final `https` URL. The download also stops after 600 s. |
| `installing app exited 2: iOS apps must be a .zip or .tar.gz containing a simulator .app bundle` | iOS | The file name must end with `.zip`, `.tar.gz` or `.tgz`. |
| `installing app exited 2: no .app bundle inside the archive` | iOS | The archive has no `*.app` directory within three levels, and no `Info.plist` at its top. An archive of a bundle's contents (Expo Go's simulator build) installs. |
| `installing app exited <code>: <simctl output>` | iOS | Often a device build. Simulators need a simulator build (`-sdk iphonesimulator`). |
| `Android apps must be an .apk` | Android | Upload an `.apk`. |
| `installing app timed out`, `adb install timed out` | both | The install took more than 600 s. |

Seiji replaces characters outside `[A-Za-z0-9._-]` in file names with `_` and removes leading dots. The extension must still be right after that.

## Screenshots

Seiji captures a PNG (`xcrun simctl io <udid> screenshot --type=png --display=1`, or `adb exec-out screencap -p`) and converts it with `/usr/bin/sips -Z 2000 -s format jpeg -s formatOptions 85`. The longest side is at most 2000 px, because vision models reject larger images. The size and quality are fixed in the code; there is no config key.

iOS screenshots and `recordVideo` explicitly select primary display 1. A multi-display simulator such as iPhone Duo otherwise fails with `Cannot choose between 2 displays`. This applies to doctor, standalone screenshots and recorded runs.

For iOS, Seiji waits for a nonempty AXe accessibility tree after `simctl bootstatus`, within the same configured boot timeout. A successful `bootstatus` exit alone does not mean the interface is ready; it can return while the display still shows the Apple logo. During a cold boot, AXe can report `commandFailed: Timed out creating the simulator remote automation session` while XCTest is still waiting for accessibility to load. Only that specific startup error remains pending within the boot deadline; other AXe errors and malformed responses fail boot. Doctor also rejects an empty tree before reporting success.

| Message | Cause | Fix |
|---|---|---|
| `taking screenshot exited <code>: <output>` | simctl or adb could not capture the screen. | Make sure the device is booted (`budget_status` shows `ready`). |
| `encoding screenshot exited <code>: <output>` | `sips` failed, often on an empty PNG. | Take the screenshot again. |

When a run does not pass, Seiji adds a screenshot of the screen to the result. If that capture fails, the result has no screenshot and no error about it. A `cancelled` run has no screenshot, because the device is being released.

## The viewer

The viewer shows a lease's iOS simulator live ([tern.md](tern.md)). Its routes and errors are in [mcp.md](mcp.md#viewer-routes).

### The viewer link has expired

**Sign:** The browser shows `This viewer link expired or was already used. Open the device again from Tern.` (`401`). Or an open page stops with `This viewer link expired. Open the device again from Tern.`

| Cause | Fix |
|---|---|
| The link is older than 60 s, or someone opened it already. A ticket works once. | Ask for a new link: `share_screen`, or open the device again in Tern. |
| The viewer session is older than `viewer.sessionHours` (default 12). | Open a new link. |
| The daemon restarted. Viewer sessions live in memory. | Open a new link. The restart also ended the lease, so acquire a device first. |
| An admin disabled or removed the user. | `sudo -u seiji seiji user enable <name>`. |
| The browser did not send the cookie. Without an `https://` `listen.publicUrl` the cookie is not `Secure`; with one, the page must load over HTTPS. | Open the link on the URL in `listen.publicUrl`, over HTTPS ([security.md](security.md#the-viewer)). |

### Input gets `409` (busy)

**Sign:** Taps and typing do nothing, the status dot is amber, and input answers `409` with `A run is using the device. Input works again when it ends.`

**Cause:** A tool call holds the device, for example a `run_actions` run. Input waits up to 1 s for the lease's operation lock, then gives up, so it never mixes with a run's steps. The picture keeps streaming.

**Fix:** Wait until the run ends. Input works again on its own.

A view-only session answers `403` with `This viewer is view-only.` for every input. Ask for a link with `control: true`.

### The picture stays black or keeps reconnecting

**Sign:** The page shows no picture, or `Reconnecting…` again and again. The daemon log has `screen stream: <error>` for the lease.

| Cause | Fix |
|---|---|
| AXe is not at `axe.bin`, or the user's account cannot run it. The stream is `axe stream-video`, and Seiji starts it again every second while someone watches. | See [AXe does not start](#axe-does-not-start). Run `seiji doctor <user>`; it does not test streaming, but it checks that AXe runs. |
| The installed AXe has no `stream-video`, or a new Xcode broke it. | `brew upgrade axe`, then open the link again. |
| The lease is not an iOS simulator. A link for an Android lease fails with `the screen viewer supports iOS simulators only for now`. | Use an iOS lease. On Android, use `take_screenshot`. |
| The lease ended. The page shows `The device was released.` (`410`). | Acquire a device and open a new link. |
| The lease already has `viewer.maxViewersPerLease` streams (`429`, `This device already has <n> viewers open.`). | Close a viewer, or raise `viewer.maxViewersPerLease`. |

> **Not verified:** the viewer stream has not run against a real simulator yet. The `axe stream-video` flags and its framing were checked against AXe's source only.

### The Tern plugin says the CLI is missing

**Sign:** The plugin shows `The seiji CLI is not on Tern's PATH. Install seiji or set SEIJI_BIN to its path.`

**Cause:** The plugin runs `seiji token print --json` to read your token. Tern did not find `seiji`. A Tern started from the Dock does not get your shell's `PATH`.

**Fix:** Install the CLI (`cargo install --git https://github.com/wiljafor1/seiji`). Tern started from the Dock may not have `~/.cargo/bin` on its PATH: run `launchctl setenv SEIJI_BIN "$HOME/.cargo/bin/seiji"` and start Tern again. Then sign in with `seiji login --url <server>` or **Seiji: Sign in** ([tern.md](tern.md)).

When the plugin says you are not signed in, `seiji token print` found no credential: run `seiji login --url <server>`.

## Tunnels

`seiji forward` opens a tunnel, so the device reaches a port on your Mac ([apps.md](apps.md#run-your-dev-server-with-seiji-forward)). Its route and statuses are in [mcp.md](mcp.md#get-v1leaseslease_idforward), its options in [cli.md](cli.md#seiji-forward).

### The server refuses the tunnel

**Sign:** `seiji forward` exits 1 with `the server refused the tunnel (<status>): <message>`.

| Status and message | Cause | Fix |
|---|---|---|
| `403 Forbidden`: `forwarding is off on this server` | The operator set `forward.enabled` to `false` ([configuration.md](configuration.md#forward)). | Install a build instead ([apps.md](apps.md#install-a-build)), or ask the operator. |
| `429 Too Many Requests`: `this lease already has <N> tunnels open` | The lease already has `forward.maxPerLease` tunnels open (default 4). | Stop another `seiji forward` on the same lease, or raise `forward.maxPerLease`. |
| `404 Not Found`: `no active lease "<lease id>" for you`, `lease <lease id> is still booting`, `lease <lease id> is ending` | The lease is not yours, has ended, is still booting, or is being released. | Check the id with `budget_status`. Wait for `acquire_device` to return. |
| `401 Unauthorized`: `missing, invalid or disabled bearer token` | The stored token is wrong, rotated, or the user is disabled. | See [Tool calls get 401](#tool-calls-get-401), then `seiji login --url <server>` again. |

### `seiji forward` cannot pick a lease

| Message | Fix |
|---|---|
| `you hold no ready device; acquire one first (acquire_device, or n in Tern)` | Acquire a device and wait until it is `ready`, then run `seiji forward` again. |
| `you hold <N> devices; pick one with --lease (<lease ids>)` | Pass one of the listed ids: `seiji forward 8081 --lease l_…`. |

### `nothing answers on localhost:<PORT>`

**Sign:** `seiji forward` prints `seiji forward: nothing answers on localhost:<PORT> (<cause>)`. The app shows that it cannot reach the bundler.

**Cause:** The device opened a connection through the tunnel, and nothing listens on that port on your Mac. Seiji closes the device's connection. The tunnel stays open.

**Fix:** Start Metro (`npx expo start` or `npx react-native start`) or your dev server, on the port you gave `seiji forward`. Then reload the app.

### The app still loads from `localhost:8081`

**Sign:** The tunnel is open, but the app cannot load its bundle from `localhost:8081`.

**Cause:** On the Seiji host, `localhost:8081` is the host, not your Mac. The app must use the device address that `seiji forward` printed.

| App | Fix |
|---|---|
| React Native debug build | Run `seiji forward 8081 --react-native <bundle id>`. It sets `RCT_jsLocation` and relaunches the app, which reads it at launch ([apps.md](apps.md#react-native-without-expo)). |
| Expo Go or an Expo development build | Open the printed URL: `seiji forward 8081 --expo-go`, or `--dev-client <scheme>` ([apps.md](apps.md#expo-go)). |
| React Native on Android | Set the device address in the dev menu (**Settings → Debug server host & port**). `--react-native` is iOS only ([apps.md](apps.md#android)). |

The port on the host is new for each tunnel. After you start `seiji forward` again, point the app at the new address.

### The tunnel closed

**Sign:** `seiji forward` prints `the tunnel closed (the lease ended, or the server stopped)` and exits.

**Cause:** The lease ended (released, `max_lease_duration`, `budget_exhausted`, `user_disabled`), or the server stopped. An open tunnel keeps the lease from idling, but not past `maxLeaseMinutes`.

**Fix:** Acquire a device, install the app again, and start `seiji forward` again. If the tunnel closes while the lease is still active, check that the TLS proxy passes WebSocket upgrades and does not cut long-lived connections ([Long tool calls fail at the proxy](#long-tool-calls-fail-at-the-proxy)).

## Leftover devices after a crash

**Sign:** After a crash or a forced stop (`SIGKILL`, power loss), devices keep using memory, or console ports stay taken.

**What Seiji does at start:** `seiji serve` ends every lease still recorded as active with `server_restart` and deletes its device as the lease's account: `simctl shutdown` and `simctl delete` for a simulator, `adb -s emulator-<port> emu kill` for an emulator (the device id is stored right after `create`). Then it lists the simulators of each user's account (one list for all users in shared mode) and deletes every simulator whose name starts with `seiji-`. That sweep covers a crash between `simctl create` and the database write, and the simulators `seiji doctor` makes. Leases ended this way are billed up to their last recorded activity, not up to the restart, so the downtime is not billed.

Before that, it marks every run still recorded as `running` as `error` ([`server restarted during the run`](#server-restarted-during-the-run)).

**What can remain:** An emulator that did not answer `adb emu kill` at start-up, or one that `seiji doctor` started before it was killed. These keep their console ports and memory.

**Fix**

1. Restart the daemon, to run the recovery again:
   ```sh
   sudo launchctl kill SIGTERM system/dev.seiji.daemon
   ```
   launchd starts it again (`KeepAlive`).
2. Find emulators that are still running:
   ```sh
   ps -axo user,pid,pgid,command | grep -- '[-]avd '
   ```
3. Stop each one that no active lease holds, by its pid:
   ```sh
   sudo kill <pid>
   ```

In shared mode on a laptop, the sweep runs as you: it deletes your own simulators whose names start with `seiji-`.

## `cheat_sheet` fails

**Sign:** `fetching the Maestro cheat sheet; see https://docs.maestro.dev/llms.txt`

**Cause:** The daemon fetches the sheet from `https://api.copilot.mobile.dev/v2/bot/maestro-cheat-sheet` on the first call (60 s limit) and keeps it in memory until it stops. The host has no outbound HTTPS to that address, or the service failed.

**Fix:** Allow outbound HTTPS from the host. Agents can read `https://docs.maestro.dev/llms.txt` instead.
