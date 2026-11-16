# Testing

This document tells you how to run the checks, what the tests in `tests/` and the unit tests in `src/` cover, and how the test harness works. It also tells you how to check a real Mac with `seiji doctor` and a manual smoke test, and gives the rules for new tests.

## Contents

- [Run the checks](#run-the-checks)
- [Test layout](#test-layout)
- [The tests](#the-tests)
- [The harness](#the-harness)
- [What the tests do not cover](#what-the-tests-do-not-cover)
- [Check a real host](#check-a-real-host)
- [Manual smoke test on a real Mac](#manual-smoke-test-on-a-real-mac)
- [Rules for new tests](#rules-for-new-tests)

## Run the checks

You need macOS and Rust 1.88 or later. Seiji builds only on macOS: the root helpers use the Darwin signatures of `getgrouplist` and `initgroups`. The tests need no simulator, no Xcode, no AXe, no Maestro and no network. They use the macOS tools `sips` (image previews) and `ditto` (`evidence.zip`), which every Mac has. `tests/tern.rs` also needs `tern` on `PATH`; without it, the test prints `skipped: tern is not on PATH` and passes.

1. Run all tests:

   ```sh
   cargo test
   ```

2. Run one test:

   ```sh
   cargo test --test e2e enforces_concurrency_capacity_and_ownership
   ```

3. Run the tests of one file:

   ```sh
   cargo test --test runs
   ```

4. Run the unit tests only:

   ```sh
   cargo test --lib
   ```

5. Run the linter:

   ```sh
   cargo clippy --all-targets -- -D warnings
   ```

6. Check the format:

   ```sh
   cargo fmt --check
   ```

CI (`.github/workflows/ci.yml`) runs on `macos-15` with `RUSTFLAGS=-D warnings`. It runs `cargo fmt --check`, `cargo clippy --all-targets --locked` and `cargo test --locked`.

## Test layout

| Path | Covers |
|---|---|
| `tests/common/mod.rs` | The harness: `FakeDevices`, the stub `maestro`, `setup`, `setup_with` and the client helpers. It has no tests |
| `tests/e2e.rs` | Leases, budgets, uploads, the reaper and `run_flow`. Real HTTP server, real MCP client, real SQLite and real `Pool` |
| `tests/runs.rs` | Recorded runs (`run_actions`, `run_xctest`), evidence files and hashes, the HTTP downloads, the evidence quota and retention, and the native device tools |
| `tests/viewer.rs` | The viewer: viewer links and tickets, the session cookie, the page's CSP, the shared stream, input, and `share_screen` |
| `tests/tern.rs` | The Tern plugin in a headless Tern (`tern serve`), with the real `seiji` CLI. Skips when `tern` is not on `PATH` |
| `src/actions.rs` | Unit tests of selectors, `render_tree` and action validation |
| `src/ios.rs` | Unit tests of the AXe command builders, the AXe error parser and the log predicate |
| `src/evidence.rs` | Unit tests of path cleaning and the `xcresulttool` parser |

## The tests

### `tests/e2e.rs`

Each test starts its own server on `127.0.0.1` and a free port, with a temporary data directory (see [The harness](#the-harness)).

| Test | What it checks |
|---|---|
| `rejects_missing_and_wrong_tokens` | A raw `tools/list` POST to `/mcp` with no token gets `401`. The token `sj_nope` gets `401` |
| `full_session_acquire_upload_install_flow_release` | `acquire_device` returns `status: "ready"`. `POST /v1/apps` with `X-Filename: My App.zip` stores the build as `My_App.zip`, and `install_app` with only `app_id` (no `lease_id`) installs it. `run_flow` with `env` returns a run with `status: "passed"`. Its `maestro/output.txt` (read with `get_run_artifact`) shows that Maestro got `--device FAKE-<lease id>`, `--driver-host-port` and `-e USER_NAME=bob`. The manifest's `input.env_keys` is `["USER_NAME"]`, and the value `bob` is not in the manifest. A failing flow returns `isError: true`, `status: "failed"`, a verdict with `exit 1`, and an image block. `inspect_screen` returns the rendered tree with `Button label="Sign In" id="signin"`. `release_device` destroys exactly that device. `take_screenshot` after the release fails with a text that contains `no device` |
| `enforces_concurrency_capacity_and_ownership` | A second `acquire_device` by the same user fails with `your limit is 1`. A third user gets `host is full: 2/2`. `acquire_device` for `android` fails with `not available on this host`. Bob cannot release Alice's lease (`no active lease`) or take its screenshot, and no device is destroyed. A fourth `add_user` fails with `all 3 seats are taken` |
| `monthly_budget_blocks_new_devices_and_reaper_reclaims` | With an ended lease of 50 minutes in the ledger, `budget_status` returns `used_minutes: 50.0`, `remaining_minutes: 10.0` and `estimated_cost_usd: 1.25`. A new lease moved 11 minutes into the past crosses the budget. One reaper pass ends it as `budget_exhausted` and destroys the device. The next `acquire_device` fails with `monthly budget exhausted` |
| `idle_leases_are_released_and_failed_boots_are_free` | A failed boot returns an error that contains `not billed`, and the device is destroyed. A lease that started 15 minutes ago with its last call 11 minutes ago is ended as `idle` by one reaper pass. The ledger bills only the 15 minutes of that lease, and nothing for the failed boot |
| `expired_builds_are_deleted` | A build with `created_at` older than `appRetentionHours` is deleted from disk by one reaper pass. `install_app` with its `app_id` then fails with `no uploaded app` |
| `uploads_respect_size_and_count_limits` | A 17-byte upload gets `413` and leaves nothing in the user's build directory. An empty upload gets `400`. Two small uploads get `200`. The third gets `429`, and the body contains `the limit is 2` |
| `reaper_ends_leases_of_disabled_users_and_overlong_leases` | Alice is disabled while she holds a lease. Bob's lease is 31 minutes old (limit 30) and was used a minute ago. One reaper pass ends Alice's lease as `user_disabled` and Bob's as `max_lease_duration`. `release_device` on Bob's ended lease then fails with `no active lease` |
| `releasing_during_boot_is_billed_and_destroys_the_device` | With `hang_boot` set, `Pool::acquire` blocks in the boot. `Pool::release` with `released` returns `true`. `acquire` then fails with `released while the device booted`. The lease ends as `released`, is billable (`billable = 1`), and its device is destroyed exactly once |

### `tests/runs.rs`

Each test starts with `ready_device`: a new user acquires an iOS device, uploads `Demo.zip` and installs it. `install_app` must return `bundle_id: "com.example.demo"`.

| Test | What it checks |
|---|---|
| `run_actions_records_every_step_with_video_logs_crashes_and_hashes` | A run of 7 actions (`launch_app`, `tap`, `type` with `submit`, `type` with `secret`, `wait_for` with `contains`, `assert_visible` with `id` and `element_type`, `screenshot`) with a `name` and two tags passes with the verdict `7 of 7 steps passed`, `video: true` and `crash_reports: 1`. The device saw, in order: `op launch_app com.example.demo` (the default app), `tap label=Sign In`, `type ana@example.com`, `key 40` (the `submit`), `type hunter2`. Both captures (video and log) were stopped. `get_run` returns `schema: "seiji.run.v1"`, the `lease_id`, `app.executable: "Demo"` and `device.runtime: "iOS-18"`. `hunter2` is nowhere in the manifest, and step 3 stores `<redacted>`. The files include `video.mp4`, `app.log`, `crashes/Demo-2026-11-16.ips`, `steps/000-launch_app.png` and `steps/006-screenshot-home.png`, and no `.started`. `app.log` shows the default predicate `process == "Demo" OR subsystem BEGINSWITH "com.example.demo"`. Every listed file downloaded from `/v1/runs/{run_id}/files/{path}` matches its `sha256`. `get_run_artifact` returns an image block for a PNG, and fails for `../../seiji.db`. `evidence.zip` gets `200` and starts with `PK`. Bob gets `404` for Alice's `evidence.zip`, and `get_run` fails for him. `export_evidence` starts with `### ✅ Login with a valid account` and has `evidence.zip`, `manifest SHA-256` and `[video.mp4]`. `list_runs` with the tag `PR-7` returns the run, and with the tag `nope` returns nothing |
| `a_failed_step_stops_the_run_and_keeps_the_screen_and_tree` | With `record_video: false` and `screenshot_each_step: false`, a tap on `Missing` fails. The result is `isError` with an image block, `status: "failed"`, and a verdict that starts with `step 2 (tap) failed: tap failed (elementNotFound)`. The step statuses are `passed`, `failed`, `skipped`, and `video` is `false`. The files have `steps/001-tap-failure.png` and no file of step 0. `steps/001-tap-tree.txt` holds the rendered tree with `Welcome, Ana!` |
| `invalid_actions_are_refused_before_anything_runs` | A screenshot name `../x` in action 2 fails the call with `action 2 (screenshot)`. The device saw no input, and `list_runs` is empty |
| `xctest_runs_keep_the_result_bundle_and_per_test_results` | `run_xctest` with an uploaded bundle, `only_testing: ["DemoUITests/LoginTests"]` and a tag fails with `status: "failed"` and the verdict `2 tests: 1 passed, 1 failed, 0 skipped`. The filter reached the device layer. Step 1 of the manifest is `DemoUITests/LoginTests/testLogout()` with the error `XCTAssertTrue failed`. The files include `xctest/result.xcresult.zip`, `xctest/attachments/Login.png`, `xctest/xcodebuild.log` and `video.mp4`. `only_testing: ["A;rm -rf /"]` fails with `test identifier` |
| `evidence_quota_refuses_new_runs_and_retention_frees_it` | With `evidence.maxBytesPerUser: 600` and `evidence.retentionHours: 1`, a first run passes and fills the quota (its manifest alone is larger than 600 bytes), and a second fails with `evidence quota full`. After `ended_at` moves 3601 seconds into the past, one reaper pass deletes the run dir, `get_run` fails, and a third run passes |
| `native_device_operations_default_to_the_installed_app` | `configure_device` with a status bar, `dark`, a location and a `photos` grant returns `applied: status_bar, appearance, location, permissions`. `push_notification` and `terminate_app` without `bundle_id` succeed. The device saw `op permissions grant photos com.example.demo`, `op push_notification com.example.demo {"aps":{"alert":"Hi"}}` and `op terminate_app com.example.demo`. `get_logs` uses the default predicate with `process == "Demo"`. A latitude of 91 fails with `latitude`. A payload above 4096 bytes fails with `APNs allows 4096`. After `uninstall_app`, `launch_app` fails with `no app installed` |
| `a_run_cut_by_a_restart_stays_readable_and_counts_against_the_quota` | A `running` run row with one file `steps/000-tap.png` in its daemon run dir goes through `Pool::recover`. `get_run` then returns `status: "error"`, the verdict `server restarted during the run` and exactly that file. `list_runs` with `status: "error"` shows `bytes` larger than the PNG. `list_runs` with `status: "weird"` fails |

### `tests/viewer.rs`

Each test acquires an iOS device for a new user. The first three open viewer links with `POST /v1/leases/{lease_id}/viewer`; the last one uses `share_screen`.

| Test | What it checks |
|---|---|
| `a_link_opens_one_session_for_its_owner_only` | `POST /v1/leases/{lease_id}/viewer` without a token gets `401`, and with Bob's token on Alice's lease `404`. Alice's link has `control: true` and `expires_in: 60`. The ticket opens once: `303`, then `401`. The page has a CSP with `default-src 'none'` and loads `/view/assets/viewer.js`. A request without the cookie, or with the forged cookie `seiji_view=00`, gets `401`. `info` returns the lease, `model: "iphone-16"`, `control: true` and `busy: false` |
| `viewers_share_one_stream_until_the_lease_ends` | With `viewer.maxViewersPerLease: 2`, the stream is `multipart/x-mixed-replace; boundary=seijiframe`, and its first part is a `Content-Type: image/jpeg` frame. A second viewer streams too; a third gets `429`. The device layer started one stream for all viewers (`streams_started` is 1). After `release_device`, the stream ends and `info` gets `410` |
| `input_maps_to_points_and_respects_view_only_and_runs` | A tap at `0.5, 0.5` reaches the device as `tap 201,437` on the fake's 402×874 screen. A tap with `hold: 0.8` is a long press. `type`, `key` and `button` inputs give `204`. A tap at `x: 2` gets `400`. Input without `X-Seiji-View` gets `400`. A view-only session gets `403`. While a `run_actions` call holds the device, input gets `409` and `info` shows `busy: true`; after the run, input works again |
| `agents_share_a_view_only_link` | `share_screen` with no arguments returns `control: false`, and its link opens with `303` |

### `tests/tern.rs`

The test runs `tern serve` headless, with `tern-plugin/` as a fixture plugin. The plugin runs the real `seiji` CLI (`SEIJI_BIN`), with `SEIJI_URL` and `SEIJI_TOKEN` in place of the keychain, against a Seiji server with `FakeDevices`. The test drives Tern through its control socket (`tern ctl`). It skips when `tern` is not on `PATH`.

| Test | What it checks |
|---|---|
| `the_panel_opens_the_screen_beside_it_and_releases_the_device` | The devices panel lists the lease with `iphone-16` and `alice`. The status segment shows `Seiji` and `min left`. `o` opens a `/view/open/…` link in a browser pane beside the panel. `x` asks `Release iphone-16?`, and enter releases the lease: the panel shows `No devices` and the device was destroyed. Tern's log has no plugin errors |

### Unit tests

| File | Test | What it checks |
|---|---|---|
| `src/actions.rs` | `selectors_match_exact_fields_and_types` | `label: "Sign In"` matches one node and `label: "Sign"` none. `id` with `element_type: "button"` matches (no case, `type`), and with `TextField` does not. `value` matches exactly. `contains: "WELCOME"` matches without case |
| `src/actions.rs` | `rendered_tree_keeps_targets_and_drops_empty_groups` | `render_tree` prints `Button label="Sign In" id="signin" @(20,700 362x50)` and `TextField label="Email" value="ana@example.com" disabled`, and drops the `Group` without a label |
| `src/actions.rs` | `actions_parse_from_one_key_objects_and_validate` | Five one-key actions (`tap`, `type`, `button`, `wait_for`, `screenshot`) parse and validate. These fail: a `tap` with a label and a point, a `tap` with only `x`, a `timeout_seconds` of 31, the name `../etc`, a `wait_for` without a criterion, the URL `not a url`, and an unknown field |
| `src/ios.rs` | `tap_uses_equals_form_so_values_never_become_flags` | A `tap` with the label `--udid=OTHER` builds `--output json tap --label=--udid=OTHER --wait-timeout=5 --element-type=Button --count=2 --udid=U1` |
| `src/ios.rs` | `axe_errors_surface_their_code` | An AXe envelope with `didError: true` becomes an error with `elementNotFound` and the message |
| `src/ios.rs` | `log_predicate_escapes_quotes` | `app_log_predicate` escapes `"` in the executable name |
| `src/evidence.rs` | `tenant_paths_are_cleaned_and_bounded` | `clean_relative` keeps `steps/001-tap.png`, turns `Login screen ✓.png` into `Login_screen__.png`, rejects `../etc/passwd` and `a//b`, turns `.hidden` into `hidden`, and rejects 9 levels |
| `src/evidence.rs` | `xcresult_tests_become_steps` | `xctest_steps` turns two `Test Case` nodes into steps named `AppUITests/LoginTests/testLogin()` (3500 ms) and a `failed` step with the error `XCTAssertTrue failed` |

The tests do not wait for time to pass. They move `started_at`, `last_activity_at`, `created_at` or `ended_at` into the past with a direct SQLite `UPDATE`, then call `Pool::reap()` once.

## The harness

`tests/common/mod.rs` holds the shared code. Each test file declares `mod common;`.

### `setup` and `setup_with`

`setup(ios_capacity)` starts a server with this config. `setup_with(ios_capacity, extra)` overrides top-level keys with `extra`, for example `{"evidence": {"maxBytesPerUser": 600}}`.

| Key | Value |
|---|---|
| `dataDir` | `<temp dir>/data` |
| `seats` | 3 |
| `costPerDeviceHourUsd` | 1.5 |
| `capacity` | `ios`: the argument, `android`: 0 |
| `defaultQuota` | `QUOTA`: `monthlyMinutes: 60`, `maxConcurrent: 1`, `maxLeaseMinutes: 30`, `idleMinutes: 10` |
| `isolation.mode` | `shared` |
| `ios` | One model `iphone-16` (`deviceType: "iPhone-16"`, `runtime: "iOS-18"`), the default |
| `maestro.bin` | The stub `maestro` in the temp dir |
| `maxUploadBytes` | 16 |
| `maxBuildsPerUser` | 2 |

There is no `android` section. `axe`, `evidence`, `xctest` and `listen` keep their defaults, so download links are relative.

The helpers:

| Helper | Does |
|---|---|
| `add_user(env, name, quota)` | Adds a user with `Store::add_user` and returns the token |
| `client(env, token)` | A real rmcp client on `/mcp` with the bearer token |
| `call(client, tool, args)` | Calls a tool and returns the `CallToolResult` |
| `text`, `ok_json`, `any_json`, `is_err`, `has_image` | Read a result: the text blocks, the JSON of a success, the JSON of any result, the `isError` flag, an image block |
| `upload(env, token, name, body)` | `POST /v1/apps` and the JSON body |

### `FakeDevices`

`FakeDevices` implements the `Devices` trait of `src/devices.rs` in memory. `Pool` uses it in place of `HostDevices`, so no `simctl`, AXe, `xcodebuild`, `emulator` or `adb` runs. It simulates a login screen: a `Sign In` button, and the text `Welcome, Ana!` after a tap on `Sign In`.

| Method | Fake behavior |
|---|---|
| `create` | Makes the lease dir, counts the call in `created`, returns the device id `FAKE-<lease id>` |
| `boot` | Succeeds. Fails with `runtime missing` when `fail_boot` is set. Blocks until the lease is released, then fails, when `hang_boot` is set |
| `destroy` | Records the lease id in `destroyed` |
| `install` | Records `<filename>:<file contents>` in `installed`. Returns `bundle_id: "com.example.demo"`, `executable: "Demo"`, `version: "1.2.3"` |
| `screenshot` | Returns four bytes: an empty JPEG (`FF D8 FF D9`) |
| `stream` | Sends frames at `viewer.fps` until it is cancelled, and counts each start in `streams_started` |
| `cleanup_orphans` | Does nothing |
| `ui` | A tap on the label `Missing` fails with `tap failed (elementNotFound): No accessibility element matched --label 'Missing'.`. A tap on `Sign In` sets `signed_in`. Logs `tap label=<label>`, `tap <x>,<y>` (with ` hold <seconds>` for a long press) for a tap on a point, `type <text read from the stdin file>`, `key <code>`, or the debug form of other inputs |
| `describe_ui` | An `Application` `Demo` with a `Button` `Sign In` (`AXUniqueId` `signin`, with a frame). After the sign-in, also a `StaticText` `Welcome, Ana!` |
| `save_screenshot` | Writes a valid 1×1 PNG (`PNG`), so `sips` previews work |
| `start_capture` | Video: writes `fake mp4` to the destination. Log: writes `predicate: <predicate>` and `Demo: started` to `app.log` |
| `stop_capture` | Counts the call in `stopped_captures` |
| `device_op` | Logs `op <name> <bundle id>`, `op permissions <action> <service> <bundle id>`, `op push_notification <bundle id> <payload>`, or the debug form of other operations. `LogShow` returns `log for <predicate>` and logs nothing |
| `run_xctest` | Writes `xcodebuild.log`, `result.xcresult.zip`, `attachments/Login.png` and an `xcresult-tests.json` with one passed and one failed test. Logs `xctest only=<only_testing>`. Returns exit code 65 |
| `collect_crashes` | Writes `crashes/<executable>-2026-11-16.ips` and returns 1 |

A test asserts on `created`, `destroyed`, `installed`, `log()`, `stopped_captures` and `streams_started` to check what Seiji asked the device layer to do.

### The stub `maestro`

`setup_with` writes `MAESTRO_STUB` as a shell script named `maestro` into the temp dir and sets `maestro.bin` to it. `Maestro` in `src/maestro.rs` calls it through the real `Runner`, exactly as it calls the real CLI.

| Input | Stub behavior |
|---|---|
| Every call | Prints `ARGS: <all arguments>`, so a test can check the command line in `maestro/output.txt` |
| `hierarchy` in the arguments | Prints `{"elements":[{"txt":"Login"}]}` and exits 0 |
| A flow file (the last argument) that contains `FAILME` | Prints `Assertion is false: Login visible` and exits 1 |
| Any other flow | Prints `Flow passed` and exits 0 |

The stub writes no `commands-*.json` debug files, so a flow run in the tests has no steps.

### Shared isolation, real HTTP, real MCP

- The tests use `isolation.mode: "shared"`. Every command runs as the account that runs `cargo test`, with no `sudo`. The root helpers do not run.
- The server is the real `seiji::http::router` on a real `TcpListener`. Auth, the `Host` check, the upload route and the `/v1/runs/…` routes are the production code.
- The MCP client is the real rmcp client (`StreamableHttpClientTransport`) with the bearer token in its auth header. The tests see exactly what an agent sees: text, image blocks and `isError`.
- The evidence path is the production code: `copy_out` reads each tenant file with `cat` through the real `Runner`, `hash_tree` hashes it, `sips` makes previews and `ditto` builds `evidence.zip`.
- Users and tokens come from `Store::add_user` in each test. A test uses only the tokens that it made.

### Why no simulators run in CI

The tests must be fast and deterministic, and they must run on any Mac. A real device needs Xcode with a simulator runtime, AXe, Java and Maestro, or the Android SDK. One boot takes 30 to 90 seconds. `os_user` isolation also needs root, macOS tenant accounts, the root-owned binary and the sudoers rule. So CI replaces the device layer with `FakeDevices` and the stub `maestro`, and the code that only a real host runs is checked with `seiji doctor` and the smoke test below.

## What the tests do not cover

| Not covered | Checked by |
|---|---|
| `HostDevices`: `simctl`, `emulator`, `adb`, `sips` screenshots, `INSTALL_SCRIPT` and the Android install | `seiji doctor`, the smoke test |
| AXe against a real simulator: taps, typing, gestures, `describe-ui`, the JSON envelope of a real AXe version (only the argument lists and the error parser are unit-tested) | `seiji doctor` (`describe-ui`), the smoke test |
| The viewer stream on a real simulator: `axe stream-video` and its raw framing (checked against AXe's source only), and viewer input through real AXe | Not verified yet. `seiji doctor` does not test streaming yet |
| AXe and `recordVideo` as a tenant account through `exec-as`. Not verified on a real host yet | `seiji doctor` with a tenant account |
| `recordVideo` stopped with SIGINT (`Runner::interrupt`) giving a playable `video.mp4`, and `log stream` | `seiji doctor` (3 s capture), the smoke test |
| `XCTEST_SCRIPT`: `xcodebuild test-without-building`, `xcresulttool`, and the exact JSON shape of `xcresulttool get test-results tests` (the parser is tolerant, and the unit test uses a hand-written shape) | The smoke test |
| `CRASHES_SCRIPT` and real `.ips` files | The smoke test, with an app that crashes |
| The root helpers (`exec-as`, `exec-drop`, `kill-as`), `tenant provision`, `os_user` isolation, `copy_out` across accounts | `seiji doctor` with a tenant account |
| The real Maestro CLI, its driver ports and `maestro_steps` on real `commands-*.json` files | The smoke test |
| `release_all` (`server_shutdown`) and the lease part of `Pool::recover` (`server_restart`). The run part is tested | The smoke test (restart step) |
| A release or the reaper during a run (`cancelled`), `evidence.maxRunSeconds`, `evidence.maxRunBytes` and `dropped`, the `app.log` cut, a failure while recording the evidence (`run <run id> failed while recording its evidence: …`), a tenant file that would replace one already in the evidence, `xctest.enabled: false` | Not tested yet |
| `scroll`, `swipe`, `button`, `sleep`, `open_url`, `capture_tree`, `assert_not_visible`, `wait_for` with `gone`, `continue_on_error` | Not tested yet |
| `get_run_artifact` for large text (tail and link) and for files without preview (download JSON), `GET /v1/runs/{run_id}/report.md`, `listen.publicUrl` | Not tested yet |
| The iOS-only errors on an Android lease, `run_flow` without `maestro`, `run_flow` on Android | Not tested yet |
| The `open_url` tool, `get_logs` limits, `configure_device` with `clear_status_bar` or `clear_location` | Not tested yet |
| `list_device_models`, `cheat_sheet` (it fetches a URL), `install_app` with `url` (redirects, timeout, download limit) | Not tested yet |
| Upload without `X-Filename`, an upload stream that breaks (`upload failed: …`) | Not tested yet |
| A release during the boot through the MCP tools (the test calls `Pool` directly), `lease <lease id> was already released` | Not tested yet |
| `you hold N devices; pass lease_id`, `lease <lease id> is still booting`, `env` key validation, flow timeout | Not tested yet |

## Check a real host

`seiji doctor` runs the real host path for one user, end to end. It does not create a lease row, so it does not touch the user's budget.

1. Run it as the daemon account, so that the `sudo` chain is the one that `seiji serve` uses:

   ```sh
   sudo -u seiji seiji doctor alice --platform ios
   ```

2. Check Android if the host has an `android` section:

   ```sh
   sudo -u seiji seiji doctor alice --platform android
   ```

In `shared` mode, run it as your own account with `-c seiji.json`.

The command prints one line `== <step>` for each step, and stops at the first failure:

| Step | What it runs |
|---|---|
| `identity` | `/usr/bin/id` as the user's account. Under `os_user` isolation, it shows the tenant account (`seiji-alice`) |
| `maestro --version` | `maestro.bin --version`, with `maestro.javaHome` as `JAVA_HOME`. Only when the config has `maestro` |
| `axe --version` | `axe.bin --version`. iOS only |
| `create <platform> <model>` | Creates a device of the default model. The iOS simulator is named `seiji-doctor-<8 hex>`. The Android emulator takes the first free even console port of `android.consolePortRange` that no active lease uses |
| `boot` | Waits for the boot and prints `booted in Ns` |
| `screenshot` | Takes a screenshot and prints `N bytes of JPEG` |
| `accessibility tree (axe describe-ui)` | Reads the tree with AXe and prints `N elements`. iOS only |
| `video and log capture (3 s)` | Records the screen and streams the `SpringBoard` log for 3 seconds, stops both, and prints `video N bytes, log N bytes`. iOS only |
| `destroy` | Deletes the device and the lease dir. It runs also when an earlier device step failed |

On success, the last line is `OK: <account> can boot, capture and delete <platform> devices`.

> **Status:** Under `os_user` isolation, Seiji runs `simctl`, AXe and `recordVideo` as a tenant account that never logged in to the GUI, through `launchctl asuser`. This path is not yet verified on a real EC2 host. `seiji doctor <user>` is the check. If it fails for iOS, set `isolation.mode` to `"shared"` (see [configuration.md](configuration.md) and [security.md](security.md)). Android emulators on an EC2 Mac (Hypervisor.framework on bare metal) are not verified either; `seiji doctor <user> --platform android` is the check.

AXe uses Apple's private accessibility and HID frameworks. A new Xcode can break it until AXe ships a fix. Run `seiji doctor` after every Xcode update.

`seiji doctor` does not tap, does not run Maestro or `xcodebuild` against the device, does not install an app, and uses no driver port. The smoke test covers these. For failures, see [troubleshooting.md](troubleshooting.md).

## Manual smoke test on a real Mac

Do this after a change to host code (`src/devices.rs`, `src/ios.rs`, `src/maestro.rs`, `src/exec.rs`, `src/tenant.rs`, `src/evidence.rs`), after an Xcode or AXe update, and on a new host. You need a running `seiji serve` with `listen.publicUrl` set, a user with a token in `SEIJI_TOKEN`, an agent or the raw curl session of [mcp.md](mcp.md#raw-curl-session), a simulator build of a sample app, and its `build-for-testing` output.

### Leases and devices

1. Check the server:

   ```sh
   curl -s https://seiji.example.com/healthz
   ```

2. Call `budget_status` and `list_device_models`. Check the quota and the capacity.
3. Call `acquire_device` with `{"platform": "ios"}`. Check that `status` is `ready` and note the `lease_id`.
4. Zip a simulator `.app` of the sample app:

   ```sh
   ditto -c -k --keepParent MyApp.app MyApp.zip
   ```

5. Upload it and note the `app_id`:

   ```sh
   curl -s -H "Authorization: Bearer $SEIJI_TOKEN" -H "X-Filename: MyApp.zip" --data-binary @MyApp.zip https://seiji.example.com/v1/apps
   ```

6. Call `install_app` with the `app_id`. Check that the result has the `bundle_id`, the `executable` and the `version` of the app.
7. Call `configure_device` with `{"status_bar": {"time": "9:41"}, "appearance": "dark"}`. Check for `applied: status_bar, appearance`.
8. Call `launch_app`, then `inspect_screen`. Check that the result lists the app's elements with labels and ids, not an error.
9. Call `take_screenshot`. Check that the image shows the app with the fixed time and dark mode.

### `run_actions`

10. Call `run_actions` with `launch_app`, a `tap` and a `type` with `"secret": true` on a real field of the app, a `wait_for` on the next screen, and a `screenshot` with a name. Check:
    - [ ] `status` is `passed`, and every step is `passed`.
    - [ ] `video` is `true`, `warnings` is empty, and `dropped_files` is 0.
    - [ ] `get_run` lists `video.mp4`, `app.log` and one `steps/NNN-<kind>.png` for each step, and stores the secret text as `<redacted>`.
    - [ ] `get_run_artifact` of `app.log` shows lines of the app's process.
    - [ ] `get_run_artifact` of `steps/NNN-screenshot-<name>.png` returns the screen.
    - [ ] `video.mp4`, downloaded from its `download` link with the token, plays in QuickTime and shows the steps.
11. Call `run_actions` with a `tap` on a label that is not on screen. Check for `isError: true`, `status: "failed"`, a verdict `step 1 (tap) failed: tap failed (elementNotFound): …`, an image block, and `steps/000-tap-failure.png` and `steps/000-tap-tree.txt` in the files.
12. Make the app crash in a run (for example with a debug menu entry). Check that `crash_reports` is 1 or more and that `crashes/<executable>…ips` is in the files.

### `run_xctest`

13. Build and zip the UI tests (see [mcp.md](mcp.md#preparing-an-xctest-bundle)):

    ```sh
    xcodebuild build-for-testing -scheme MyApp -destination 'generic/platform=iOS Simulator' -derivedDataPath DD
    ditto -c -k --keepParent DD/Build/Products MyAppTests.zip
    ```

14. Upload `MyAppTests.zip` and call `run_xctest` with its `app_id`. Check:
    - [ ] The verdict is `<n> tests: <p> passed, <f> failed, <s> skipped`, with the counts that Xcode shows for the same tests.
    - [ ] The steps have one entry for each test, named `<bundle>/<suite>/<test>`, and a failed test has its failure message as `error`.
    - [ ] The files have `xctest/result.xcresult.zip`, `xctest/xcodebuild.log`, `xctest/xcresult-summary.json`, `xctest/xcresult-tests.json`, and `xctest/attachments/…` when the tests add attachments.
    - [ ] `xctest/result.xcresult.zip`, unzipped, opens in Xcode.
15. Call `run_xctest` with `only_testing` of one test class. Check that only its tests appear in the steps.

### `export_evidence` and `evidence.zip`

16. Call `export_evidence` with the `run_id` of step 11. Check:
    - [ ] The heading starts with ❌ and the verdict is the one of the run.
    - [ ] The step table has one row for each step.
    - [ ] The `Files:` links (`video.mp4`, `app.log`, `steps/000-tap-failure.png`) are absolute URLs on `listen.publicUrl`, and open with the token.
    - [ ] The manifest SHA-256 matches `manifest_sha256` of the run result.
17. Download `evidence.zip` and check it:

    ```sh
    curl -s -H "Authorization: Bearer $SEIJI_TOKEN" -o evidence.zip https://seiji.example.com/v1/runs/<run id>/evidence.zip
    unzip -q evidence.zip
    shasum -a 256 <run id>/manifest.json
    ```

    - [ ] The zip has one top folder `<run id>/` with `manifest.json` and every file the manifest lists.
    - [ ] The SHA-256 of `manifest.json` matches `manifest_sha256`.
    - [ ] The SHA-256 of each file matches its entry in `files`.
18. Download the same `evidence.zip` with the token of another user. Check for `404`.

### Concurrency, release and restart

19. If `maxConcurrent` and `capacity.ios` allow it, acquire a second iOS device and run `run_actions` on both leases at the same time. Both must pass. If the host has Maestro, also run a `run_flow` on both. This checks the separate driver ports.
20. Start a long `run_actions` (for example a `sleep` of 60) and call `release_device` during it. Check that the run ends as `cancelled` and that `get_run` still returns its files.
21. Call `release_device` for each lease. Check that the result is `released <lease id>`.
22. Check the ledger:

    ```sh
    sudo -u seiji seiji usage
    ```

    The user's `used min` must have grown by about the length of the leases, and `leases` by their number.

23. Check that no Seiji simulator is left. In `shared` mode:

    ```sh
    xcrun simctl list devices | grep seiji-
    ```

    Under `os_user` isolation the simulators are in the tenant account's device set. Read the daemon log for `lease released` instead.

24. Restart the service during a lease and a run, then call `budget_status`. The lease must be gone from `active_leases`. A graceful stop ends it as `server_shutdown`. After a crash, the next start ends it as `server_restart`. The run that was `running` is listed by `list_runs` as `error` with the verdict `server restarted during the run`, and `get_run` returns its manifest with the warning `the server restarted during the run; only files written before that are kept`.

For Android, repeat steps 3 to 6, 8, 9 and a `run_flow` with `{"platform": "android"}` and an `.apk`. The other tools return `<name> supports iOS simulators only for now` on Android.

## Rules for new tests

These rules are also in [AGENTS.md](../AGENTS.md).

- **Test what a consumer sees.** Check a tool result, an `isError` flag, an HTTP status, a row in the ledger, a file in the evidence, or a device that was or was not destroyed. Do not test private wiring, log text or source text.
- **Be deterministic.** Do not sleep and do not depend on test order. Move timestamps in SQLite and call `Pool::reap()` once, as the existing tests do.
- **Use no network, no simulator, no Xcode, no AXe.** Use `FakeDevices` and the stub `maestro`. Add a behavior to the fake or the stub only when a test needs it.
- **A bug fix starts with a failing test.** Write the test, see it fail, then fix the code.
- **Host-only code is checked on a Mac.** For a change to `HostDevices`, `src/ios.rs` scripts or the root helpers, run `seiji doctor` and the smoke test, and put the output in the pull request.
- Before you finish, run `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
