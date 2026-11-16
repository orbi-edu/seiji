# `seiji` (command line)

`seiji` is one binary. It runs the MCP server, manages users, prints usage, creates tenant accounts, checks the host, and holds the root helpers that the daemon reaches through sudo. On a developer's Mac, the same binary is the client that signs in to a server for the Tern plugin (`seiji login`, `seiji logout`, `seiji token print`). This document lists every command and flag, the account that must run each one, and what each prints. [install.md](install.md) shows the commands in order on a new host. [configuration.md](configuration.md) is the reference of the config file they read.

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
- [Exit codes and errors](#exit-codes-and-errors)
- [Root helpers (internal)](#root-helpers-internal)

## Conventions

- `--config PATH` (`-c PATH`) selects the config file. It is a global flag, so it may come before or after the subcommand: `seiji user add alice -c /path/seiji.json` and `seiji user -c /path/seiji.json add alice` are the same. The default is `/usr/local/etc/seiji/seiji.json`, or `$SEIJI_CONFIG` when it is set. `sudo` drops `SEIJI_CONFIG`, so pass `-c` with `sudo` when the file is somewhere else.
- The root helpers (`exec-as`, `exec-drop`, `kill-as`) take no `--config` and never read the config. The client commands (`login`, `logout`, `token print`) do not read it either.
- User names have 1 to 32 characters of `[a-z0-9-]`. Other names fail with `user names must be 1-32 chars of [a-z0-9-]`.
- `seiji --help` and `seiji <command> --help` print the usage. `seiji --version` prints `seiji <version>`.

## Which account runs what

| Command | Run it as | Why |
|---|---|---|
| `seiji serve` | daemon account `seiji` (launchd `UserName`) | It owns the data directory and is the only account sudoers lets run the root helpers. |
| `seiji user …`, `seiji usage` | daemon account: `sudo -u seiji seiji …` | They open `seiji.db` for writing (WAL mode, table creation), with umask `077`. Run as another account, they fail on file permissions, or create files the daemon cannot read or write. |
| `seiji doctor` | daemon account: `sudo -u seiji seiji doctor …` | It uses the same data directory, runner and sudo chain as the daemon. |
| `seiji tenant provision` | root: `sudo seiji tenant provision …` | It creates macOS accounts and groups. |
| `seiji exec-as`, `seiji exec-drop`, `seiji kill-as` | root, reached only by the daemon through sudo | Internal. See [Root helpers](#root-helpers-internal). |
| `seiji login`, `seiji logout`, `seiji token print` | yourself, on a developer's Mac, not on the server | Client commands. They use your login keychain and your `~/Library/Application Support/seiji/`. |
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

Routes: `/mcp` (Streamable HTTP MCP, bearer token), `POST /v1/apps` (build upload, bearer token), `GET /v1/runs/{run_id}/files/{*path}`, `GET /v1/runs/{run_id}/evidence.zip` and `GET /v1/runs/{run_id}/report.md` (run evidence, bearer token, only the token's own runs), `GET /v1/me`, `POST /v1/leases/{lease_id}/viewer` and `GET /v1/leases/{lease_id}/screenshot.jpg` (bearer token, only the token's own leases), the viewer routes under `/view/` (viewer session cookie), and `GET /healthz` (no token, answers `ok`). [mcp.md](mcp.md) documents them.

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
| `--monthly-minutes N` | `monthlyMinutes` | At least 0. `0` blocks every acquire. |
| `--max-concurrent N` | `maxConcurrent` | At least 1. |
| `--max-lease-minutes N` | `maxLeaseMinutes` | At least 1. |
| `--idle-minutes N` | `idleMinutes` | At least 1. |

A value that breaks a rule fails with `quotas must be positive (monthly-minutes may be 0)`. A name that does not exist fails with `no user "<name>"`.

Output of `add`:

```text
user alice added
token (shown once): sj_rOp-22QxFnp6KMfO59XAHMnR2cLp01NIlueqyY83nac
```

Tokens start with `sj_`. Seiji stores only their SHA-256 hash, so a lost token cannot be shown again: rotate it.

Output of `list` (`os account` is `-` in shared mode):

```text
name             os account             min/mo  conc lease-min   idle  status
alice            seiji-alice              1200     2       120     15  active
bob              seiji-bob                3000     1       120     15  disabled
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
2. Creates the account with `dscl` if it does not exist: uid one above the highest existing uid (at least 600), primary group 20 (`staff`), home `/Users/<account>`, shell `/usr/bin/false`, no password (`Password *`), so nobody can log in as it. Then adds it to `seiji-tenants`.
3. Hides it from the login window (`IsHidden 1`), creates its home and sets the home to mode `700`.
4. Checks the result with the same rules as the root helpers: the account exists, has uid 501 or more, is in `seiji-tenants` and is not in `admin`.

Running it again on a tenant account is safe; it repairs the hidden flag, the home and its mode. It refuses an existing account that is not already a tenant account (not in `seiji-tenants`, an admin, or a uid below 501): `"<account>" already exists and is not a Seiji tenant account; refusing to change it`. Without root it fails with `must run as root (through sudo)`.

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
| `create <platform> <model>` | always | Creates a device of the platform's `defaultModel`: simulator `seiji-doctor-<8 hex>`, or an emulator on the first free even port of `android.consolePortRange` that no active lease holds. |
| `boot` | always | Waits for boot with the configured boot timeout. |
| `screenshot` | always | Captures a JPEG through `sips`. |
| `accessibility tree (axe describe-ui)` | `--platform ios` | Reads the accessibility tree with `axe describe-ui` and prints the number of elements. |
| `video and log capture (3 s)` | `--platform ios` | Starts `xcrun simctl io <udid> recordVideo` and `xcrun simctl spawn <udid> log stream` (predicate `process == "SpringBoard"`) as the user's account, waits 3 s, stops the video with `SIGINT` and the log stream, then prints both sizes. It fails with `video file exited <code>: <output>` when no video file was written. A log of 0 bytes does not fail the step. |
| `destroy` | always, also when an earlier step failed | Deletes the device and its directory. |

```text
== identity
uid=601(seiji-alice) gid=20(staff) groups=20(staff),701(seiji-tenants)
== maestro --version
<maestro version>
== axe --version
<axe version>
== create ios iphone-16
device <UDID>
== boot
booted in 38s
== screenshot
412233 bytes of JPEG
== accessibility tree (axe describe-ui)
57 elements
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
