# AGENTS.md

This file gives the rules for AI coding agents that work on this repository. Human contributors
must also obey these rules. Read all of this file before you change code.

## 1. What this repository is

Seiji lends iOS simulators and Android emulators on one shared Mac to AI agents. Agents call it
over MCP (Streamable HTTP, one bearer token per user). Seiji boots a fresh device for each lease,
drives it with the Maestro CLI, meters every lease in a SQLite ledger, and enforces seats, monthly
budgets, concurrency, host capacity, lease length and idle release. With `os_user` isolation,
every user is a separate macOS account, and the unprivileged daemon reaches it only through the
root helpers `seiji exec-as` and `seiji kill-as`. People can watch and drive a lease's iOS
simulator in the viewer, from a browser or the Tern plugin.

Read these documents before you change a component:

| You change | Read first |
|---|---|
| Any code | [docs/architecture.md](docs/architecture.md), [docs/concepts.md](docs/concepts.md) |
| Admission, leases, the reaper, billing | [docs/concepts.md](docs/concepts.md) |
| `src/tenant.rs`, `src/exec.rs`, sudoers, permissions | [docs/security.md](docs/security.md) |
| MCP tools or HTTP routes | [docs/mcp.md](docs/mcp.md) |
| `src/viewer.rs`, `src/viewer/`, `/view/…` routes | [docs/security.md](docs/security.md#the-viewer), [docs/mcp.md](docs/mcp.md#viewer-routes) |
| `tern-plugin/`, `src/client.rs` | [docs/tern.md](docs/tern.md), [docs/cli.md](docs/cli.md) |
| `src/forward.rs`, `seiji forward`, `/v1/leases/{lease_id}/forward` | [docs/apps.md](docs/apps.md), [docs/security.md](docs/security.md#tunnels-seiji-forward), [docs/mcp.md](docs/mcp.md#get-v1leaseslease_idforward) |
| Config keys | [docs/configuration.md](docs/configuration.md) |
| CLI commands or flags | [docs/cli.md](docs/cli.md) |
| Install steps, `deploy/` | [docs/install.md](docs/install.md) |
| Tests | [docs/testing.md](docs/testing.md) |

The documents are part of the interface. If your change makes a document wrong, change the
document in the same commit.

## 2. Repository layout

| Path | Contents |
|---|---|
| `src/main.rs` | The `seiji` CLI: `serve`, `user`, `usage`, `tenant`, `doctor`, the client commands `login`, `logout`, `token print` and `forward`, and the hidden root helpers. |
| `src/config.rs` | `seiji.json`: types, defaults, validation. |
| `src/store.rs` | SQLite: users, leases (the usage ledger), builds, runs. Month windows and token hashing. |
| `src/pool.rs` | Admission, acquire and release, operations on a lease, the reaper, recovery. |
| `src/runs.rs` | Recorded runs (`run_actions`, `run_xctest`, `run_flow`), native device operations, logs, run lookup. |
| `src/actions.rs` | The `run_actions` step model: `Action`, `Selector`, validation, execution, `render_tree`, redaction. |
| `src/evidence.rs` | Evidence: copy-out from the tenant, manifest and SHA-256s, Markdown summary, step parsers, zip. |
| `src/devices.rs` | The `Devices` trait and `HostDevices`: lifecycle, input, captures, device ops, XCTest, crashes. |
| `src/ios.rs` | Command builders for `simctl` and AXe, and the install, XCTest and crash scripts. |
| `src/maestro.rs` | Maestro CLI calls with a driver port for each lease. |
| `src/exec.rs` | `Runner`: runs a command as a user's account, captures output or streams it to a file, kills or interrupts process groups. |
| `src/tenant.rs` | Root helpers and tenant account provisioning. Security-critical. |
| `src/mcp.rs` | The MCP tools. |
| `src/http.rs` | axum router: `/mcp`, `/v1/apps`, `/v1/runs/…`, `/v1/me`, `/v1/leases/{lease_id}/viewer`, `/v1/leases/{lease_id}/screenshot.jpg`, `/v1/leases/{lease_id}/forward` (WebSocket), `/healthz`, bearer auth. Merges the viewer routes. |
| `src/viewer.rs` | The viewer: viewer links, tickets and viewer sessions (in memory), the `/view/…` routes and cookie checks, one shared AXe stream per lease, input under the op lock. Security-critical. |
| `src/viewer/` | The viewer page assets, embedded in the binary: `index.html`, `viewer.js`, `viewer.css`. |
| `src/forward.rs` | `seiji forward`: the tunnel protocol (hello, OPEN, DATA and CLOSE frames), tunnel admission and the server loop on a loopback port, and the client end that dials one local port. |
| `src/client.rs` | The client commands on a developer's Mac: `login` (token into the keychain), `logout`, `token print`, and `forward` (picks the lease, runs the tunnel, points the app at it). |
| `tern-plugin/` | The Tern plugin: `plugin.toml`, `host.luau`, `window.luau`, `styles.css`, `lib/*.luau`. |
| `tests/common/mod.rs` | Test harness: real HTTP and MCP, `FakeDevices` (a login screen and a fake stream), a stub `maestro`. |
| `tests/e2e.rs`, `tests/runs.rs` | End-to-end tests: leases and billing; recorded runs and evidence. |
| `tests/viewer.rs`, `tests/tern.rs` | The viewer; the Tern plugin in a headless Tern (skips without `tern` on `PATH`). |
| `tests/forward.rs` | `seiji forward`: the tunnel end to end, its refusals, and the real CLI pointing the app. |
| `assets/` | Logo: `logo-mark.svg`, `logo-light.svg`, `logo-dark.svg`. |
| `deploy/` | Example config, sudoers rule, launchd plist. |
| `docs/` | User and operator documentation. |

## 3. Commands

| Task | Command |
|---|---|
| Build | `cargo build` |
| Run all tests | `cargo test` |
| Run one test | `cargo test --test e2e enforces_concurrency_capacity_and_ownership` |
| Lint | `cargo clippy --all-targets -- -D warnings` |
| Format | `cargo fmt` (check with `cargo fmt --check`) |
| Run locally | `cargo run -- serve -c seiji.json` with `isolation.mode` `"shared"` |
| Check a real host | `seiji doctor <user> --platform ios` |

The tests need no simulator, no Xcode and no network. Seiji builds only on macOS: the root
helpers use the Darwin signatures of `getgrouplist` and `initgroups`.

## 4. Rules that you must obey

### 4.1 Language

- Write all code, comments, documentation, commit messages, log events and user-facing text in
  English.
- Write short sentences. Use the active voice. Use one term for one thing in all files. The terms
  are in [docs/glossary.md](docs/glossary.md).

### 4.2 Privilege boundary

These rules protect the host. A change that breaks one of them is a security bug.

- The daemon never runs as root. Only `seiji exec-as`, `seiji exec-drop`, `seiji kill-as` and
  `seiji tenant provision` run as root.
- The root helpers never read `seiji.json` or any other file that the daemon account can write.
  Everything that decides who a tenant is, is fixed in `src/tenant.rs`: the group
  `TENANT_GROUP` (`seiji-tenants`), uid 501 or higher, and not a member of `admin` (gid 80).
- Every root helper calls `validate_tenant` before it acts. Do not add a root path that skips it.
- `exec-drop` drops privileges with `initgroups`, `setgid`, `setuid`, in that order, and then
  checks that `setuid(0)` fails. It writes the pidfile only after the drop. Keep this order.
- Do not add a new sudoers entry or a new root subcommand without a review of
  [docs/security.md](docs/security.md) and an update to it in the same change.
- Every command for a user's device goes through `Runner` (`src/exec.rs`). Do not call
  `std::process::Command` or `tokio::process::Command` for device work anywhere else.
- Pass arguments to shell scripts as positional parameters (`CmdSpec::sh(script, args)`). Never
  format user input into a script string.

### 4.3 Credentials

- A token is shown once, at `user add` or `user rotate-token`. The database stores only its
  SHA-256. The server never logs, prints or returns a token after that.
- On a developer's Mac, `seiji login` keeps the token in the macOS login keychain, never in a
  file. Only `seiji token print` prints it, for the signed-in user and the Tern plugin.
- Never put a token in a command line, an environment variable of a device process, or a file
  in a workspace.
- A test uses the tokens that the test itself creates.

### 4.4 Billing and admission

- The `leases` table is the only source of usage. Do not add another counter.
- Admission is atomic: the checks and the insert of the `booting` lease happen under
  `Pool.admission`. Do not move an `await` that releases the lock between a check and the insert.
- A lease ends exactly once, through `Store::end_lease`. Only the caller whose `end_lease`
  returns `true` destroys the device.
- A failed boot is not billed (`boot_failed`, `billable = 0`). Every other end reason is billed.
- New end reasons are part of the interface. Add them to [docs/concepts.md](docs/concepts.md)
  and [docs/glossary.md](docs/glossary.md).

### 4.5 Devices

- Seiji deletes only what it made: simulators named `seiji-<lease id>`, emulators on a console
  port in `android.consolePortRange`, and lease directories. Never delete or shut down any other
  device.
- One device has at most one operation at a time (the lease's `op` lock). Two Maestro processes
  on one device break each other.
- Every lease has its own driver port. Do not use `maestro mcp`: it fixes the iOS driver port at
  22087.
- A lease belongs to one user. Resolve leases only through `Pool::resolve`, which checks the
  owner.

### 4.6 Evidence

- Files a tenant writes are untrusted. The daemon reads them only through `evidence::copy_out`,
  which reads each file as the tenant, cleans its path with `clean_relative` and applies the
  byte budget. Do not open a path in a tenant's directory from the daemon.
- Serve only files that a run's manifest lists, plus `manifest.json`, and only to the run's
  owner. Every new route or tool that returns run files goes through `Pool::run_file` or
  `Pool::run_manifest`.
- Secrets stay out of evidence and command lines. Typed text and APNs payloads reach the tool
  through stdin files. `type` steps with `secret: true` are stored through `Action::redacted`.
  `run_flow` keeps only env keys. A new input that can carry a secret needs the same treatment.
- Every byte a run keeps counts against `evidence.maxBytesPerUser`. Do not write evidence that
  bypasses the run's budget.
- The manifest schema is `seiji.run.v1`. Adding a field is fine; renaming or removing one needs
  a new schema version.

### 4.7 Change discipline

- Make the smallest change that solves the task. Do not refactor code that the task does not
  need.
- Do a clean cutover. Remove the old code path, aliases and dead tests in the same change.
- Do not add a dependency without a clear need. Commit `Cargo.lock`.
- A new config key needs a default or a validation error, and a row in
  [docs/configuration.md](docs/configuration.md). `deny_unknown_fields` stays on every config
  struct.

### 4.8 Tests

- A bug fix comes with a test that fails before the fix.
- A test checks behavior that a user or an operator can see: a tool result, an HTTP status, a
  row in the ledger, a device that was or was not destroyed. Do not test private wiring, log
  text or source text.
- Tests are deterministic and isolated. Use `FakeDevices` and the stub `maestro` in
  `tests/common/mod.rs`. Do not depend on the network, a simulator or test order.
- Code that only a real host can exercise (`HostDevices`, the root helpers) is verified with
  `seiji doctor` on a Mac. Say so in the pull request.
- Before you finish, run `cargo test`, `cargo clippy --all-targets -- -D warnings` and
  `cargo fmt --check`. Do not finish with a failure.

## 5. Commits and pull requests

- One commit has one purpose. Write the subject in the imperative mood, 72 characters or less,
  with a type prefix: `feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`.
- The body says what changed and why. It does not repeat the diff.
- A pull request links its issue, says how you verified the change (tests, and `seiji doctor`
  output for host code), and lists the risks.

## 6. When you are not sure

- Read the code and the documents first. Most answers are there.
- If a requirement is not clear, stop and ask. Do not guess about privileges, credentials,
  billing or public interfaces.
