//! SQLite persistence: users, leases (the usage ledger) and uploaded apps.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::config::{Platform, Quota};

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock before 1970").as_secs() as i64
}

/// `[start, end)` of the UTC calendar month containing `ts`, in unix seconds.
pub fn month_window(ts: i64) -> (i64, i64) {
    let dt = time::OffsetDateTime::from_unix_timestamp(ts).expect("valid timestamp");
    let start = time::Date::from_calendar_date(dt.year(), dt.month(), 1).expect("valid date");
    let (ny, nm) = if dt.month() == time::Month::December {
        (dt.year() + 1, time::Month::January)
    } else {
        (dt.year(), dt.month().next())
    };
    let end = time::Date::from_calendar_date(ny, nm, 1).expect("valid date");
    (start.midnight().assume_utc().unix_timestamp(), end.midnight().assume_utc().unix_timestamp())
}

/// Parses `YYYY-MM` into its month window.
pub fn parse_month(s: &str) -> Result<(i64, i64)> {
    let (y, m) = s.split_once('-').context("month must be YYYY-MM")?;
    let month = time::Month::try_from(m.parse::<u8>()?)?;
    let date = time::Date::from_calendar_date(y.parse()?, month, 1)?;
    Ok(month_window(date.midnight().assume_utc().unix_timestamp()))
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_token() -> String {
    use base64::Engine;
    let bytes: [u8; 32] = rand::random();
    format!("sj_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub name: String,
    /// macOS account the user's devices run under (`os_user` isolation only).
    pub os_user: Option<String>,
    pub quota: Quota,
    pub disabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseStatus {
    Booting,
    Ready,
    Ended,
}

impl LeaseStatus {
    fn as_str(self) -> &'static str {
        match self {
            LeaseStatus::Booting => "booting",
            LeaseStatus::Ready => "ready",
            LeaseStatus::Ended => "ended",
        }
    }
    fn parse(s: &str) -> Self {
        match s {
            "booting" => LeaseStatus::Booting,
            "ready" => LeaseStatus::Ready,
            _ => LeaseStatus::Ended,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Lease {
    pub id: String,
    pub user_id: i64,
    pub platform: Platform,
    pub model: String,
    pub device_id: Option<String>,
    pub driver_port: u16,
    pub console_port: Option<u16>,
    pub status: LeaseStatus,
    pub started_at: i64,
    pub last_activity_at: i64,
    pub ended_at: Option<i64>,
    pub end_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct App {
    pub id: String,
    pub user_id: i64,
    pub filename: String,
    pub path: String,
    pub size: i64,
}

#[derive(Debug, Clone)]
pub struct UsageRow {
    pub user: User,
    pub used_seconds: i64,
    pub leases: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
  id INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  token_hash TEXT NOT NULL UNIQUE,
  os_user TEXT,
  monthly_minutes INTEGER NOT NULL,
  monthly_unlimited INTEGER NOT NULL DEFAULT 0,
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
CREATE INDEX IF NOT EXISTS runs_user ON runs(user_id, started_at);
"#;

const USER_COLS: &str =
    "id, name, os_user, monthly_minutes, monthly_unlimited, max_concurrent, max_lease_minutes, idle_minutes, disabled";
const LEASE_COLS: &str = "id, user_id, platform, model, device_id, driver_port, console_port, status, started_at, last_activity_at, ended_at, end_reason";

fn user_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: r.get(0)?,
        name: r.get(1)?,
        os_user: r.get(2)?,
        quota: Quota {
            monthly_minutes: r.get(3)?,
            monthly_unlimited: r.get::<_, i64>(4)? != 0,
            max_concurrent: r.get(5)?,
            max_lease_minutes: r.get(6)?,
            idle_minutes: r.get(7)?,
        },
        disabled: r.get::<_, i64>(8)? != 0,
    })
}

fn lease_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Lease> {
    let platform: String = r.get(2)?;
    let status: String = r.get(7)?;
    Ok(Lease {
        id: r.get(0)?,
        user_id: r.get(1)?,
        platform: Platform::parse(&platform).unwrap_or(Platform::Ios),
        model: r.get(3)?,
        device_id: r.get(4)?,
        driver_port: r.get::<_, i64>(5)? as u16,
        console_port: r.get::<_, Option<i64>>(6)?.map(|p| p as u16),
        status: LeaseStatus::parse(&status),
        started_at: r.get(8)?,
        last_activity_at: r.get(9)?,
        ended_at: r.get(10)?,
        end_reason: r.get(11)?,
    })
}

/// Billable seconds of all leases of `user_id` that overlap `[from, to)`, clipped to the window.
/// Active leases count up to `now`.
const USAGE_SQL: &str =
    "SELECT COALESCE(SUM(MAX(0, MIN(COALESCE(ended_at, ?3), ?2) - MAX(started_at, ?1))), 0), COUNT(*)
     FROM leases WHERE user_id = ?4 AND billable = 1 AND started_at < ?2 AND COALESCE(ended_at, ?3) > ?1";

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        // Serialize schema inspection and migration across concurrent CLI/server opens.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let has_col: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('users') WHERE name = 'monthly_unlimited')",
            [],
            |r| r.get(0),
        )?;
        if !has_col {
            tx.execute_batch("ALTER TABLE users ADD COLUMN monthly_unlimited INTEGER NOT NULL DEFAULT 0")?;
        }
        tx.commit()?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex poisoned")
    }

    // ---- users ----

    /// Creates a user and returns its plaintext token (shown once). Enforces the seat limit.
    pub fn add_user(&self, name: &str, os_user: Option<&str>, quota: Quota, seats: i64) -> Result<(User, String)> {
        let conn = self.conn();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
        if count >= seats {
            bail!("all {seats} seats are taken; raise `seats` in the config or remove a user");
        }
        let token = new_token();
        conn.execute(
            "INSERT INTO users (name, token_hash, os_user, monthly_minutes, monthly_unlimited, max_concurrent, max_lease_minutes, idle_minutes, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                name,
                hash_token(&token),
                os_user,
                quota.monthly_minutes,
                quota.monthly_unlimited as i64,
                quota.max_concurrent,
                quota.max_lease_minutes,
                quota.idle_minutes,
                now()
            ],
        )
        .with_context(|| format!("adding user {name:?}"))?;
        let user = conn.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE id = ?1"),
            [conn.last_insert_rowid()],
            user_row,
        )?;
        Ok((user, token))
    }

    pub fn user_by_token(&self, token: &str) -> Result<Option<User>> {
        Ok(self
            .conn()
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE token_hash = ?1"), [hash_token(token)], user_row)
            .optional()?)
    }

    pub fn user_by_name(&self, name: &str) -> Result<Option<User>> {
        Ok(self
            .conn()
            .query_row(&format!("SELECT {USER_COLS} FROM users WHERE name = ?1"), [name], user_row)
            .optional()?)
    }

    pub fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        Ok(self.conn().query_row(&format!("SELECT {USER_COLS} FROM users WHERE id = ?1"), [id], user_row).optional()?)
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY name"))?;
        Ok(stmt.query_map([], user_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_quota(&self, name: &str, quota: Quota) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE users SET monthly_minutes = ?2, monthly_unlimited = ?3, max_concurrent = ?4, max_lease_minutes = ?5, idle_minutes = ?6 WHERE name = ?1",
            params![name, quota.monthly_minutes, quota.monthly_unlimited as i64, quota.max_concurrent, quota.max_lease_minutes, quota.idle_minutes],
        )?;
        if n == 0 {
            bail!("no user {name:?}");
        }
        Ok(())
    }

    /// Updates only the admin-editable limits (budget, unlimited flag, concurrency).
    pub fn set_admin_limits(
        &self,
        id: i64,
        monthly_minutes: i64,
        monthly_unlimited: bool,
        max_concurrent: i64,
    ) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE users SET monthly_minutes = ?2, monthly_unlimited = ?3, max_concurrent = ?4 WHERE id = ?1",
            params![id, monthly_minutes, monthly_unlimited as i64, max_concurrent],
        )?;
        Ok(n > 0)
    }

    pub fn set_disabled(&self, name: &str, disabled: bool) -> Result<()> {
        let n =
            self.conn().execute("UPDATE users SET disabled = ?2 WHERE name = ?1", params![name, disabled as i64])?;
        if n == 0 {
            bail!("no user {name:?}");
        }
        Ok(())
    }

    pub fn rotate_token(&self, name: &str) -> Result<String> {
        let token = new_token();
        let n = self
            .conn()
            .execute("UPDATE users SET token_hash = ?2 WHERE name = ?1", params![name, hash_token(&token)])?;
        if n == 0 {
            bail!("no user {name:?}");
        }
        Ok(token)
    }

    /// Deletes a user and its history; returns its id. Refuses while the user holds devices.
    pub fn remove_user(&self, name: &str) -> Result<i64> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let id: i64 = tx
            .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
            .optional()?
            .with_context(|| format!("no user {name:?}"))?;
        let active: i64 =
            tx.query_row("SELECT COUNT(*) FROM leases WHERE user_id = ?1 AND status != 'ended'", [id], |r| r.get(0))?;
        if active > 0 {
            bail!("{name:?} holds {active} device(s); disable the user and wait for them to be released");
        }
        tx.execute("DELETE FROM apps WHERE user_id = ?1", [id])?;
        tx.execute("DELETE FROM runs WHERE user_id = ?1", [id])?;
        tx.execute("DELETE FROM leases WHERE user_id = ?1", [id])?;
        tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(id)
    }

    // ---- usage ----

    pub fn used_seconds(&self, user_id: i64, from: i64, to: i64, now: i64) -> Result<i64> {
        Ok(self.conn().query_row(USAGE_SQL, params![from, to, now, user_id], |r| r.get(0))?)
    }

    pub fn usage(&self, from: i64, to: i64, now: i64) -> Result<Vec<UsageRow>> {
        let users = self.users()?;
        let conn = self.conn();
        let mut stmt = conn.prepare(USAGE_SQL)?;
        users
            .into_iter()
            .map(|user| {
                let (used_seconds, leases) =
                    stmt.query_row(params![from, to, now, user.id], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(UsageRow { user, used_seconds, leases })
            })
            .collect()
    }

    // ---- leases ----

    pub fn insert_lease(&self, lease: &Lease) -> Result<()> {
        self.conn().execute(
            "INSERT INTO leases (id, user_id, platform, model, device_id, driver_port, console_port, status, started_at, last_activity_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                lease.id,
                lease.user_id,
                lease.platform.as_str(),
                lease.model,
                lease.device_id,
                lease.driver_port as i64,
                lease.console_port.map(|p| p as i64),
                lease.status.as_str(),
                lease.started_at,
                lease.last_activity_at
            ],
        )?;
        Ok(())
    }

    /// The idle clock starts when the device is usable, not at admission.
    pub fn mark_ready(&self, id: &str, device_id: &str, at: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE leases SET status = 'ready', device_id = ?2, last_activity_at = ?3 WHERE id = ?1 AND status = 'booting'",
            params![id, device_id, at],
        )?;
        Ok(())
    }

    pub fn set_device_id(&self, id: &str, device_id: &str) -> Result<()> {
        self.conn().execute("UPDATE leases SET device_id = ?2 WHERE id = ?1", params![id, device_id])?;
        Ok(())
    }

    pub fn touch_lease(&self, id: &str, at: i64) -> Result<()> {
        self.conn()
            .execute("UPDATE leases SET last_activity_at = ?2 WHERE id = ?1 AND status != 'ended'", params![id, at])?;
        Ok(())
    }

    /// Ends a lease. Returns false if it had already ended. A lease that never booted is not billed.
    pub fn end_lease(&self, id: &str, reason: &str, at: i64, billable: bool) -> Result<bool> {
        let n = self.conn().execute(
            "UPDATE leases SET status = 'ended', ended_at = ?2, end_reason = ?3, billable = ?4 WHERE id = ?1 AND status != 'ended'",
            params![id, at, reason, billable as i64],
        )?;
        Ok(n > 0)
    }

    pub fn lease(&self, id: &str) -> Result<Option<Lease>> {
        Ok(self
            .conn()
            .query_row(&format!("SELECT {LEASE_COLS} FROM leases WHERE id = ?1"), [id], lease_row)
            .optional()?)
    }

    pub fn active_leases(&self) -> Result<Vec<Lease>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare(&format!("SELECT {LEASE_COLS} FROM leases WHERE status != 'ended' ORDER BY started_at"))?;
        Ok(stmt.query_map([], lease_row)?.collect::<rusqlite::Result<_>>()?)
    }

    // ---- apps ----

    pub fn count_apps(&self, user_id: i64) -> Result<i64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM apps WHERE user_id = ?1", [user_id], |r| r.get(0))?)
    }

    pub fn insert_app(&self, app: &App) -> Result<()> {
        self.conn().execute(
            "INSERT INTO apps (id, user_id, filename, path, size, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![app.id, app.user_id, app.filename, app.path, app.size, now()],
        )?;
        Ok(())
    }

    pub fn app(&self, id: &str, user_id: i64) -> Result<Option<App>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT id, user_id, filename, path, size FROM apps WHERE id = ?1 AND user_id = ?2",
                params![id, user_id],
                |r| {
                    Ok(App { id: r.get(0)?, user_id: r.get(1)?, filename: r.get(2)?, path: r.get(3)?, size: r.get(4)? })
                },
            )
            .optional()?)
    }

    /// Removes and returns the user's app `id`; `None` when the user has no such app.
    pub fn take_app(&self, id: &str, user_id: i64) -> Result<Option<App>> {
        Ok(self
            .conn()
            .query_row(
                "DELETE FROM apps WHERE id = ?1 AND user_id = ?2 RETURNING id, user_id, filename, path, size",
                params![id, user_id],
                |r| {
                    Ok(App { id: r.get(0)?, user_id: r.get(1)?, filename: r.get(2)?, path: r.get(3)?, size: r.get(4)? })
                },
            )
            .optional()?)
    }

    /// Removes and returns the apps that arrived before `before`.
    pub fn take_apps_before(&self, before: i64) -> Result<Vec<App>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("DELETE FROM apps WHERE created_at < ?1 RETURNING id, user_id, filename, path, size")?;
        let apps = stmt
            .query_map([before], |r| {
                Ok(App { id: r.get(0)?, user_id: r.get(1)?, filename: r.get(2)?, path: r.get(3)?, size: r.get(4)? })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(apps)
    }

    // ---- runs ----

    pub fn insert_run(&self, run: &RunRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO runs (id, user_id, lease_id, kind, name, tags, status, started_at, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                run.id,
                run.user_id,
                run.lease_id,
                run.kind,
                run.name,
                serde_json::to_string(&run.tags)?,
                run.status,
                run.started_at,
                run.summary
            ],
        )?;
        Ok(())
    }

    pub fn finish_run(&self, id: &str, status: &str, ended_at: i64, bytes: i64, summary: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE runs SET status = ?2, ended_at = ?3, bytes = ?4, summary = ?5 WHERE id = ?1",
            params![id, status, ended_at, bytes, summary],
        )?;
        Ok(())
    }

    pub fn run(&self, id: &str, user_id: i64) -> Result<Option<RunRow>> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1 AND user_id = ?2"),
                params![id, user_id],
                run_row,
            )
            .optional()?)
    }

    /// Newest first. `tag` matches one element of the run's tags.
    pub fn runs(&self, user_id: i64, status: Option<&str>, tag: Option<&str>, limit: i64) -> Result<Vec<RunRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RUN_COLS} FROM runs WHERE user_id = ?1
               AND (?2 IS NULL OR status = ?2)
               AND (?3 IS NULL OR EXISTS (SELECT 1 FROM json_each(runs.tags) WHERE json_each.value = ?3))
             ORDER BY started_at DESC LIMIT ?4"
        ))?;
        Ok(stmt.query_map(params![user_id, status, tag, limit], run_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn evidence_bytes(&self, user_id: i64) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM runs WHERE user_id = ?1", [user_id], |r| r.get(0))?)
    }

    /// Removes and returns the runs that ended before `before`.
    pub fn take_runs_before(&self, before: i64) -> Result<Vec<RunRow>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(&format!("DELETE FROM runs WHERE ended_at IS NOT NULL AND ended_at < ?1 RETURNING {RUN_COLS}"))?;
        Ok(stmt.query_map([before], run_row)?.collect::<rusqlite::Result<_>>()?)
    }

    /// After a restart no run is still executing: marks them `error` and returns them.
    pub fn fail_running_runs(&self, at: i64) -> Result<Vec<RunRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "UPDATE runs SET status = 'error', ended_at = ?1, summary = 'server restarted during the run'
             WHERE status = 'running' RETURNING {RUN_COLS}"
        ))?;
        Ok(stmt.query_map([at], run_row)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_run_bytes(&self, id: &str, bytes: i64) -> Result<()> {
        self.conn().execute("UPDATE runs SET bytes = ?2 WHERE id = ?1", params![id, bytes])?;
        Ok(())
    }
}

/// A recorded run. Its files live in `<dataDir>/runs/<user id>/<run id>/`.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub user_id: i64,
    pub lease_id: String,
    /// `actions`, `flow` or `xctest`.
    pub kind: String,
    pub name: Option<String>,
    pub tags: Vec<String>,
    /// `running`, `passed`, `failed`, `error` or `cancelled`.
    pub status: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub bytes: i64,
    /// One-line verdict.
    pub summary: Option<String>,
}

const RUN_COLS: &str = "id, user_id, lease_id, kind, name, tags, status, started_at, ended_at, bytes, summary";

fn run_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRow> {
    let tags: String = r.get(5)?;
    Ok(RunRow {
        id: r.get(0)?,
        user_id: r.get(1)?,
        lease_id: r.get(2)?,
        kind: r.get(3)?,
        name: r.get(4)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        status: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        bytes: r.get(9)?,
        summary: r.get(10)?,
    })
}
