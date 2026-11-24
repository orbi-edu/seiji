//! Administration: authorization, unlimited budget, admin endpoints and persistence.

mod common;

use common::*;
use serde_json::{Value, json};

use seiji::config::Quota;
use seiji::store::now;

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

async fn get_me(url: &str, token: &str) -> Value {
    http().get(format!("{url}/v1/me")).bearer_auth(token).send().await.unwrap().json().await.unwrap()
}

async fn get_admin_users(url: &str, token: &str) -> reqwest::Response {
    http().get(format!("{url}/v1/admin/users")).bearer_auth(token).send().await.unwrap()
}

async fn put_limits(url: &str, token: &str, user_id: i64, body: &Value) -> reqwest::Response {
    http().put(format!("{url}/v1/admin/users/{user_id}/limits")).bearer_auth(token).json(body).send().await.unwrap()
}

// ---- /v1/me is_admin ----

#[tokio::test]
async fn me_reflects_admin_status() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let bob_token = add_user(&env, "bob", QUOTA);

    let alice_me = get_me(&env.url, &alice_token).await;
    assert_eq!(alice_me["is_admin"], true);
    assert_eq!(alice_me["user"], "alice");
    assert!(alice_me["quota"]["monthlyUnlimited"] == false);

    let bob_me = get_me(&env.url, &bob_token).await;
    assert_eq!(bob_me["is_admin"], false);
}

// ---- admin endpoints authorization ----

#[tokio::test]
async fn admin_endpoints_deny_unauthenticated_and_non_admin() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let bob_token = add_user(&env, "bob", QUOTA);

    // No token -> 401.
    let no_token = http().get(format!("{}/v1/admin/users", env.url)).send().await.unwrap();
    assert_eq!(no_token.status(), 401);

    let no_token_put = http()
        .put(format!("{}/v1/admin/users/1/limits", env.url))
        .json(&json!({"monthly_minutes": 100, "monthly_unlimited": false, "max_concurrent": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token_put.status(), 401);
    let no_token_leases = http().get(format!("{}/v1/admin/leases", env.url)).send().await.unwrap();
    assert_eq!(no_token_leases.status(), 401);
    let no_token_viewer =
        http().post(format!("{}/v1/admin/leases/l_000000000000/viewer", env.url)).send().await.unwrap();
    assert_eq!(no_token_viewer.status(), 401);

    // Non-admin -> 403.
    let bob_list = get_admin_users(&env.url, &bob_token).await;
    assert_eq!(bob_list.status(), 403);

    let alice_user = env.pool.store.user_by_name("alice").unwrap().unwrap();
    let bob_put = put_limits(
        &env.url,
        &bob_token,
        alice_user.id,
        &json!({"monthly_minutes": 100, "monthly_unlimited": false, "max_concurrent": 1}),
    )
    .await;
    assert_eq!(bob_put.status(), 403);

    // Admin succeeds.
    let alice_list = get_admin_users(&env.url, &alice_token).await;
    assert_eq!(alice_list.status(), 200);
}

// ---- admin list ----

#[tokio::test]
async fn admin_lists_users_with_capacity_and_usage() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    add_user(&env, "bob", QUOTA);

    let resp: Value = get_admin_users(&env.url, &alice_token).await.json().await.unwrap();
    let users = resp["users"].as_array().unwrap();
    assert_eq!(users.len(), 2);

    let alice_entry = users.iter().find(|u| u["name"] == "alice").unwrap();
    assert_eq!(alice_entry["monthly_minutes"], 60);
    assert_eq!(alice_entry["monthly_unlimited"], false);
    assert_eq!(alice_entry["max_concurrent"], 1);
    assert_eq!(alice_entry["max_lease_minutes"], 30);
    assert_eq!(alice_entry["idle_minutes"], 10);
    assert_eq!(alice_entry["used_minutes"], 0.0);
    assert_eq!(alice_entry["active_devices"], 0);
    assert_eq!(alice_entry["disabled"], false);
    assert_eq!(alice_entry["is_admin"], true);

    let bob_entry = users.iter().find(|u| u["name"] == "bob").unwrap();
    assert_eq!(bob_entry["is_admin"], false);

    assert_eq!(resp["capacity"]["ios"], 2);
    assert_eq!(resp["capacity"]["android"], 0);
    assert_eq!(resp["in_use"]["ios"], 0);
}

// ---- admin PUT limits ----

#[tokio::test]
async fn admin_updates_limits_and_persists() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    add_user(&env, "bob", QUOTA);
    let bob = env.pool.store.user_by_name("bob").unwrap().unwrap();

    let resp = put_limits(
        &env.url,
        &alice_token,
        bob.id,
        &json!({"monthly_minutes": 9600, "monthly_unlimited": true, "max_concurrent": 3}),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["monthly_minutes"], 9600);
    assert_eq!(body["monthly_unlimited"], true);
    assert_eq!(body["max_concurrent"], 3);
    // Lease/idle settings are preserved.
    assert_eq!(body["max_lease_minutes"], 30);
    assert_eq!(body["idle_minutes"], 10);

    // Persisted to the database.
    let bob_after = env.pool.store.user_by_name("bob").unwrap().unwrap();
    assert_eq!(bob_after.quota.monthly_minutes, 9600);
    assert!(bob_after.quota.monthly_unlimited);
    assert_eq!(bob_after.quota.max_concurrent, 3);
    assert_eq!(bob_after.quota.max_lease_minutes, 30);
    assert_eq!(bob_after.quota.idle_minutes, 10);
}

#[tokio::test]
async fn admin_update_rejects_bad_input_and_unknown_user() {
    let env = setup_with(2, json!({ "adminUsers": ["alice"] })).await;
    let alice_token = add_user(&env, "alice", QUOTA);
    let alice = env.pool.store.user_by_name("alice").unwrap().unwrap();

    // Missing field.
    let r = put_limits(&env.url, &alice_token, alice.id, &json!({"monthly_minutes": 100})).await;
    assert_eq!(r.status(), 400);

    // Negative monthly_minutes.
    let r = put_limits(
        &env.url,
        &alice_token,
        alice.id,
        &json!({"monthly_minutes": -1, "monthly_unlimited": false, "max_concurrent": 1}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Zero concurrency.
    let r = put_limits(
        &env.url,
        &alice_token,
        alice.id,
        &json!({"monthly_minutes": 100, "monthly_unlimited": false, "max_concurrent": 0}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Unknown field.
    let r = put_limits(
        &env.url,
        &alice_token,
        alice.id,
        &json!({"monthly_minutes": 100, "monthly_unlimited": false, "max_concurrent": 1, "role": "admin"}),
    )
    .await;
    assert_eq!(r.status(), 400);

    // Unknown user id.
    let r = put_limits(
        &env.url,
        &alice_token,
        99999,
        &json!({"monthly_minutes": 100, "monthly_unlimited": false, "max_concurrent": 1}),
    )
    .await;
    assert_eq!(r.status(), 404);
}

// ---- unlimited budget ----

#[tokio::test]
async fn unlimited_budget_admits_and_meters_usage() {
    let unlimited = Quota {
        monthly_minutes: 0,
        monthly_unlimited: true,
        max_concurrent: 1,
        max_lease_minutes: 30,
        idle_minutes: 10,
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", unlimited);
    let c = client(&env, &token).await;

    // monthlyMinutes is 0 but unlimited=true, so admission succeeds.
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    assert_eq!(lease["status"], "ready");

    let t = now();
    let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
    conn.execute(
        "UPDATE leases SET started_at = ?1 WHERE id = ?2",
        rusqlite::params![t - 120, lease["lease_id"].as_str().unwrap()],
    )
    .unwrap();
    let user = env.pool.store.user_by_token(&token).unwrap().unwrap();
    assert_eq!(env.pool.store.used_seconds(user.id, t - 120, t, t).unwrap(), 120);

    // budget_status shows unlimited with null remaining.
    let status = ok_json(&call(&c, "budget_status", json!({})).await);
    assert_eq!(status["monthly_unlimited"], true);
    assert!(status["remaining_minutes"].is_null(), "remaining_minutes must be null when unlimited");
    assert_eq!(status["monthly_minutes"], 0);

    call(&c, "release_device", json!({})).await;
}

#[tokio::test]
async fn zero_budget_without_unlimited_blocks_admission() {
    let zero = Quota {
        monthly_minutes: 0,
        monthly_unlimited: false,
        max_concurrent: 1,
        max_lease_minutes: 30,
        idle_minutes: 10,
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", zero);
    let c = client(&env, &token).await;

    let r = call(&c, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&r));
    assert!(text(&r).contains("monthly budget exhausted"), "{}", text(&r));
}

// ---- unlimited still enforces concurrency, capacity, lease duration, idle ----

#[tokio::test]
async fn unlimited_preserves_concurrency_and_capacity() {
    let unlimited = Quota {
        monthly_minutes: 0,
        monthly_unlimited: true,
        max_concurrent: 1,
        max_lease_minutes: 30,
        idle_minutes: 10,
    };
    let env = setup(1).await; // capacity = 1
    let token = add_user(&env, "alice", unlimited);
    let bob_token = add_user(&env, "bob", unlimited);
    let c = client(&env, &token).await;
    let bc = client(&env, &bob_token).await;

    ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);

    // Concurrency limit (max_concurrent=1).
    let second = call(&c, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&second) && text(&second).contains("your limit is 1"), "{}", text(&second));

    // Capacity limit (ios=1).
    let full = call(&bc, "acquire_device", json!({"platform": "ios"})).await;
    assert!(is_err(&full) && text(&full).contains("host is full"), "{}", text(&full));

    call(&c, "release_device", json!({})).await;
}

#[tokio::test]
async fn reaper_skips_budget_for_unlimited_but_enforces_lease_duration() {
    let unlimited = Quota {
        monthly_minutes: 0,
        monthly_unlimited: true,
        max_concurrent: 1,
        max_lease_minutes: 30,
        idle_minutes: 10,
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", unlimited);
    let c = client(&env, &token).await;

    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let id = lease["lease_id"].as_str().unwrap();

    // Push started_at 31 minutes ago: exceeds max_lease_minutes (30).
    let t = now();
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        conn.execute("UPDATE leases SET started_at = ?1 WHERE id = ?2", rusqlite::params![t - 31 * 60, id]).unwrap();
    }

    // Reaper should NOT release for budget (unlimited) but SHOULD release for max_lease_duration.
    env.pool.reap().await.unwrap();
    let ended = env.pool.store.lease(id).unwrap().unwrap();
    assert_eq!(ended.end_reason.as_deref(), Some("max_lease_duration"));
}

#[tokio::test]
async fn reaper_releases_idle_unlimited_leases() {
    let unlimited = Quota {
        monthly_minutes: 0,
        monthly_unlimited: true,
        max_concurrent: 1,
        max_lease_minutes: 60,
        idle_minutes: 5,
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", unlimited);
    let c = client(&env, &token).await;

    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    let id = lease["lease_id"].as_str().unwrap();

    // Push last_activity_at 6 minutes ago: exceeds idle_minutes (5).
    let t = now();
    {
        let conn = rusqlite::Connection::open(env.pool.config.db_path()).unwrap();
        conn.execute("UPDATE leases SET last_activity_at = ?1 WHERE id = ?2", rusqlite::params![t - 6 * 60, id])
            .unwrap();
    }

    env.pool.reap().await.unwrap();
    let ended = env.pool.store.lease(id).unwrap().unwrap();
    assert_eq!(ended.end_reason.as_deref(), Some("idle"));
}

// ---- CLI partial update preserves budget/unlimited/concurrency ----

#[tokio::test]
async fn cli_partial_set_preserves_budget_and_unlimited() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join("maestro");
    std::fs::write(&stub, "#!/bin/sh\ntrue\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    let config_json = serde_json::json!({
        "dataDir": dir.path().join("data"),
        "seats": 3,
        "costPerDeviceHourUsd": 1.5,
        "capacity": { "ios": 2, "android": 0 },
        "defaultQuota": QUOTA,
        "adminUsers": ["admin"],
        "isolation": { "mode": "shared" },
        "ios": {
            "models": { "iphone-16": { "deviceType": "iPhone-16", "runtime": "iOS-18" } },
            "defaultModel": "iphone-16"
        },
        "maestro": { "bin": stub }
    });
    let config_path = dir.path().join("seiji.json");
    std::fs::write(&config_path, config_json.to_string()).unwrap();

    // Add a user via CLI so the DB is populated.
    let add = std::process::Command::new(env!("CARGO_BIN_EXE_seiji"))
        .args(["user", "add", "alice", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(add.status.success(), "user add failed: {}", String::from_utf8_lossy(&add.stderr));

    // Set alice to unlimited + concurrency 3 via the store (simulating admin PUT).
    {
        let store = seiji::store::Store::open(&dir.path().join("data").join("seiji.db")).unwrap();
        let alice = store.user_by_name("alice").unwrap().unwrap();
        store.set_admin_limits(alice.id, 9600, true, 3).unwrap();
    }

    // Run the real CLI: `seiji user set alice --max-lease-minutes 60 --idle-minutes 15 --config <path>`
    let set = std::process::Command::new(env!("CARGO_BIN_EXE_seiji"))
        .args(["user", "set", "alice", "--max-lease-minutes", "60", "--idle-minutes", "15", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(set.status.success(), "user set failed: {}", String::from_utf8_lossy(&set.stderr));
    assert!(String::from_utf8_lossy(&set.stdout).contains("updated alice"));

    // Verify the database: budget, unlimited, concurrency preserved; time limits updated.
    let store = seiji::store::Store::open(&dir.path().join("data").join("seiji.db")).unwrap();
    let alice = store.user_by_name("alice").unwrap().unwrap();
    assert_eq!(alice.quota.monthly_minutes, 9600, "monthly_minutes not preserved");
    assert!(alice.quota.monthly_unlimited, "monthly_unlimited not preserved");
    assert_eq!(alice.quota.max_concurrent, 3, "max_concurrent not preserved");
    assert_eq!(alice.quota.max_lease_minutes, 60, "max_lease_minutes not updated");
    assert_eq!(alice.quota.idle_minutes, 15, "idle_minutes not updated");
}

// ---- migration ----

#[tokio::test]
async fn migration_preserves_tokens_history_and_survives_reopen() {
    use seiji::store::{Store, hash_token};

    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("data").join("seiji.db");
    std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();

    let real_token = "sj_test-migration-token-that-is-long-enough-xxx";
    let token_hash = hash_token(real_token);

    // Create a database with the OLD schema (no monthly_unlimited column),
    // a real token hash, a lease (usage history) and a run.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS users (
               id INTEGER PRIMARY KEY,
               name TEXT NOT NULL UNIQUE,
               token_hash TEXT NOT NULL UNIQUE,
               os_user TEXT,
               monthly_minutes INTEGER NOT NULL,
               max_concurrent INTEGER NOT NULL,
               max_lease_minutes INTEGER NOT NULL,
               idle_minutes INTEGER NOT NULL,
               disabled INTEGER NOT NULL DEFAULT 0,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS leases (
               id TEXT PRIMARY KEY,
               user_id INTEGER NOT NULL REFERENCES users(id),
               platform TEXT NOT NULL,
               model TEXT NOT NULL,
               device_id TEXT,
               driver_port INTEGER NOT NULL,
               console_port INTEGER,
               status TEXT NOT NULL,
               started_at INTEGER NOT NULL,
               last_activity_at INTEGER NOT NULL,
               ended_at INTEGER,
               end_reason TEXT,
               billable INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS leases_user ON leases(user_id, started_at);
             CREATE INDEX IF NOT EXISTS leases_active ON leases(status) WHERE status != 'ended';
             CREATE TABLE IF NOT EXISTS apps (
               id TEXT PRIMARY KEY,
               user_id INTEGER NOT NULL REFERENCES users(id),
               filename TEXT NOT NULL,
               path TEXT NOT NULL,
               size INTEGER NOT NULL,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS runs (
               id TEXT PRIMARY KEY,
               user_id INTEGER NOT NULL REFERENCES users(id),
               lease_id TEXT NOT NULL,
               kind TEXT NOT NULL,
               name TEXT,
               tags TEXT NOT NULL DEFAULT '[]',
               status TEXT NOT NULL,
               started_at INTEGER NOT NULL,
               ended_at INTEGER,
               bytes INTEGER NOT NULL DEFAULT 0,
               summary TEXT
             );
             CREATE INDEX IF NOT EXISTS runs_user ON runs(user_id, started_at);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (name, token_hash, os_user, monthly_minutes, max_concurrent, max_lease_minutes, idle_minutes, created_at)
             VALUES ('legacy', ?1, NULL, 1200, 2, 120, 15, 1000000)",
            rusqlite::params![token_hash],
        )
        .unwrap();
        // A billed, ended lease (usage history).
        conn.execute(
            "INSERT INTO leases (id, user_id, platform, model, device_id, driver_port, status, started_at, last_activity_at, ended_at, end_reason, billable)
             VALUES ('l_oldlease0001', 1, 'ios', 'iphone-16', 'UDID-OLD', 9000, 'ended', 1000000, 1001800, 1001800, 'released', 1)",
            [],
        )
        .unwrap();
    }

    // Open with Store, which runs the migration.
    let store = Store::open(&db_path).unwrap();

    // Token authentication survives: lookup by the real token finds the user.
    let authed = store.user_by_token(real_token).unwrap();
    assert!(authed.is_some(), "token authentication must survive migration");
    let user = authed.unwrap();
    assert_eq!(user.name, "legacy");
    assert!(!user.quota.monthly_unlimited);
    assert_eq!(user.quota.monthly_minutes, 1200);
    assert_eq!(user.quota.max_concurrent, 2);

    // Lease/usage history survives.
    let lease = store.lease("l_oldlease0001").unwrap();
    assert!(lease.is_some(), "lease history must survive migration");
    let used = store.used_seconds(user.id, 0, i64::MAX, seiji::store::now()).unwrap();
    assert!(used > 0, "usage must survive migration: got {used}");

    // Set unlimited on the legacy user.
    store.set_admin_limits(user.id, 9600, true, 5).unwrap();
    let updated = store.user_by_name("legacy").unwrap().unwrap();
    assert!(updated.quota.monthly_unlimited);

    // Close and reopen: the column and updated unlimited state persist.
    drop(store);
    let store2 = Store::open(&db_path).unwrap();
    let reopened = store2.user_by_name("legacy").unwrap().unwrap();
    assert!(reopened.quota.monthly_unlimited, "unlimited must persist across reopen");
    assert_eq!(reopened.quota.monthly_minutes, 9600);
    assert_eq!(reopened.quota.max_concurrent, 5);

    // Can add a new user with the column present.
    let quota = Quota {
        monthly_minutes: 600,
        monthly_unlimited: true,
        max_concurrent: 1,
        max_lease_minutes: 60,
        idle_minutes: 10,
    };
    let (new_user, _token) = store2.add_user("new-user", None, quota, 10).unwrap();
    assert!(new_user.quota.monthly_unlimited);
}

// ---- large finite budget ----

#[tokio::test]
async fn large_finite_budget_admits_without_overflow() {
    let big = Quota {
        monthly_minutes: i64::MAX,
        monthly_unlimited: false,
        max_concurrent: 1,
        max_lease_minutes: 30,
        idle_minutes: 10,
    };
    let env = setup(2).await;
    let token = add_user(&env, "alice", big);
    let c = client(&env, &token).await;

    // Should admit: huge budget, zero usage, no overflow.
    let lease = ok_json(&call(&c, "acquire_device", json!({"platform": "ios"})).await);
    assert_eq!(lease["status"], "ready");
    call(&c, "release_device", json!({})).await;
}
