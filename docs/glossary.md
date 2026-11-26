# Glossary

All Seiji documents use these terms with these meanings only. [concepts.md](concepts.md) explains how they work together.

| Term | Meaning |
|---|---|
| **Admin** | A user whose name is in `adminUsers` in the config. Admins can list users and change budgets and concurrency through the admin HTTP endpoints, list every user's devices, and get a viewer link to any user's device. Authority comes only from the config, never from client-supplied flags. An admin is still an unprivileged macOS tenant. |
| **adb** | The Android Debug Bridge (`<android.sdkRoot>/platform-tools/adb`); all emulators on the host share one adb server on port 5037, because Maestro hardcodes that port. |
| **Admission** | The atomic check under the admission lock (budget, concurrency, capacity, ports) that creates a `booting` lease, or fails at once because Seiji has no queue. |
| **Admission lock** | The in-process lock of `seiji serve` that makes admission atomic, so two calls cannot both take the last slot. |
| **AVD** | An Android Virtual Device in `android.avdHome`; each Android model names one, and emulators start it with `-read-only` so all leases share it unchanged. |
| **AXe** | The third-party CLI ([cameroncooke/AXe](https://github.com/cameroncooke/AXe), `axe.bin`) that taps, types, swipes and reads the accessibility tree of an iOS simulator through Apple's private accessibility and HID frameworks; `run_actions`, `inspect_screen` on iOS and the viewer's text input use it. |
| **Budget** | The monthly device-minutes of a user (`monthlyMinutes`), counted per UTC calendar month. |
| **Budget (unlimited)** | When `monthlyUnlimited` is true, admission and the reaper skip the monthly budget check. The numeric `monthlyMinutes` is preserved for switching back, and usage is still metered. |
| **Build** | An app file that a user uploaded (`POST /v1/apps`) or that Seiji downloaded (`install_app` with `url`), with the id `a_<12 hex>`, kept `appRetentionHours` or until its owner deletes it (`DELETE /v1/apps/{app_id}`), at most the user's `maxBuilds` (or `maxBuildsPerUser`) per user. |
| **Capacity** | The host-wide limit of simultaneous devices for each platform (`capacity.ios`, `capacity.android`). |
| **Capture** | A recording that runs for the whole of an iOS run: the screen video (`simctl io recordVideo`, stopped with SIGINT) or the unified-log stream into `app.log`. |
| **Console port** | The even port from `android.consolePortRange` that Seiji passes to the emulator as `-port`; the emulator also uses the next port, and its serial is `emulator-<console port>`. |
| **CoreSimulatorService** | The macOS service that owns the simulators of one account, in that account's launchd domain; each tenant account has its own, and so its own device set. |
| **Daemon account** | The unprivileged, hidden macOS account `seiji` that runs `seiji serve`. |
| **Device** | An iOS simulator (named `seiji-<lease id>`) or an Android emulator (`emulator-<console port>`), created for one lease and deleted at its release. |
| **Device address** | `host:port` at which a lease's device reaches a tunnel: `127.0.0.1:<port>` on an iOS simulator, `10.0.2.2:<port>` on an Android emulator; `seiji forward` prints it ([apps.md](apps.md)). |
| **Device-minute** | One minute that one lease exists, from admission (boot included) to its end. |
| **Driver port** | The host TCP port that Seiji gives each lease and passes to Maestro as `--driver-host-port`. |
| **End reason** | Why a lease ended: `released`, `idle`, `max_lease_duration`, `budget_exhausted`, `user_disabled`, `boot_failed` (not billed), `server_restart` or `server_shutdown`. |
| **Evidence** | The files that a run keeps in `<dataDir>/runs/<user id>/<run id>/` with its manifest, served only to the run's owner, bounded by `evidence.maxRunBytes` and `evidence.maxBytesPerUser`, and deleted `evidence.retentionHours` after the run ended. |
| **Flow** | A Maestro test in YAML: a config section (`appId: ...`), `---`, then commands; `run_flow` runs one on a lease's device. |
| **Isolation mode** | `isolation.mode`: `os_user` runs each user's commands as the user's tenant account, `shared` runs them all as the daemon account. |
| **launchd domain** | The per-user context of launchd that holds the user's services; `launchctl asuser <uid>` runs a command in it, so `simctl` finds that user's CoreSimulatorService. |
| **Lease** | One user's hold on one device, with the id `l_<12 hex>` and the states `booting`, `ready` and `ended`. |
| **Lease dir** | `<workspace>/leases/<lease id>`: the builds, the tenant run dirs of runs in progress and the screenshots of one lease, deleted at release. |
| **Maestro** | The mobile UI test tool whose CLI (`maestro test`, `maestro hierarchy`) Seiji runs for `run_flow` and for `inspect_screen` on Android; optional, only with `maestro` in the config. |
| **Manifest** | `manifest.json` of a run (schema `seiji.run.v1`): status, verdict, device, app, input, step records, and every file with its size and SHA-256. |
| **MCP** | The Model Context Protocol: the JSON-RPC protocol through which AI agents list and call Seiji's tools. |
| **Op lock** | The lock of one lease that serializes its device operations (boot, install, device tools, screen reads, screenshots, runs, viewer input), so one device has at most one driver at a time. |
| **Quota** | The limits of a user: `monthlyMinutes`, `monthlyUnlimited`, `maxConcurrent`, `maxLeaseMinutes`, `idleMinutes` and the optional `maxBuilds`. |
| **Reaper** | The 15 s loop in `seiji serve` that releases leases (disabled user, budget, lease length, idle) and deletes expired builds and runs. |
| **Root helpers** | `seiji exec-as`, `seiji exec-drop` and `seiji kill-as`, which run as root through sudo, never read the config, and act only as a tenant account; `seiji tenant provision` is run by an admin with sudo. |
| **Run** | One recorded test on a lease's device, with the id `r_<12 hex>`, the kind `actions`, `flow` or `xctest`, and the status `running`, then `passed`, `failed`, `error` or `cancelled`. |
| **Seat** | One place for a user; `seats` in the config is the maximum number of users, disabled ones included. |
| **Selector** | The fields (`id`, `label`, `value`, `contains`, `element_type`) that name an element in the accessibility tree for `tap`, `wait_for`, the assertions and `scroll` `until`. |
| **simctl** | `xcrun simctl`, the Xcode command that creates, boots, captures and deletes iOS simulators. |
| **Step** | One action of `run_actions` (for example `tap` or `wait_for`), or one Maestro command or test case of a run, recorded with its status, timing, error and files. |
| **Streamable HTTP** | The MCP transport over plain HTTP `POST`; Seiji serves it at `/mcp` without sessions, with SSE responses and 15 s keepalive comments. |
| **Tenant account** | The macOS account of a user under `os_user` isolation, named `<isolation.userPrefix><user>` (default `seiji-alice`), member of the fixed group `seiji-tenants`. |
| **Tern plugin** | `tern-plugin/`: a [Tern](https://tern.sh) panel with a user's budget, devices and runs that opens the viewer beside it; it reads the token with `seiji token print --json` ([tern.md](tern.md)). |
| **Ticket** | The single-use, 64-hex secret in a viewer link; it lives 60 s, in memory only, and `GET /view/open/<ticket>` trades it for a viewer session. |
| **Token** | A user's credential, `sj_` plus 43 URL-safe base64 characters, sent as `Authorization: Bearer <token>` and stored only as its SHA-256. |
| **Tunnel** | What `seiji forward` opens: a WebSocket to `GET /v1/leases/{lease_id}/forward` that carries every connection the device makes to its device address to one port on the developer's machine, such as Metro on `8081`; it ends with the client or the lease. |
| **User** | A Seiji account with a token and a quota: a row in the `users` table. |
| **Screen helper** | `seiji-sim` (`viewer.simBin`, source in `sim/`): one process per watched lease, run as the user's account, that encodes the simulator's screen as H.264 and sends the viewers' touches, keys and buttons over one persistent HID connection. |
| **Viewer** | The live view of a lease's iOS simulator screen in a browser or in Tern (`/view/…`), which can also send touches, keys, text and buttons; one screen helper per lease serves every viewer of it over the page's WebSocket. |
| **Viewer link** | The one-time URL `<publicUrl>/view/open/<ticket>` from `share_screen` (view-only by default), `POST /v1/leases/{lease_id}/viewer` (control by default) or, for an admin, `POST /v1/admin/leases/{lease_id}/viewer` (view-only by default), valid 60 s. |
| **Viewer session** | What an opened viewer link becomes: a session id and a secret in the `seiji_view` cookie, kept in memory as a SHA-256, view-only or with control, ending at `viewer.sessionHours`, at the lease's end or at a daemon restart. |
| **Workspace** | The root of a user's files on the host: `~<tenant account>/seiji` under `os_user`, `<dataDir>/tenants/<user>` under `shared`. |
| **XCTest driver** | The test runner app that Maestro installs in an iOS simulator to drive it, reached through the driver port. |
