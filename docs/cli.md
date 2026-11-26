# `seiji` (command line)

`seiji` is one binary. It runs the MCP server, manages users, prints usage, creates tenant accounts, checks the host, and holds the root helpers that the daemon reaches through sudo. On a developer's Mac, the same binary is the client that signs in to a server for the Tern plugin (`seiji login`, `seiji logout`, `seiji token print`), keeps itself and the plugin at the server's commit (`seiji update`) and opens a tunnel to your dev server (`seiji forward`). This document lists every command and flag, the account that must run each one, and what each prints. [install.md](install.md) shows the commands in order on a new host. [configuration.md](configuration.md) is the reference of the config file they read.

## Contents

- [Conventions](#conventions)
- [Which account runs what](#which-account-runs-what)
- [`seiji serve`](#seiji-serve)
- [`seiji user`](#seiji-user)
- [`seiji usage`](#seiji-usage)
- [`seiji tenant provision`](#seiji-tenant-provision)
- [`seiji doctor`](#seiji-doctor)
- [`seiji login`](#seiji-login)
- [`seiji logout`](#seiji-logout)
- [`seiji token print`](#seiji-token-print)
- [`seiji update`](#seiji-update)
- [`seiji forward`](#seiji-forward)
- [Exit codes and errors](#exit-codes-and-errors)
- [Root helpers (internal)](#root-helpers-internal)

## Conventions

- `--config PATH` (`-c PATH`) selects the config file. It is a global flag, so it may come before or after the subcommand: `seiji user add alice -c /path/seiji.json` and `seiji user -c /path/seiji.json add alice` are the same. The default is `/usr/local/etc/seiji/seiji.json`, or `$SEIJI_CONFIG` when it is set. `sudo` drops `SEIJI_CONFIG`, so pass `-c` with `sudo` when the file is somewhere else.
- The root helpers (`exec-as`, `exec-drop`, `kill-as`) take no `--config` and never read the config. The client commands (`login`, `logout`, `token print`, `update`, `forward`) do not read it either.
- User names have 1 to 32 characters of `[a-z0-9-]`. Other names fail with `user names must be 1-32 chars of [a-z0-9-]`.
- `seiji --help` and `seiji <command> --help` print the usage. `seiji --version` prints `seiji <version> (<commit>)`: the first 10 characters of the git commit the binary is built from. A build outside a git checkout prints `seiji <version>`.

## Which account runs what

| Command | Run it as | Why |
|---|---|---|
| `seiji serve` | daemon account `seiji` (launchd `UserName`) | It owns the data directory and is the only account sudoers lets run the root helpers. |
| `seiji user …`, `seiji usage` | daemon account: `sudo -u seiji seiji …` | They open `seiji.db` for writing (WAL mode, table creation), with umask `077`. Run as another account, they fail on file permissions, or create files the daemon cannot read or write. |
| `seiji doctor` | daemon account: `sudo -u seiji seiji doctor …` | It uses the same data directory, runner and sudo chain as the daemon. |
| `seiji tenant provision` | root: `sudo seiji tenant provision …` | It creates macOS accounts and groups. |
| `seiji exec-as`, `seiji exec-drop`, `seiji kill-as` | root, reached only by the daemon through sudo | Internal. See [Root helpers](#root-helpers-internal). |
| `seiji login`, `seiji logout`, `seiji token print`, `seiji update`, `seiji forward` | yourself, on a developer's Mac, not on the server | Client commands. They use your login keychain and your `~/Library/Application Support/seiji/`. |
| `seiji --help`, `seiji --version` | any account | They read nothing. |

The examples below use `sudo -u seiji` as on a host. In local development (shared mode) you run everything as yourself ([install.md](install.md#15-local-development-shared-mode)).

## `seiji serve`

```sh
seiji serve [-c PATH]
```

Runs the MCP server. In order, it:

1. Loads the config and opens `<dataDir>/seiji.db`.
2. Sets `<dataDir>` and `<dataDir>/run` to mode `0711` and `<dataDir>/run/pids` to mode `1733`.
3. Recovers: closes every run still recorded as `running` with the status `error`, the verdict `server restarted during the run` and a manifest that lists the files written before the restart, whose bytes count against the user's evidence quota. Then it ends every lease still recorded as active with the end reason `server_restart`, billed up to its last recorded activity (not the downtime), and deletes its device: `simctl shutdown` and `simctl delete` for a simulator, `adb -s emulator-<port> emu kill` for an emulator, run as the lease's account. Then it deletes every simulator named `seiji-*` in each user's device set (one pass for all users in shared mode), which covers a crash between `simctl create` and the database write. See [troubleshooting.md](troubleshooting.md#leftover-devices-after-a-crash).
4. Starts the reaper (every 15 s). It releases leases, deletes builds older than `appRetentionHours`, and deletes runs that ended more than `evidence.retentionHours` ago.
5. Binds `listen.host:listen.port` and logs `seiji listening on http://<host>:<port>/mcp`.

Routes: `/mcp` (Streamable HTTP MCP, bearer token), `POST /v1/apps` (build upload, bearer token), `DELETE /v1/apps/{app_id}` (deletes one of the token's own builds, bearer token), `GET /v1/runs/{run_id}/files/{*path}`, `GET /v1/runs/{run_id}/evidence.zip` and `GET /v1/runs/{run_id}/report.md` (run evidence, bearer token, only the token's own runs), `GET /v1/me`, `GET /v1/admin/users` and `PUT /v1/admin/users/{id}/limits` (admin endpoints, bearer token, admin users only), `POST /v1/leases/{lease_id}/viewer`, `GET /v1/leases/{lease_id}/screenshot.jpg` and the `seiji forward` WebSocket `GET /v1/leases/{lease_id}/forward` (bearer token, only the token's own leases), the viewer routes under `/view/` (viewer session cookie), and `GET /healthz` (no token, answers `ok`). [mcp.md](mcp.md) documents them.

On `SIGTERM` or `SIGINT` it stops the reaper and new MCP requests, logs `shutting down: releasing devices`, and releases every lease at once with `server_shutdown`. Releasing cancels running runs, which end with the status `cancelled` and keep their evidence, so open requests end quickly; then it waits for them and exits 0. The launchd plist gives it 120 s (`ExitTimeOut`) for this. If launchd kills the process first, the next start ends the leases with `server_restart` and marks the runs `error`.

## `seiji user`

```sh
seiji user [-c PATH] <add|list|set|disable|enable|rotate-token|remove> ...
```

| Command | What it does |
|---|---|
| `seiji user add <name> [quota flags]` | Adds a user and prints its token once. Each omitted quota flag takes its value from `defaultQuota`. Fails when all seats are taken. Under `os_user` the tenant account `<userPrefix><name>` must exist, be in `seiji-tenants` and not be an admin; otherwise it fails with ``run `sudo seiji tenant provision <name>` first`` and the cause. |
| `seiji user list` | Prints every user with its tenant account, quota and status, and the seat count. |
| `seiji user set <name> [quota flags]` | Changes the given quota fields and keeps the others. Applies at once: admission and the reaper read the database each time. |
| `seiji user disable <name>` | Blocks the user. Its token gets `401` at once, and the reaper releases its devices with `user_disabled` within 15 s. |
| `seiji user enable <name>` | Unblocks the user. |
| `seiji user rotate-token <name>` | Prints a new token once. The old token stops working at once. |
| `seiji user remove <name>` | Deletes the user, its lease history (its usage), its builds (rows and `<dataDir>/apps/<user id>`) and its runs (rows and `<dataDir>/runs/<user id>`). Refuses while the user holds devices: disable the user first and wait. The tenant account and its home stay. |

Quota flags, for `add` and `set`:

| Flag | Config key | Rule |
|---|---|---|
| `--monthly-minutes N` | `monthlyMinutes` | At least 0. `0` blocks every acquire unless `--monthly-unlimited true`. |
| `--monthly-unlimited BOOL` | `monthlyUnlimited` | `true` or `false`. When omitted, preserves the current value. |
| `--max-concurrent N` | `maxConcurrent` | At least 1. |
| `--max-lease-minutes N` | `maxLeaseMinutes` | At least 1. |
| `--idle-minutes N` | `idleMinutes` | At least 1. |
| `--max-builds N\|default` | `maxBuilds` | At least 1. `default` removes the user's own limit, so the user follows `maxBuildsPerUser`. A new user without the flag gets `defaultQuota.maxBuilds`, or follows `maxBuildsPerUser` when that is not set. |

A value that breaks a rule fails with `quotas must be positive (monthly-minutes may be 0)`. A name that does not exist fails with `no user "<name>"`. Omitting a flag preserves the user's current value, so `seiji user set alice --max-lease-minutes 60` changes only the lease limit and keeps the budget, concurrency, build limit and unlimited state.

Give a service user, one account that many agents share, a higher build limit than a person, so that the builds of all its agents fit: `seiji user set ci --max-builds 50`.

Output of `add`:

```text
user alice added
token (shown once): sj_rOp-22QxFnp6KMfO59XAHMnR2cLp01NIlueqyY83nac
```

Tokens start with `sj_`. Seiji stores only their SHA-256 hash, so a lost token cannot be shown again: rotate it.

Output of `list` (`os account` is `-` in shared mode; `builds` is the build limit in effect, the user's own or `maxBuildsPerUser`):

```text
name             os account               min/mo  conc lease-min   idle builds  status
alice            seiji-alice                1200     2        60     15     10  active
bob              seiji-bob             unlimited     1        60     15     50  disabled
seats: 2/10
```

Output of the other commands:

| Command | Output |
|---|---|
| `set` | `updated <name>` |
| `disable` | `disabled <name>; held devices are released within 15s` |
| `enable` | `enabled <name>` |
| `rotate-token` | `new token for <name> (shown once): sj_...` |
| `remove` | `removed <name> (its macOS account, if any, is left in place)` |

## `seiji usage`

```sh
seiji usage [-c PATH] [--month YYYY-MM]
```

Prints the device-minutes of every user for one UTC calendar month, the current month by default. Usage counts every billed lease that overlaps the month, clipped to the month; an active lease counts up to now. Leases that ended with `boot_failed` are not billed. `est. USD` needs `costPerDeviceHourUsd`; without it the column shows `-`. A bad month fails with `month must be YYYY-MM`.

```text
month 2026-11
user               used min    quota  used%  leases  est. USD
alice                 312.0     1200    26%      14  3.90
bob                  2952.0     3000    98%      61  36.90
total                3264.0                          40.80
```

`leases` counts the billed leases that overlap the month. A removed user disappears from every month, because `seiji user remove` deletes the history.

## `seiji tenant provision`

```sh
sudo seiji tenant provision [-c PATH] <name>
```

Only for `os_user` isolation. Creates or repairs the tenant account `<isolation.userPrefix><name>`:

1. Creates the group `seiji-tenants` if it does not exist (`dseditgroup`).
2. Creates the account with `dscl` if it does not exist: uid one above the highest existing uid (at least 600), primary group 20 (`staff`), home `/Users/<account>` and shell `/usr/bin/false`. Then adds it to `seiji-tenants`.
3. Disables authentication (`AuthenticationAuthority ;DisabledUser;`), hides it from the login window (`IsHidden 1`), creates its home and sets the home to mode `700`.
4. Checks the result with the same rules as the root helpers: the account exists, has uid 501 or more, is in `seiji-tenants` and is not in `admin`.

Running it again on a tenant account is safe; it repairs the disabled authentication authority, the hidden flag, the home and its mode. It refuses an existing account that is not already a tenant account (not in `seiji-tenants`, an admin, or a uid below 501): `"<account>" already exists and is not a Seiji tenant account; refusing to change it`. Without root it fails with `must run as root (through sudo)`.

```text
macOS account seiji-alice ready (uid 601, home /Users/seiji-alice)
next: seiji user add alice
```

## `seiji doctor`

```sh
seiji doctor [-c PATH] <user> [--platform ios|android]
```

Exercises the real host path for one user, as the daemon would, without a lease and without touching the user's budget. `--platform` defaults to `ios`. Steps, in order:

| Step | When | What it runs |
|---|---|---|
| `identity` | always | `/usr/bin/id` as the user's account (through `sudo -n … exec-as` under `os_user`). |
| `maestro --version` | only when `maestro` is configured | `maestro.bin --version` with `maestro.javaHome`. |
| `axe --version` | `--platform ios` | `axe.bin --version` as the user's account (30 s limit). |
| `seiji-sim --version` | `--platform ios` | `viewer.simBin --version` as the user's account (30 s limit). |
| `create <platform> <model>` | always | Creates a device of the platform's `defaultModel`: simulator `seiji-doctor-<8 hex>`, or an emulator on the first free even port of `android.consolePortRange` that no active lease holds. |
| `boot` | always | Waits for boot with the configured boot timeout. |
| `screenshot` | always | Captures a JPEG through `sips`. |
| `accessibility tree (axe describe-ui)` | `--platform ios` | Reads the accessibility tree with `axe describe-ui` and prints the number of elements. |
| `screen session (seiji-sim: capture, H.264, HID)` | `--platform ios` | Starts the viewer's screen helper as the user's account, waits up to 20 s for its decoder setup and first keyframe, then stops it. Prints the codec, the size, the keyframe's bytes and the time it took. Its HID connection opens before the first frame, so a pass shows that input can connect too; it does not prove that a touch reaches the app. |
| `video and log capture (3 s)` | `--platform ios` | Starts `xcrun simctl io <udid> recordVideo` and `xcrun simctl spawn <udid> log stream` (predicate `process == "SpringBoard"`) as the user's account, waits 3 s, stops the video with `SIGINT` and the log stream, then prints both sizes. It fails with `video file exited <code>: <output>` when no video file was written. A log of 0 bytes does not fail the step. |
| `destroy` | always, also when an earlier step failed | Deletes the device and its directory. |

```text
== identity
uid=601(seiji-alice) gid=20(staff) groups=20(staff),701(seiji-tenants)
== maestro --version
<maestro version>
== axe --version
<axe version>
== seiji-sim --version
seiji-sim 0.1.0
== create ios iphone-16
device <UDID>
== boot
booted in 38s
== screenshot
412233 bytes of JPEG
== accessibility tree (axe describe-ui)
57 elements
== screen session (seiji-sim: capture, H.264, HID)
avc1.64001F 602x1310, first keyframe 33799 bytes after 1.6s
== video and log capture (3 s)
video 184320 bytes, log 2048 bytes
== destroy
OK: seiji-alice can boot, capture and delete ios devices
```

The values above show the shape of the output; yours differ. Any failed step exits 1 with the error. Under `os_user` this is the check for the untested paths: simctl for an account that never logged in to the GUI, through `launchctl asuser`, and AXe and `recordVideo` run as a tenant account through `exec-as`. If `create` or `boot` fails, use `isolation.mode: "shared"` ([configuration.md](configuration.md#isolation)). If an AXe step or the capture fails, see [troubleshooting.md](troubleshooting.md#axe-does-not-start).

Doctor does not go through admission: its device is not counted against `capacity` or any budget. Run it when the host is quiet. In shared mode it runs in your SSH session, not under launchd, so a passing doctor does not prove the daemon's context.

## `seiji login`

```sh
seiji login --url <server>
```

A client command. Run it on a developer's Mac, not on the server. It signs you in to a Seiji server for the Tern plugin ([tern.md](tern.md)). In order, it:

1. Checks the URL: it must be `http(s)://host[:port]`, else `the url must be http(s)://host[:port]`. A trailing `/` is dropped.
2. Reads the token from stdin. On a terminal it prompts `Token: ` and does not echo. Piped stdin works too (`pbpaste | seiji login --url …`). An empty token fails with `no token given`.
3. Checks the token with `GET /v1/me`. A `401` fails with `the server refused the token`.
4. Stores the token in the macOS login keychain: a generic password with service `seiji` and the server URL as account.
5. Writes `~/Library/Application Support/seiji/client.json` with `{"url", "user"}`. The file holds no token.

```text
signed in to https://seiji.example.com as alice
```

Signing in again overwrites `client.json` and the keychain item of that URL.

## `seiji logout`

```sh
seiji logout
```

A client command. Deletes the keychain item and `client.json`, and prints `signed out of <url>`. Without `client.json` it prints `not signed in`.

## `seiji token print`

```sh
seiji token print [--json]
```

A client command. Prints the stored token. With `--json` it prints `{"url", "token", "user"}` on one line. The Tern plugin runs `seiji token print --json` to read the credential, so it never stores the token itself.

`SEIJI_URL` and `SEIJI_TOKEN`, when both are set and not empty, override the keychain. `user` is then `null`. Use them in CI and tests.

When you are not signed in, it fails with ``You are not signed in. Run `seiji login --url <server>`.`` With `--json` it prints this instead, and exits 1:

```json
{"error":{"code":"not_logged_in","message":"You are not signed in. Run `seiji login --url <server>`."}}
```

The token is a secret: do not paste the output into logs or chats.

## `seiji update`

```sh
seiji update [--check] [--json] [--tern-plugin DIR]
```

A client command. It brings this CLI, and with `--tern-plugin` the Tern plugin in `DIR`, to the git commit the server runs. The server reports its commit in `GET /v1/me` ([mcp.md](mcp.md#get-v1me)). The clients follow the server, never `main`, so they never call an API the server does not have yet. The Tern plugin runs it when its panel opens and then every hour ([tern.md](tern.md#updates)).

Each part gets one status:

| Status | Meaning |
|---|---|
| `current` | It is built from the server's commit. |
| `outdated` | It is not, and `seiji update` replaces it. |
| `unmanaged` | It is not, but it is not where an install puts it, so `seiji update` leaves it alone. `reason` says why. The CLI is managed only when it runs from `$CARGO_HOME/bin/seiji` (`~/.cargo/bin/seiji`), where `cargo install` puts it; a build in a clone is not. The plugin is managed only when `DIR` is in Tern's plugins directory (`tern plugin dir`); a linked copy (`tern plugin link`) is not. |
| `updated` | `seiji update` just replaced it. |

To update, it fetches the server's commit from the repository into `~/Library/Caches/seiji/source` (a shallow git checkout kept between updates). Then:

1. The CLI: `cargo install --locked --force --path <source>`, with its build directory in `~/Library/Caches/seiji/target`, so the next build reuses it. A first build takes a few minutes.
2. The plugin: it writes the commit to `<source>/tern-plugin/commit` and runs `tern plugin install <source>/tern-plugin --force`. Tern reloads its plugins. The `commit` file tells the next `seiji update` which commit the plugin has; a plugin without it counts as outdated.

`--check` only reports and changes nothing. `--json` prints one object, and the output of git, cargo and Tern goes to stderr:

```json
{"server":"f05d477e95…","cli":{"commit":"9437de6902…","status":"outdated"},"plugin":{"commit":"f05d477e95…","status":"current"}}
```

On failure, `--json` prints `{"error":{"code","message"}}` and exits 1. The codes:

| Code | Cause |
|---|---|
| `not_logged_in` | No credential ([`seiji token print`](#seiji-token-print)). |
| `unreachable` | `GET /v1/me` failed, or the server refused the token. |
| `no_commit` | The server does not report its commit: it runs a Seiji older than `seiji update`, or one built outside a git checkout. |
| `busy` | Another `seiji update` holds `~/Library/Caches/seiji/update.lock`. |
| `failed` | git, cargo or Tern failed. The message names the step. |

Tern is `$TERN_EXE`, then `/Applications/Tern.app/Contents/MacOS/tern`, then `tern` on `PATH`. Cargo is `$CARGO_HOME/bin/cargo`, else `cargo` on `PATH`, and it runs with `$CARGO_HOME/bin` first on `PATH`.

## `seiji forward`

```sh
seiji forward [PORT] [--lease ID] [--expo-go | --dev-client SCHEME | --react-native [BUNDLE_ID]]
```

A client command. Run it on a developer's Mac while you hold a device. It opens a tunnel, so the device on the Seiji host reaches `localhost:PORT` on your Mac: Metro, or any other dev server. [apps.md](apps.md#run-your-dev-server-with-seiji-forward) shows how to use it with Expo and React Native.

| Option | Default | What it does |
|---|---|---|
| `PORT` | `8081` | The port on your Mac. The tunnel dials only this port, on `127.0.0.1`, then on `::1`. |
| `--lease ID` | your only ready lease | The lease whose device gets the tunnel. Needed when you hold two or more devices. |
| `--expo-go` | off | Then calls `open_url` with `exp://<device address>`, which opens the project in Expo Go. |
| `--dev-client SCHEME` | off | Then calls `open_url` with `<SCHEME>://expo-development-client/?url=<http://device address, percent-encoded>`, which opens the project in your Expo development build. |
| `--react-native [BUNDLE_ID]` | off | Then calls `configure_device` with `react_native_bundler: {bundle_id, location: <device address>}` and `launch_app` with `restart: true`, so a React Native debug build loads its bundle through the tunnel. Without `BUNDLE_ID`: the app installed last on the lease. iOS only. |

`--expo-go`, `--dev-client` and `--react-native` exclude each other. In order, it:

1. Reads the credential like `seiji token print`: the keychain, or `SEIJI_URL` and `SEIJI_TOKEN`.
2. Without `--lease`, calls `budget_status` and takes your only lease with the status `ready`.
3. Opens a WebSocket to `GET /v1/leases/{lease_id}/forward` ([mcp.md](mcp.md#get-v1leaseslease_idforward)): `wss://` for an `https://` server, `ws://` for `http://`.
4. Prints the device address: the address at which the device reaches your port. It is `127.0.0.1:<port>` on an iOS simulator and `10.0.2.2:<port>` on an Android emulator. The port on the host is new for each tunnel.
5. With an option that points the app, calls the tools and prints `opened <url>` or `set RCT_jsLocation to <device address> and relaunched the app`. If a call fails, it prints `seiji forward: could not point the app: <cause>` and keeps the tunnel open.
6. Carries every connection that the device makes to the device address to `localhost:PORT`, until Ctrl-C.

```text
Forwarding l_4cfb333bdd6b: the device reaches localhost:8081 at 127.0.0.1:52314
  Expo Go:      exp://127.0.0.1:52314
  Dev build:    <scheme>://expo-development-client/?url=http%3A%2F%2F127.0.0.1%3A52314
  React Native: seiji forward 8081 --react-native [BUNDLE_ID]
Ctrl-C stops.
```

Ctrl-C closes the tunnel and exits 0. When the server closes it, `seiji forward` prints `the tunnel closed (the lease ended, or the server stopped)` and exits 0. While the tunnel is open, the server counts it as activity on the lease every 60 s, so the lease is not released for idling. `maxLeaseMinutes` still applies.

The MCP tool calls that `seiji forward` makes (steps 2 and 5) work with both JSON and SSE responses from the server, so the CLI is compatible with servers that stream MCP over `text/event-stream`.

| Message | Cause |
|---|---|
| ``You are not signed in. Run `seiji login --url <server>`.`` | No credential. Run `seiji login`. |
| `you hold no ready device; acquire one first (acquire_device, or n in Tern)` | No `--lease`, and you hold no `ready` lease. |
| `you hold <N> devices; pick one with --lease (<lease ids>)` | No `--lease`, and you hold two or more `ready` leases. |
| `the server refused the tunnel (<status>): <message>` | The server refused the WebSocket: `403` `forwarding is off on this server` (`forward.enabled` is `false`), `404` with a [lease error](mcp.md#lease-errors), `429` `this lease already has <N> tunnels open` (`forward.maxPerLease`), or `401` for a refused token. |
| `seiji forward: nothing answers on localhost:<PORT> (<cause>)` | The device opened a connection, and nothing listens on `PORT` on your Mac. The device's connection is closed and the tunnel stays open. Start your dev server. |

[troubleshooting.md](troubleshooting.md#tunnels) lists the fixes. [security.md](security.md#tunnels-seiji-forward) explains what the tunnel can reach.

## Exit codes and errors

| Code | When |
|---|---|
| `0` | Success. `seiji serve` also exits 0 after a graceful shutdown. |
| `1` | Any error. stderr shows `Error: <message>`, then a `Caused by:` list. |
| `2` | Usage error from the argument parser (unknown flag, missing argument). |

```text
Error: run `sudo seiji tenant provision alice` first

Caused by:
    no macOS account "seiji-alice"
```

[troubleshooting.md](troubleshooting.md) lists the errors with their causes and fixes.

## Root helpers (internal)

`seiji exec-as`, `seiji exec-drop` and `seiji kill-as` are hidden from `--help`. They are internal: only the daemon calls them, and only through sudo. Do not call them by hand or from scripts. They never read the config, because the daemon account can change files it owns. Each one starts by checking that it runs as root (`must run as root (through sudo)`) and that the target is a tenant account: it exists, has uid 501 or more, is in `seiji-tenants` and is not in `admin`.

| Helper | How the daemon reaches it | What it does |
|---|---|---|
| `seiji exec-as <account> [--pidfile P] [--cwd D] [--env K=V]... -- <command>...` | `sudo -n /usr/local/bin/seiji exec-as …`, allowed by `/etc/sudoers.d/seiji` | Re-runs itself as `exec-drop` through `launchctl asuser <uid>`, so the command joins the tenant account's launchd domain, where its CoreSimulator service lives. |
| `seiji exec-drop` (same arguments) | Only from `exec-as`, already root; not in sudoers | Starts a new session, drops to the tenant account and checks that it cannot regain root. Then, as the tenant, it changes to `--cwd` (default: the account's home), writes the process-group id to `--pidfile` (mode `0644`), sets umask `077`, and runs the command with a cleared environment. |
| `seiji kill-as <account> <pgid> <signal>` | `sudo -n /usr/local/bin/seiji kill-as …`, allowed by sudoers | Drops to the tenant account and signals the process group, so the kernel lets it reach only that account's processes. Refuses a pgid of 1 or less. A group that is already gone is not an error. |

[security.md](security.md) explains the privilege chain and why the binary must be root-owned.
