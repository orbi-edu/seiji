# Configuration

Seiji reads one JSON file, `seiji.json`. This document is the reference for every key in it, the rules Seiji checks when it loads the file, the environment variables, and the files Seiji keeps in the data directory. [install.md](install.md) shows where the file goes on a real host. [concepts.md](concepts.md) explains the quotas, capacity and isolation that the keys control.

## Contents

- [The file](#the-file)
- [Validation](#validation)
- [`listen`](#listen)
- [`dataDir`](#datadir)
- [`seats`](#seats)
- [`costPerDeviceHourUsd`](#costperdevicehourusd)
- [`capacity`](#capacity)
- [`defaultQuota`](#defaultquota)
- [`adminUsers`](#adminusers)
- [`isolation`](#isolation)
- [`ios`](#ios)
- [`android`](#android)
- [`maestro`](#maestro)
- [`axe`](#axe)
- [`evidence`](#evidence)
- [`xctest`](#xctest)
- [`viewer`](#viewer)
- [`forward`](#forward)
- [`maxUploadBytes`](#maxuploadbytes)
- [`appRetentionHours`](#appretentionhours)
- [`maxBuildsPerUser`](#maxbuildsperuser)
- [Full example](#full-example)
- [Environment variables](#environment-variables)
- [Environment of host commands](#environment-of-host-commands)
- [The data directory](#the-data-directory)

## The file

- The default path is `/usr/local/etc/seiji/seiji.json`. Every command that reads the config takes `--config PATH` (`-c PATH`) or the environment variable `SEIJI_CONFIG`. See [cli.md](cli.md).
- Keys are camelCase. Unknown keys are errors in every section, so a typo fails the load instead of being ignored.
- Each command reads the file once, when it starts. `seiji serve` does not reload it: restart the daemon after a change. The other commands (`seiji user add`, `seiji usage`, …) read the current file each time, so a new `seats` value applies to the next `seiji user add` at once.
- The root helpers (`seiji exec-as`, `seiji exec-drop`, `seiji kill-as`) never read the file. `seiji tenant provision` reads it only for `isolation.userPrefix`.
- The file holds no secrets. Tokens live in the database as SHA-256 hashes. Keep the file owned by root and readable by the daemon account; Seiji never writes it.

## Validation

`Config::parse` loads the file in this order. The first failure stops the command.

| Rule | Error |
|---|---|
| The file can be read | `reading <path>` |
| The JSON parses, required keys are present, types match, no unknown keys | `parsing config`, followed by the JSON error (for example ``unknown field `seat` ``) |
| `ios.defaultModel` is a key of `ios.models` | `ios.defaultModel "<name>" is not in ios.models` |
| `android.defaultModel` is a key of `android.models` | `android.defaultModel "<name>" is not in android.models` |
| `android.consolePortRange`: the first port is even, the second is larger, and both are within 5554–5682 (the emulator's console ports) | `android.consolePortRange must be [even, larger] within 5554-5682` |
| `android.bootTimeoutSeconds` is above 0 | `android.bootTimeoutSeconds must be > 0` |
| `ios.bootTimeoutSeconds` is above 0 | `ios.bootTimeoutSeconds must be > 0` |
| `seats` is at least 1 | `seats must be >= 1` |
| `capacity.ios` and `capacity.android` are not negative | `capacity values must be >= 0` |
| `defaultQuota.monthlyMinutes` is at least 0, and `maxConcurrent`, `maxLeaseMinutes` and `idleMinutes` are at least 1 | `defaultQuota: monthlyMinutes must be >= 0 and the other limits >= 1` |
| `listen.allowedHosts` is not empty | `listen.allowedHosts must not be empty` |
| `maestro.flowTimeoutSeconds` (when `maestro` is set), `appRetentionHours` and `maxBuildsPerUser` are above 0 | `maestro.flowTimeoutSeconds, appRetentionHours and maxBuildsPerUser must be > 0` |
| `evidence.retentionHours`, `evidence.maxRunSeconds`, `evidence.maxSteps`, `evidence.maxRunBytes` and `evidence.maxBytesPerUser` are above 0 | `evidence.retentionHours, maxRunSeconds, maxSteps, maxRunBytes and maxBytesPerUser must be > 0` |
| `axe.stepTimeoutSeconds` and `xctest.timeoutSeconds` are above 0 | `axe.stepTimeoutSeconds and xctest.timeoutSeconds must be > 0` |
| `viewer.fps` is within 1–60, `viewer.scale` within 0.1–1, `viewer.bitrateKbps` within 100–20000, `viewer.paceMaxMs` within 0–1000, and `viewer.maxViewersPerLease` and `viewer.sessionHours` are above 0 | `viewer: fps 1-60, scale 0.1-1, bitrateKbps 100-20000, paceMaxMs 0-1000, maxViewersPerLease and sessionHours must be > 0` |
| With `viewer.webrtc.enabled`, `viewer.webrtc.ports` is `[first, last]` with `0 < first <= last` | `viewer.webrtc.ports must be [first, last] with 0 < first <= last` |
| With `viewer.webrtc.enabled` and an unspecified `viewer.webrtc.bind` (`0.0.0.0`, `::`), `viewer.webrtc.candidates` is not empty | `viewer.webrtc.candidates: set the addresses that pages reach this host at` |
| `forward.maxPerLease` is above 0 | `forward.maxPerLease must be > 0` |
| `listen.publicUrl`, when set, starts with `https://` or `http://` | `listen.publicUrl must start with https:// or http://` |
| Every name in `adminUsers` is a valid user name (1–32 chars of `[a-z0-9-]`) | `adminUsers: "<name>" is not a valid user name (1-32 chars of [a-z0-9-])` |

Then a relative `dataDir` becomes absolute (see [`dataDir`](#datadir)).

Seiji does **not** check these at load time:

- Whether the second port of `android.consolePortRange` is even. Admission only uses even ports, so an odd last port is never used.
- `maxUploadBytes`. A value of `0` refuses every non-empty upload.
- Whether the paths (`android.sdkRoot`, `maestro.bin`, `axe.bin`, …) exist. `seiji doctor` finds these problems ([cli.md](cli.md#seiji-doctor)).

## `listen`

The HTTP listener of `seiji serve`. The whole section is optional.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `listen.host` | string | `"127.0.0.1"` | Address to bind. Keep `127.0.0.1` when a TLS proxy on the same host is in front ([install.md](install.md#11-put-tls-in-front)). `0.0.0.0` exposes plain HTTP on every interface. |
| `listen.port` | integer | `4780` | TCP port. |
| `listen.allowedHosts` | list of strings | `["localhost", "127.0.0.1"]` | `Host` header values that `/mcp` accepts (DNS-rebinding guard). Add the public hostname that agents use. An entry without a port matches any port; an entry with a port (`"seiji.example.com:8443"`) matches only that port. |
| `listen.publicUrl` | string | unset | Base URL that agents use to reach Seiji, for example `"https://seiji.example.com"`. It must start with `https://` or `http://`. When set, the run links in tool results (`evidence_zip`, `get_run_artifact` download links, the `export_evidence` Markdown and `report.md`) are absolute (`<publicUrl>/v1/runs/<run id>/…`). When unset, they are server-relative (`/v1/runs/<run id>/…`). The same applies to the viewer link (`<publicUrl>/view/open/<ticket>`). With an `https://` value, the viewer session cookie is `Secure` ([security.md](security.md#the-viewer)). A trailing `/` is ignored. |

A request to `/mcp` with another `Host` gets `403` and the body `Forbidden: Host header is not allowed`. The list must not be empty: the MCP library would treat an empty list as "accept every host", so `Config::parse` refuses it. The check applies only to `/mcp`, not to `POST /v1/apps` or `/healthz`.

## `dataDir`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `dataDir` | path | `"./data"` | Directory for the database, the builds, the run evidence and the runtime files. See [The data directory](#the-data-directory). |

A relative `dataDir` is resolved against the directory of the config file, not the working directory. With `-c /usr/local/etc/seiji/seiji.json` the default becomes `/usr/local/etc/seiji/./data`. On a host, set an absolute path such as `/usr/local/var/seiji`.

## `seats`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `seats` | integer | required | Maximum number of users. Must be at least 1. |

`seiji user add` refuses a new user when the count reaches `seats`: ``all <seats> seats are taken; raise `seats` in the config or remove a user``. Disabled users still take a seat. Lowering `seats` below the current count removes nobody.

## `costPerDeviceHourUsd`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `costPerDeviceHourUsd` | number | unset | Price of one device-hour in US dollars. Used only for estimates. |

When set, `budget_status` returns `estimated_cost_usd` (used minutes / 60 × rate, rounded to cents) and `seiji usage` fills the `est. USD` column. When unset, `budget_status` omits the field and `seiji usage` prints `-`.

The rate does not change what you pay AWS. An EC2 Mac dedicated host has a 24-hour minimum allocation (Apple's macOS license), and you pay for the host whether devices run or not. Use the rate to split that fixed cost between users.

## `capacity`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `capacity.ios` | integer | required | Host-wide limit of iOS simulators at the same time. At least 0. |
| `capacity.android` | integer | required | Host-wide limit of Android emulators at the same time. At least 0. |

Both keys are required, also when a platform is not configured. A platform without its section (`ios`, `android`) has capacity 0, whatever this value is. `list_device_models` shows only platforms with a capacity above 0.

The limit is RAM-bound. Rule of thumb from the code: about 1.5 GiB of memory per iOS simulator and about 4 GiB per Android emulator. Keep `1.5 × capacity.ios + 4 × capacity.android` well below the host's memory, with room for Xcode services, Maestro (one JVM per running operation) and macOS. A full host fails `acquire_device` at once with `host is full: <in use>/<capacity> <platform> devices in use; try again later`. There is no queue.

## `defaultQuota`

The quota that `seiji user add` gives a new user for each flag you omit. Changing it does not change existing users; use `seiji user set` for them.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `defaultQuota.monthlyMinutes` | integer | required | Device-minutes per UTC calendar month. Boot time counts. `0` blocks every `acquire_device` unless `monthlyUnlimited` is true. |
| `defaultQuota.monthlyUnlimited` | boolean | `false` | When true, admission and the reaper skip the monthly budget check. Usage is still metered. |
| `defaultQuota.maxConcurrent` | integer | required | Devices one user may hold at the same time. At least 1. |
| `defaultQuota.maxLeaseMinutes` | integer | required | Longest wall-clock length of one lease, boot included. The reaper releases the lease with `max_lease_duration`. At least 1. |
| `defaultQuota.idleMinutes` | integer | required | A `ready` lease with no tool call for this long is released with `idle`. The clock starts when the device becomes ready, then restarts at each tool call. A running operation is never idle. At least 1. |

`monthlyMinutes`, `maxConcurrent`, `maxLeaseMinutes` and `idleMinutes` are required. `monthlyUnlimited` defaults to `false`. `Config::parse` checks the same limits, so a bad `defaultQuota` stops every command at load time. [concepts.md](concepts.md) explains how Seiji counts device-minutes and applies each limit.

## `adminUsers`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `adminUsers` | list of strings | `[]` | User names allowed to call the admin endpoints (`GET /v1/admin/users`, `PUT /v1/admin/users/{id}/limits`, `GET /v1/admin/leases`, `POST /v1/admin/leases/{lease_id}/viewer`). An admin can watch and drive any user's device ([security.md](security.md#the-viewer)). |

Authority comes only from this operator-controlled config, never from client-supplied flags. User tokens and database rows are unchanged; an admin is still an unprivileged macOS tenant. A disabled admin cannot authenticate. Names must meet the user-name rules (1–32 chars of `[a-z0-9-]`), or `Config::parse` fails with `adminUsers: "<name>" is not a valid user name`.

`GET /v1/me` includes `is_admin: true` for users in this list, so clients can show or hide the admin UI. The server enforces permission on every admin request regardless of the client.

## `isolation`

How Seiji runs host commands (`simctl`, `emulator`, `adb`, `maestro`) for a user. Required.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `isolation.mode` | `"os_user"` or `"shared"` | required | `os_user`: each user has a tenant account, and every command runs as that account through `sudo -n <helper> exec-as`. `shared`: every command runs as the daemon account; only Seiji separates users. |
| `isolation.userPrefix` | string | `"seiji-"` | Tenant account name = prefix + user name (`seiji-alice`). Used by `seiji tenant provision` and `seiji user add`. The full name may use only `[A-Za-z0-9_-]`. |
| `isolation.helper` | path | `"/usr/local/bin/seiji"` | Absolute path of the `seiji` binary that the daemon runs with `sudo -n` for `exec-as` and `kill-as`. It must match the path in `/etc/sudoers.d/seiji` ([`deploy/sudoers.seiji`](../deploy/sudoers.seiji)). |

`userPrefix` and `helper` matter only with `os_user`. The tenant group is always `seiji-tenants`; it is fixed in the code and not configurable, because the root helpers must not trust a file the daemon could change.

> **Not tested:** running `simctl` for a tenant account that never logged in to the GUI, through `launchctl asuser`, has not been tested on a real host. Run `seiji doctor <user>` before you give out tokens. If it fails at `create` or `boot`, set `isolation.mode` to `"shared"`. [security.md](security.md) explains what you give up.

Changing the mode does not change existing users. A user added under `shared` has no tenant account and fails under `os_user` with `user "<name>" has no macOS account`. Remove and add such users again after you switch.

## `ios`

Optional. Without it, iOS is not offered: `acquire_device` with `platform: "ios"` fails with `ios is not available on this host`.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `ios.models` | object | required | Map of model name → model. The names are what agents pass as `model` and see in `list_device_models`. |
| `ios.models.<name>.deviceType` | string | required | Simulator device type identifier, for example `com.apple.CoreSimulator.SimDeviceType.iPhone-16`. List them with `xcrun simctl list devicetypes`. |
| `ios.models.<name>.runtime` | string | required | Simulator runtime identifier, for example `com.apple.CoreSimulator.SimRuntime.iOS-18-5`. List them with `xcrun simctl list runtimes`. |
| `ios.defaultModel` | string | required | Model used when `acquire_device` has no `model`. Must be a key of `ios.models`. |
| `ios.bootTimeoutSeconds` | integer | `180` | Time limit of `xcrun simctl bootstatus <udid> -b`, which boots the simulator and waits until it is ready. Above 0. |

For each lease Seiji runs `xcrun simctl create seiji-<lease id> <deviceType> <runtime>` and deletes the simulator at release. Screen input and the accessibility tree on iOS go through AXe ([`axe`](#axe)); everything else is native `xcrun simctl`.

## `android`

Optional. Without it, Android is not offered.

> **Not tested:** Android emulators on an EC2 Mac (Hypervisor.framework on bare metal) have not been verified. Run `seiji doctor <user> --platform android` first.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `android.sdkRoot` | path | required | Android SDK root. Seiji runs `<sdkRoot>/emulator/emulator` and `<sdkRoot>/platform-tools/adb`. |
| `android.avdHome` | path | required | Directory of the AVDs, passed as `ANDROID_AVD_HOME`. Emulators run with `-read-only`, so Seiji never writes it. Under `os_user` it must be readable by the `seiji-tenants` group. |
| `android.models` | object | required | Map of model name → model. |
| `android.models.<name>.avd` | string | required | AVD name in `avdHome`, for example `pixel_8_api_35`. |
| `android.defaultModel` | string | required | Model used when `acquire_device` has no `model`. Must be a key of `android.models`. |
| `android.consolePortRange` | `[first, last]` | `[5600, 5640]` | Even console ports Seiji may give to emulators. The first port must be even, the last larger, and both within 5554–5682. |
| `android.bootTimeoutSeconds` | integer | `240` | How long Seiji polls `adb -s emulator-<port> shell getprop sys.boot_completed` (every 2 s) for `1`. Above 0. |
| `android.emulatorArgs` | list of strings | `["-gpu", "swiftshader_indirect", "-no-audio", "-no-boot-anim"]` | Extra emulator flags. The default renders on the CPU, which works on a headless host. A value replaces the whole list. |

For each lease Seiji starts:

```sh
<sdkRoot>/emulator/emulator -avd <avd> -port <console port> -read-only -no-window -no-snapshot-save <emulatorArgs...>
```

The device id is `emulator-<console port>`. Each emulator uses its console port and the next odd port (adb). Admission takes the first even port in the range that no active lease uses and that is free on `127.0.0.1` together with the port after it. The range gives `(last - first) / 2 + 1` slots (21 with the default). Make it at least `capacity.android`, or admission fails with `no free emulator console port in android.consolePortRange`. The range must stay inside the emulator's console ports, 5554 to 5682.

All emulators share one adb server on port 5037, because Maestro hardcodes that port. `adb devices` on the host lists every user's emulators.

## `maestro`

Optional. Only `run_flow` and `inspect_screen` on Android need Maestro. Without the section, both fail with ``Maestro is not configured on this host (`maestro` in seiji.json)``, and `seiji doctor` skips its `maestro --version` step. iOS runs (`run_actions`, `run_xctest`) and `inspect_screen` on iOS do not use Maestro.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `maestro.bin` | path | `"/opt/maestro/bin/maestro"` | The Maestro CLI. Under `os_user` every tenant account must be able to read and run it. |
| `maestro.javaHome` | path | unset | Passed as `JAVA_HOME` to Maestro. Set it: tenant accounts have no shell profile, and their `PATH` is fixed (see [Environment of host commands](#environment-of-host-commands)). Example: `/opt/homebrew/opt/openjdk@17`. |
| `maestro.flowTimeoutSeconds` | integer | `600` | Time limit of one `run_flow`. On timeout Seiji kills Maestro's process group; the run is `failed` with the verdict `the flow timed out (maestro.flowTimeoutSeconds)`. Above 0. |

Seiji does not use `maestro mcp`. It hardcodes the iOS XCTest port 22087 (`McpMaestroSessionManager.DEFAULT_XCTEST_PORT`), so two iOS simulators on one host would collide. Seiji runs `maestro --device <id> --driver-host-port <driver port> test|hierarchy` once per operation, with a driver port per lease that the OS picks. `inspect_screen` on Android (`maestro hierarchy`) has a fixed 120 s limit.

## `axe`

Optional section. [AXe](https://github.com/cameroncooke/AXe) drives iOS simulators through the accessibility tree: the taps, typing, gestures and buttons of `run_actions`, and the tree of `inspect_screen` (`axe describe-ui`). Install it with `brew install cameroncooke/axe/axe` ([install.md](install.md#axe)).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `axe.bin` | path | `"/opt/homebrew/bin/axe"` | The AXe CLI. Under `os_user` every tenant account must be able to read and run it. |
| `axe.stepTimeoutSeconds` | integer | `60` | Time limit of one AXe command: one tap, one `type`, one swipe, one `describe-ui`. On timeout the step fails with `<what> timed out` (for example `tap timed out`). Above 0. |

Seiji runs every AXe command as `<axe.bin> --output json <command> … --udid=<udid>`, with each value in the `--key=value` form, so a value that starts with `-` never becomes a flag. Typed text never appears on a command line: Seiji writes it to a daemon-owned file in `<dataDir>/run/` and passes that file as stdin to `axe type --stdin`.

## `evidence`

Optional section. Every run (`run_actions`, `run_flow`, `run_xctest`) keeps its evidence under `<dataDir>/runs/<user id>/<run id>/` ([The data directory](#the-data-directory)). These keys bound how much and for how long.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `evidence.retentionHours` | integer | `168` (7 days) | The reaper deletes a run (its row and its directory) this many hours after it ended. After that, the run is gone from `list_runs` and `get_run` fails with `no run "<run id>" for you`. Above 0. |
| `evidence.maxBytesPerUser` | integer | `5368709120` (5 GiB) | Evidence one user may keep, all runs together, counting the budget that running runs reserve. When it is reached, a new run fails before it starts with `evidence quota full: <held> of <maxBytesPerUser> bytes kept or reserved by running runs; runs expire <retentionHours>h after they end`. Above 0. |
| `evidence.maxRunBytes` | integer | `2147483648` (2 GiB) | Files one run may keep. Above 0. |
| `evidence.maxRunSeconds` | integer | `1800` | Time limit of one `run_actions` call, all steps included. A step that runs past it fails with `the run reached evidence.maxRunSeconds`. When the limit falls between steps, the rest are skipped and the run is `failed` with `<P> of <N> steps ran: the run reached evidence.maxRunSeconds`. Above 0. |
| `evidence.maxSteps` | integer | `200` | Most actions in one `run_actions` call. More fail with `run_actions takes 1-<maxSteps> actions`. Above 0. |

The byte budget of one run is the smaller of `evidence.maxRunBytes` and what is left of `evidence.maxBytesPerUser`, after the user's kept runs and the budgets reserved by the user's running runs. A run reserves its budget when it starts and releases the reservation when it ends; then its real size counts. `app.log` may use at most half of the budget; past that Seiji cuts the file and adds the warning `app.log was cut to <N> bytes`. The files of the run are then copied until the budget is spent. A file that does not fit is not kept: the manifest lists it under `dropped`.

The budget applies only at the end of the run. While the run executes, the screen recording and the log stream grow on disk without a limit, in the lease dir and in `<dataDir>/runs/…/app.log`.

`evidence.maxRunSeconds` does not apply to `run_flow` (`maestro.flowTimeoutSeconds`) or `run_xctest` (`xctest.timeoutSeconds`).

On a host, plan the disk for `seats × evidence.maxBytesPerUser` (50 GiB with ten seats and the default).

## `xctest`

Optional section.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `xctest.enabled` | boolean | `true` | Offers `run_xctest`. When `false`, `run_xctest` fails with `run_xctest is turned off on this host (xctest.enabled)`. Turn it off when you do not want users to run their own test code on the host ([security.md](security.md)). |
| `xctest.timeoutSeconds` | integer | `3600` | Time limit of one `run_xctest`: unpacking the bundle, `xcodebuild test-without-building` and the `xcresulttool` exports. On timeout the run is `failed` with the verdict `xcodebuild timed out (xctest.timeoutSeconds)`. Above 0. |

`run_xctest` runs the bundle with `/usr/bin/xcrun xcodebuild test-without-building` and reads the result bundle with `/usr/bin/xcrun xcresulttool`. Both come with Xcode ([install.md](install.md#xcode-and-the-ios-runtime)).

## `viewer`

Optional section. The viewer shows a lease's screen live in a browser or in Tern, and can send taps, swipes, typing and buttons to it ([tern.md](tern.md), [security.md](security.md#the-viewer)). It works on iOS simulators only. A viewer link comes from `share_screen`, `POST /v1/leases/{lease_id}/viewer`, or for an admin `POST /v1/admin/leases/{lease_id}/viewer` ([mcp.md](mcp.md)).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `viewer.simBin` | path | `"/usr/local/bin/seiji-sim"` | The screen helper, built from `sim/` ([install.md](install.md#3-build-and-install-the-binary)). Under `os_user` every tenant account must be able to read and run it. |
| `viewer.fps` | integer | `30` | Highest frames per second of the stream. A still screen sends none. 1–60. An admin can change it for every stream while Seiji runs ([mcp.md](mcp.md#get-v1adminviewer-put-v1adminviewer)): 60 is smoother and lowers the frame age by about 8 ms on average, at more bandwidth; a viewer that falls behind gets every other frame. |
| `viewer.scale` | number | `0.5` | Frame scale. `0.5` halves each side of the simulator's pixels. 0.1–1. |
| `viewer.bitrateKbps` | integer | `2500` | Target H.264 bitrate in kbit/s, for the `balanced` preset. A congested viewer lowers it for a while ([mcp.md](mcp.md#the-stream)). 100–20000. |
| `viewer.maxViewersPerLease` | integer | `4` | Viewer pages that may stream one lease at the same time. One more gets `429` `full`. Above 0. |
| `viewer.sessionHours` | integer | `12` | Longest life of a viewer session (and of its cookie). A session also ends when its lease ends. Above 0. |
| `viewer.paceMaxMs` | integer | `80` | The longest a touch move or lift waits to keep a gesture's own spacing when the link delivered its points late or bunched ([mcp.md](mcp.md#gesture-timing)). Higher keeps more of a jittery swipe's speed and adds more input delay; `0` sends every point at once. 0–1000. An admin can change it while Seiji runs. |
| `viewer.webrtc.enabled` | boolean | `false` | Pages may take the stream and send their input over WebRTC (UDP) instead of the WebSocket. See below. |
| `viewer.webrtc.bind` | IP address | `"0.0.0.0"` | The address the WebRTC UDP sockets bind to. |
| `viewer.webrtc.ports` | `[first, last]` | `[50000, 50099]` | The UDP ports, one for each open WebRTC session. When all are in use, a page stays on the WebSocket. |
| `viewer.webrtc.candidates` | list of IP addresses | `[]` | The addresses pages reach this host at, sent to the page as ICE host candidates. Empty: `bind` (which must then be a specific address). |

Seiji runs one helper process for each watched lease, however many viewers watch it: `<viewer.simBin> session --udid <udid> --fps <fps> --scale <scale> --bitrate <bits per second>`, through `Runner` as the user's account. It starts with the first viewer and stops when the last one leaves or the lease ends. It also carries the viewers' touches, keys and buttons. Higher `fps`, `scale` and `bitrateKbps` cost more bandwidth for each viewer; encoding runs on the Mac's media engine.

Before this version the viewer streamed JPEG and took `viewer.quality`. That key no longer exists: remove it from `seiji.json`, or the config does not load.

### WebRTC

With `viewer.webrtc.enabled`, the page offers a WebRTC session over its WebSocket. Seiji answers it on one UDP port from `ports`, with a candidate for each address in `candidates`. The page then gets the stream as H.264 over SRTP and sends its input on a data channel, while the WebSocket keeps the replies and the presets. The page asks the browser to play each frame at once, without a jitter buffer.

Turn it on only after you measure your users' links. On an emulated link of 150 ms round trip and 4 Mbit/s (a real simulator, Chrome in Tern), the frame age at p50 and p90 and the frames per second were:

| Link | WebSocket | WebRTC |
|---|---|---|
| Clean | 131 / 225 ms, 22 fps | 112 / 132 ms, 26 fps |
| ±60 ms jitter, 0.3 % loss | 132 / 245 ms, 18 fps | 154 / 842 ms, 13 fps |
| ±20 ms jitter, 2 % loss | 150 / 401 ms, 19 fps | 128 / 700 ms, 11 fps |

WebRTC wins on a clean link and loses on a bad one. Seiji sends no FEC (`str0m` has none for video), so a lost packet costs a retransmission and holds back the frames after it for a round trip. Jitter also makes the bandwidth estimate fall, and with it the bitrate. The WebSocket's own flow control copes better with both.

Seiji is ICE-lite and uses no STUN or TURN server: the page must reach one of `candidates` over UDP, on the ports in `ports`. When it cannot (the ports are closed, the page is behind a proxy that carries only HTTPS), the page stays on the WebSocket after 10 s and says nothing. On EC2, `candidates` is the instance's public address (or its private one, for pages that reach it over a VPN), and the security group must allow UDP on `ports` from the pages' networks ([install.md](install.md#webrtc)).

```json
"viewer": { "webrtc": { "enabled": true, "ports": [50000, 50099], "candidates": ["203.0.113.10"] } }
```

## `forward`

Optional section. `seiji forward` opens a tunnel from a lease's host to a port on the developer's machine, so the device reaches Metro or another dev server there ([apps.md](apps.md#run-your-dev-server-with-seiji-forward)). The route is `GET /v1/leases/{lease_id}/forward` ([mcp.md](mcp.md#get-v1leaseslease_idforward)).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `forward.enabled` | boolean | `true` | Offers tunnels. When `false`, the route answers `403` with `forwarding is off on this server`. Turn it off when tenants must not share the host's loopback ([security.md](security.md#tunnels-seiji-forward)). |
| `forward.maxPerLease` | integer | `4` | Tunnels open on one lease at the same time, for example one for each app's Metro. One more gets `429` with `this lease already has <maxPerLease> tunnels open`. Above 0. |

Each tunnel listens on a new port on `127.0.0.1`, which the OS picks. It ends when the client closes it, the lease ends or the daemon stops. While it is open, it counts as activity on the lease every 60 s.

## `maxUploadBytes`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `maxUploadBytes` | integer | `1073741824` (1 GiB) | Largest build. |

`POST /v1/apps` checks the size while it streams the body. Past the limit it stops, deletes the partial file and answers `413` with `upload exceeds maxUploadBytes (<maxUploadBytes> bytes)`.

`install_app` with a `url` applies the same limit to the download: `download exceeds <maxUploadBytes> bytes`. The download also has a 600 s time limit and follows at most 5 redirects, all of them `https`. A failed download leaves no file.

## `appRetentionHours`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `appRetentionHours` | integer | `72` | The reaper deletes a build (uploaded or downloaded) this many hours after it arrived. After that, its `app_id` fails with `no uploaded app "<app_id>"`. At least 1. |

The reaper checks every 15 s. A build already installed on a device stays on that device until the lease ends.

## `maxBuildsPerUser`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `maxBuildsPerUser` | integer | `10` | Builds one user may keep at the same time, uploaded and downloaded together. At least 1. |

When the user already keeps this many builds, `POST /v1/apps` answers `429` and `install_app` with a `url` fails, both with `you already keep <N> builds, the limit is <maxBuildsPerUser>; they expire <appRetentionHours>h after upload`. Builds leave the count when the reaper deletes them after `appRetentionHours`, or earlier when the user deletes one with `DELETE /v1/apps/{app_id}` ([mcp.md](mcp.md#delete-v1appsapp_id)).

## Full example

This is [`deploy/seiji.example.json`](../deploy/seiji.example.json): ten seats, `os_user` isolation, two iOS models and one Android model.

```json
{
  "listen": {
    "host": "0.0.0.0",
    "port": 4780,
    "allowedHosts": ["localhost", "127.0.0.1", "seiji.example.com"],
    "publicUrl": "https://seiji.example.com"
  },
  "dataDir": "/usr/local/var/seiji",
  "seats": 10,
  "costPerDeviceHourUsd": 0.75,
  "capacity": { "ios": 12, "android": 6 },
  "defaultQuota": {
    "monthlyMinutes": 1200,
    "maxConcurrent": 2,
    "maxLeaseMinutes": 60,
    "idleMinutes": 15
  },
  "adminUsers": [],
  "isolation": {
    "mode": "os_user",
    "userPrefix": "seiji-",
    "helper": "/usr/local/bin/seiji"
  },
  "ios": {
    "models": {
      "iphone-16": {
        "deviceType": "com.apple.CoreSimulator.SimDeviceType.iPhone-16",
        "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-18-5"
      },
      "ipad-air": {
        "deviceType": "com.apple.CoreSimulator.SimDeviceType.iPad-Air-11-inch-M3",
        "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-18-5"
      }
    },
    "defaultModel": "iphone-16"
  },
  "android": {
    "sdkRoot": "/opt/android-sdk",
    "avdHome": "/opt/android-avd",
    "models": {
      "pixel-8": { "avd": "pixel_8_api_35" }
    },
    "defaultModel": "pixel-8"
  },
  "axe": {
    "bin": "/opt/homebrew/bin/axe",
    "stepTimeoutSeconds": 60
  },
  "evidence": {
    "retentionHours": 168,
    "maxBytesPerUser": 5368709120,
    "maxRunBytes": 2147483648,
    "maxRunSeconds": 1800,
    "maxSteps": 200
  },
  "xctest": {
    "timeoutSeconds": 3600
  },
  "viewer": {
    "simBin": "/usr/local/bin/seiji-sim",
    "fps": 30,
    "scale": 0.5,
    "bitrateKbps": 2500,
    "maxViewersPerLease": 4,
    "sessionHours": 12,
    "paceMaxMs": 80,
    "webrtc": {
      "enabled": false,
      "bind": "0.0.0.0",
      "ports": [50000, 50099],
      "candidates": ["203.0.113.10"]
    }
  },
  "forward": {
    "enabled": true,
    "maxPerLease": 4
  },
  "maestro": {
    "bin": "/opt/maestro/bin/maestro",
    "javaHome": "/opt/homebrew/opt/openjdk@17",
    "flowTimeoutSeconds": 600
  }
}
```

With a TLS proxy on the same host, change `listen.host` to `"127.0.0.1"`.

The smallest valid file, for a laptop in shared mode with iOS only:

```json
{
  "seats": 1,
  "capacity": { "ios": 2, "android": 0 },
  "defaultQuota": { "monthlyMinutes": 6000, "maxConcurrent": 2, "maxLeaseMinutes": 120, "idleMinutes": 15 },
  "isolation": { "mode": "shared" },
  "ios": {
    "models": {
      "iphone-16": {
        "deviceType": "com.apple.CoreSimulator.SimDeviceType.iPhone-16",
        "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-18-5"
      }
    },
    "defaultModel": "iphone-16"
  }
}
```

## Environment variables

| Variable | Read by | Effect |
|---|---|---|
| `SEIJI_CONFIG` | every command that takes `--config` | Path of the config file. `--config` wins over it. `sudo` drops it (`env_reset`), so pass `-c` to commands you run with `sudo`. |
| `RUST_LOG` | `seiji serve`, `seiji doctor` | Log filter ([`tracing` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax). Default `seiji=info`. Use `seiji=debug` for more detail, or `seiji=info,rmcp=warn` to also see the MCP library's warnings, such as rejected `Host` headers. The launchd plist sets `seiji=info`. |

## Environment of host commands

Seiji controls the environment of every command it runs for a user:

| Mode | Environment |
|---|---|
| `os_user` | `exec-drop` clears the environment. The command gets `HOME` (the tenant account's home), `USER` and `LOGNAME` (the account), `SHELL=/bin/zsh`, `PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin`, and the variables below. It starts in its lease dir or the account's home. |
| `shared` | The daemon's own environment, with `PATH` set to the same fixed value, plus the variables below. |

| Command | Extra variables |
|---|---|
| `maestro` | `MAESTRO_CLI_NO_ANALYTICS=1`, `MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED=true`, `MAESTRO_DISABLE_UPDATE_CHECK=true`, `JAVA_HOME` (when `maestro.javaHome` is set), `ANDROID_HOME` (when `android` is set) |
| `emulator`, `adb` | `ANDROID_SDK_ROOT` and `ANDROID_HOME` (`android.sdkRoot`), `ANDROID_AVD_HOME` (`android.avdHome`) |
| `axe`, `xcrun simctl`, `xcodebuild`, `xcresulttool` | None |
| `run_flow` | Each `env` entry of the tool call becomes `-e KEY=VALUE` on the Maestro command line. Keys must match `[A-Za-z0-9_]+` and have at most 128 characters; another key fails the call before any run is recorded with `env key "<key>" must match [A-Za-z0-9_]+`. The manifest keeps only the keys (`env_keys`), not the values. Maestro's own output (`maestro/output.txt`, `maestro/debug/`) may still contain the resolved values. |

## The data directory

`seiji serve`, `seiji doctor` and the `user` and `usage` commands create what they need under `dataDir`. Run them as the daemon account, so that it owns every file. These commands (and `seiji tenant provision`) set the umask to `077`, so every file and directory they create is private to the daemon account unless the table says otherwise.

| Path | Mode | Contents |
|---|---|---|
| `dataDir` itself | `0711`, set at each start of `serve` and `doctor` | Tenant accounts can pass through it to reach `run/pids`, but cannot list it. Its parent directories must also let them pass. |
| `seiji.db` (and `seiji.db-wal`, `seiji.db-shm`) | `0600` | SQLite database in WAL mode. Tables `users` (name, token hash, tenant account, quota, disabled), `leases` (the usage ledger: every lease with its times, ports, end reason and whether it is billed), `apps` (builds) and `runs` (every run with its kind, name, tags, status, times, bytes and verdict). |
| `apps/<user id>/<app id>/<file name>` | `0700` directories, `0600` files | Builds from `POST /v1/apps` and from `install_app` URLs. `<user id>` is the numeric id of the row in `users`. The reaper deletes each one after `appRetentionHours`; `DELETE /v1/apps/{app_id}` deletes one earlier; `seiji user remove` deletes `apps/<user id>`. |
| `runs/<user id>/<run id>/` | `0700` directories, `0600` files | The evidence of one run (see below). The reaper deletes it `evidence.retentionHours` after the run ended. |
| `run/` | `0711`, set at each start | Scratch files, each deleted when its command ends: flow YAML (`<uuid>.yaml`), typed text (`<uuid>.txt`) and APNs payloads (`<uuid>.apns`) that commands read as stdin, image previews (`<uuid>.jpg`, made by `sips` running as the daemon account on the run's copy of a tenant image), `evidence.zip` while it is sent (`<uuid>.zip`, built again for every request), the output of a screen recording (`cap-<uuid>.log`), and the log of `seiji doctor`'s capture (`doctor-<8 hex>-doctor.log`). Also emulator logs (`emulator-<lease id>.log`, deleted at release). |
| `run/pids/` | `1733`, set at each start | Under `os_user`, `exec-drop` writes the process-group id of each command here as the tenant account (`<uuid>.pid`, mode `0644`), so the daemon can signal the group through `kill-as`. The daemon deletes the file when the command ends. The mode lets tenant accounts create files but not list or remove other files. |
| `tenants/<user>/` | `0700` | Only under `shared`: the user's workspace. Lease dirs are `tenants/<user>/leases/<lease id>/`. |

A run directory `runs/<user id>/<run id>/` holds `manifest.json` (schema `seiji.run.v1`) and the run's files under fixed relative paths. Only these files, and `manifest.json`, are served over HTTP ([mcp.md](mcp.md)).

| Path in the run directory | Written by | Contents |
|---|---|---|
| `manifest.json` | Seiji | Status, verdict, device, app, input, steps, and every other file with its size and SHA-256. It does not list itself. |
| `steps/<NNN>-<kind>.png`, `steps/<NNN>-<kind>-failure.png`, `steps/<NNN>-screenshot-<name>.png` | the tenant | `run_actions` screenshots: after each step (`screenshot_each_step`), at a failed step, and from a `screenshot` step (which saves only its named file). `<NNN>` is the step index from `000`, `<kind>` the action (`tap`, `type`, `wait_for`, …). |
| `steps/<NNN>-<kind>-tree.txt`, `steps/<NNN>-capture_tree-<name>.json` | Seiji | The accessibility tree at a failed step, and the tree of a `capture_tree` step. |
| `video.mp4` | the tenant | The screen recording (`xcrun simctl io <udid> recordVideo --codec=h264`), iOS only. |
| `app.log` | Seiji | The unified log stream (`xcrun simctl spawn <udid> log stream`), iOS only. |
| `crashes/<executable>-*.ips` | the tenant | Crash reports of the installed app written during the run, from the account's `~/Library/Logs/DiagnosticReports`. Under `shared` that folder belongs to the daemon account and is shared by every user. |
| `flow.yaml`, `maestro/` | the tenant and Seiji | `run_flow`: the flow, `maestro/report.xml` (JUnit), `maestro/debug/`, `maestro/output/`, and `maestro/output.txt` (Maestro's output). |
| `xctest/` | the tenant and Seiji | `run_xctest`: `xcodebuild.log`, `result.xcresult.zip`, `xcresult-summary.json`, `xcresult-tests.json`, `attachments/`, and `output-tail.txt`. |

Files the tenant writes go first to `<lease dir>/runs/<run id>/`. At the end of the run Seiji reads each one as the tenant account with `/bin/cat`, so a symlink reaches only what the tenant can read. It cleans each path (characters outside `[A-Za-z0-9._-]` become `_`, leading dots go, at most 8 levels), copies the files within the run's byte budget ([`evidence`](#evidence)), and deletes the tenant's copy. It never replaces a file already in the evidence, neither Seiji's own files nor an earlier tenant file whose name cleans to the same path; it warns `skipped "<rel>": <clean> already exists in the evidence` instead.

Every run ends with a manifest. If recording the evidence fails after the run started (copy, hashes, manifest), or the daemon restarts during the run, Seiji writes an `error` manifest with the files that exist, and counts their bytes against the quota.

Commands that run as a tenant account also run with umask `077`, so the files they create in the workspace are private to that account.

Under `os_user` the workspace is `~<tenant account>/seiji` (for example `/Users/seiji-alice/seiji`), and lease dirs are `~<tenant account>/seiji/leases/<lease id>/`. A lease dir holds the copied build (`app/`), the tenant's side of each run while it runs (`runs/<run id>/`) and screenshots while they are encoded. Seiji deletes the lease dir at release.

`seiji user remove` deletes `apps/<user id>/` and `runs/<user id>/` with the user's rows.

Seiji creates the database tables with `CREATE TABLE IF NOT EXISTS` when it opens the store. There are no schema migrations. To back up the usage ledger and the users, copy `seiji.db` with SQLite's online backup:

```sh
sudo -u seiji sqlite3 /usr/local/var/seiji/seiji.db ".backup /usr/local/var/seiji/backup.db"
```

[security.md](security.md) and [install.md](install.md#5-create-the-data-and-config-directories) cover the ownership and modes of these paths.
