//! Leases: admission against quotas and host capacity, device lifecycle, and the reaper that
//! enforces idle timeouts, lease length and monthly budgets.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, IsolationMode, Platform};
use crate::devices::{DeviceCtx, Devices, InstalledApp};
use crate::exec::{Account, Runner};
use crate::maestro::Maestro;
use crate::store::{App, Lease, LeaseStatus, Store, User, month_window, now};

pub const REAP_INTERVAL: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) struct LeaseRuntime {
    /// Serializes device operations of one lease (two Maestro processes on one device fight).
    pub(crate) op: Arc<Mutex<()>>,
    /// Cancelled on release: aborts boot, running flows and screen streams.
    pub(crate) cancel: CancellationToken,
    /// Set by `release` before it cancels, so a boot it interrupts ends with this reason.
    end_reason: StdMutex<Option<String>>,
    /// The app installed last on this lease: default target for launches, logs and crash reports.
    pub(crate) app: StdMutex<Option<InstalledApp>>,
    /// The screen's size in points, read once from the accessibility tree for viewer input.
    pub(crate) screen: StdMutex<Option<(f64, f64)>>,
}

pub struct Pool {
    pub config: Arc<Config>,
    pub store: Arc<Store>,
    pub(crate) devices: Arc<dyn Devices>,
    pub(crate) runner: Arc<Runner>,
    pub(crate) maestro: Maestro,
    admission: Mutex<()>,
    runtime: StdMutex<HashMap<String, Arc<LeaseRuntime>>>,
    /// Evidence bytes promised to runs in progress, per user, so parallel runs share the quota.
    pub(crate) evidence_reserved: StdMutex<HashMap<i64, u64>>,
    /// Live screen viewer tickets, sessions and streams.
    pub(crate) viewers: crate::viewer::Viewers,
    /// Open `seiji forward` tunnels per lease.
    pub(crate) forwards: crate::forward::Forwards,
}

#[derive(Debug, Serialize)]
pub struct LeaseView {
    pub lease_id: String,
    pub platform: Platform,
    pub model: String,
    pub device_id: Option<String>,
    pub status: &'static str,
    pub started_at: i64,
    /// The lease is released at this time at the latest (max lease length).
    pub ends_at_latest: i64,
    pub idle_release_minutes: i64,
}

/// An active lease and its owner, for the admin endpoints.
#[derive(Debug, Serialize)]
pub struct AdminLeaseView {
    pub user_id: i64,
    pub user: String,
    #[serde(flatten)]
    pub lease: LeaseView,
}

#[derive(Debug, Serialize)]
pub struct BudgetView {
    pub user: String,
    pub month: String,
    pub used_minutes: f64,
    pub monthly_minutes: i64,
    pub monthly_unlimited: bool,
    /// `None` (JSON null) when unlimited; finite remaining minutes otherwise.
    pub remaining_minutes: Option<f64>,
    pub max_concurrent: i64,
    pub max_lease_minutes: i64,
    pub idle_minutes: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_cost_usd: Option<f64>,
    pub active_leases: Vec<LeaseView>,
}

#[derive(Debug, Serialize)]
pub struct ModelsView {
    pub platform: Platform,
    pub models: Vec<String>,
    pub default_model: Option<String>,
    pub capacity: i64,
    pub in_use: i64,
}

pub enum AppSource {
    Uploaded(String),
    Url(String),
}

/// Outcome of an admin limits update.
pub enum AdminLimitsOutcome {
    /// Updated successfully; the refreshed user is returned.
    Ok(User),
    /// The caller's token was disabled or deleted while queued.
    CallerGone,
    /// The caller is not in `adminUsers`.
    Forbidden,
    /// The target user does not exist.
    NotFound,
}

/// `true` when `used_seconds` exhausts a finite budget of `monthly_minutes`.
/// Overflow-safe: compares seconds against minutes without multiplying.
fn budget_exhausted(monthly_minutes: i64, used_seconds: i64) -> bool {
    used_seconds >= monthly_minutes.saturating_mul(60)
}

fn status_str(s: LeaseStatus) -> &'static str {
    match s {
        LeaseStatus::Booting => "booting",
        LeaseStatus::Ready => "ready",
        LeaseStatus::Ended => "ended",
    }
}

pub fn month_label(ts: i64) -> String {
    let dt = time::OffsetDateTime::from_unix_timestamp(ts).expect("valid timestamp");
    format!("{:04}-{:02}", dt.year(), dt.month() as u8)
}

fn free_tcp_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// The lowest even console port in `android.consolePortRange` whose port and adb port (`+1`)
/// are free and not held by an active lease.
pub fn free_console_port(config: &Config, used: &HashSet<u16>) -> Result<u16> {
    let (lo, hi) = config.android.as_ref().context("android is not configured")?.console_port_range;
    (lo..=hi)
        .step_by(2)
        .find(|p| !used.contains(p) && !used.contains(&(p + 1)) && port_free(*p) && port_free(p + 1))
        .context("no free emulator console port in android.consolePortRange")
}

/// Where and as whom a user's commands run.
pub fn account_for(config: &Config, user: &User) -> Result<Account> {
    match config.isolation.mode {
        IsolationMode::OsUser => {
            let os_user = user.os_user.clone().with_context(|| format!("user {:?} has no macOS account", user.name))?;
            let account = crate::tenant::lookup(&os_user)?
                .with_context(|| format!("macOS account {os_user:?} is missing; run `sudo seiji tenant provision`"))?;
            Ok(Account { name: user.name.clone(), os_user: Some(os_user), workspace: account.home.join("seiji") })
        }
        IsolationMode::Shared => Ok(Account {
            name: user.name.clone(),
            os_user: None,
            workspace: config.data_dir.join("tenants").join(&user.name),
        }),
    }
}

impl Pool {
    pub fn new(config: Arc<Config>, store: Arc<Store>, runner: Arc<Runner>, devices: Arc<dyn Devices>) -> Self {
        let maestro = Maestro::new(config.clone(), runner.clone());
        Self {
            config,
            store,
            devices,
            runner,
            maestro,
            admission: Mutex::new(()),
            runtime: StdMutex::new(HashMap::new()),
            evidence_reserved: StdMutex::new(HashMap::new()),
            viewers: Default::default(),
            forwards: Default::default(),
        }
    }

    pub fn account(&self, user: &User) -> Result<Account> {
        account_for(&self.config, user)
    }

    pub(crate) fn ctx(&self, lease: &Lease, account: Account) -> DeviceCtx {
        DeviceCtx {
            lease_id: lease.id.clone(),
            platform: lease.platform,
            model: lease.model.clone(),
            device_id: lease.device_id.clone(),
            console_port: lease.console_port,
            account,
        }
    }

    fn view(&self, lease: &Lease, user: &User) -> LeaseView {
        LeaseView {
            lease_id: lease.id.clone(),
            platform: lease.platform,
            model: lease.model.clone(),
            device_id: lease.device_id.clone(),
            status: status_str(lease.status),
            started_at: lease.started_at,
            ends_at_latest: lease.started_at + user.quota.max_lease_minutes * 60,
            idle_release_minutes: user.quota.idle_minutes,
        }
    }

    pub(crate) fn fresh_user(&self, user: &User) -> Result<User> {
        let user = self.store.user_by_id(user.id)?.context("user no longer exists")?;
        if user.disabled {
            bail!("user {:?} is disabled", user.name);
        }
        Ok(user)
    }

    pub fn budget(&self, user: &User) -> Result<BudgetView> {
        let user = self.store.user_by_id(user.id)?.context("user no longer exists")?;
        let t = now();
        let (from, to) = month_window(t);
        let used = self.store.used_seconds(user.id, from, to, t)? as f64 / 60.0;
        let active = self
            .store
            .active_leases()?
            .into_iter()
            .filter(|l| l.user_id == user.id)
            .map(|l| self.view(&l, &user))
            .collect();
        Ok(BudgetView {
            user: user.name.clone(),
            month: month_label(t),
            used_minutes: (used * 10.0).round() / 10.0,
            monthly_minutes: user.quota.monthly_minutes,
            monthly_unlimited: user.quota.monthly_unlimited,
            remaining_minutes: if user.quota.monthly_unlimited {
                None
            } else {
                Some(((user.quota.monthly_minutes as f64 - used).max(0.0) * 10.0).round() / 10.0)
            },
            max_concurrent: user.quota.max_concurrent,
            max_lease_minutes: user.quota.max_lease_minutes,
            idle_minutes: user.quota.idle_minutes,
            estimated_cost_usd: self.config.cost_per_device_hour_usd.map(|r| (used / 60.0 * r * 100.0).round() / 100.0),
            active_leases: active,
        })
    }

    pub fn models(&self) -> Result<Vec<ModelsView>> {
        let active = self.store.active_leases()?;
        Ok([Platform::Ios, Platform::Android]
            .into_iter()
            .filter(|p| self.config.capacity(*p) > 0)
            .map(|p| ModelsView {
                platform: p,
                models: self.config.models(p),
                default_model: self.config.default_model(p).map(String::from),
                capacity: self.config.capacity(p),
                in_use: active.iter().filter(|l| l.platform == p).count() as i64,
            })
            .collect())
    }

    /// Updates admin-editable limits under the admission lock, re-checking the caller's identity
    /// and admin authority at execution time.
    pub async fn set_admin_limits(
        &self,
        caller_id: i64,
        target_id: i64,
        monthly_minutes: i64,
        monthly_unlimited: bool,
        max_concurrent: i64,
    ) -> Result<AdminLimitsOutcome> {
        let _guard = self.admission.lock().await;
        // Re-check caller under the lock: they may have been disabled/deleted while queued.
        let caller = match self.store.user_by_id(caller_id)? {
            Some(u) if !u.disabled => u,
            _ => return Ok(AdminLimitsOutcome::CallerGone),
        };
        if !self.config.admin_users.contains(&caller.name) {
            return Ok(AdminLimitsOutcome::Forbidden);
        }
        if !self.store.set_admin_limits(target_id, monthly_minutes, monthly_unlimited, max_concurrent)? {
            return Ok(AdminLimitsOutcome::NotFound);
        }
        Ok(AdminLimitsOutcome::Ok(self.store.user_by_id(target_id)?.context("user vanished after update")?))
    }

    /// Admits, creates and boots a device. Admission is atomic; boot happens outside the lock.
    pub async fn acquire(&self, user: &User, platform: Platform, model: Option<String>) -> Result<LeaseView> {
        let model = match model {
            Some(m) => m,
            None => self
                .config
                .default_model(platform)
                .with_context(|| format!("{} is not available on this host", platform.as_str()))?
                .to_string(),
        };
        if !self.config.models(platform).contains(&model) {
            bail!("unknown {} model {model:?}; available: {:?}", platform.as_str(), self.config.models(platform));
        }

        let (lease, user, rt, account, op) = {
            let _guard = self.admission.lock().await;
            let user = self.fresh_user(user)?;
            let t = now();
            let (from, to) = month_window(t);
            let used = self.store.used_seconds(user.id, from, to, t)?;
            if !user.quota.monthly_unlimited && budget_exhausted(user.quota.monthly_minutes, used) {
                bail!(
                    "monthly budget exhausted: {:.1} of {} device-minutes used in {}",
                    used as f64 / 60.0,
                    user.quota.monthly_minutes,
                    month_label(t)
                );
            }
            let active = self.store.active_leases()?;
            let mine = active.iter().filter(|l| l.user_id == user.id).count() as i64;
            if mine >= user.quota.max_concurrent {
                bail!(
                    "you already hold {mine} device(s), your limit is {}; release one first",
                    user.quota.max_concurrent
                );
            }
            let capacity = self.config.capacity(platform);
            let in_use = active.iter().filter(|l| l.platform == platform).count() as i64;
            if in_use >= capacity {
                bail!("host is full: {in_use}/{capacity} {} devices in use; try again later", platform.as_str());
            }
            let used_ports: HashSet<u16> = active
                .iter()
                .flat_map(|l| [Some(l.driver_port), l.console_port, l.console_port.map(|p| p + 1)])
                .flatten()
                .collect();
            let driver_port = loop {
                let p = free_tcp_port()?;
                if !used_ports.contains(&p) {
                    break p;
                }
            };
            let console_port = match platform {
                Platform::Ios => None,
                Platform::Android => Some(free_console_port(&self.config, &used_ports)?),
            };
            let lease = Lease {
                id: format!("l_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
                user_id: user.id,
                platform,
                model,
                device_id: None,
                driver_port,
                console_port,
                status: LeaseStatus::Booting,
                started_at: t,
                last_activity_at: t,
                ended_at: None,
                end_reason: None,
            };
            // Resolve the account before the lease exists, so a missing account leaves no lease.
            let account = self.account(&user)?;
            self.store.insert_lease(&lease)?;
            let rt = Arc::new(LeaseRuntime {
                op: Arc::new(Mutex::new(())),
                cancel: CancellationToken::new(),
                end_reason: StdMutex::new(None),
                app: StdMutex::new(None),
                screen: StdMutex::new(None),
            });
            // Hold the op lock before the lease is visible to `release`: a release that arrives
            // during boot then waits for the boot to finish or fail, and never misses a device.
            let op = rt.op.clone().try_lock_owned().expect("fresh op lock");
            self.runtime.lock().expect("runtime mutex").insert(lease.id.clone(), rt.clone());
            (lease, user, rt, account, op)
        };

        let mut ctx = self.ctx(&lease, account);
        let _op = op;
        let booted: Result<String> = async {
            let id = self.devices.create(&ctx).await?;
            self.store.set_device_id(&lease.id, &id)?;
            ctx.device_id = Some(id.clone());
            self.devices.boot(&ctx, &rt.cancel).await?;
            Ok(id)
        }
        .await;
        match booted {
            Ok(id) if !rt.cancel.is_cancelled() => {
                self.store.mark_ready(&lease.id, &id, now())?;
                let lease = self.store.lease(&lease.id)?.context("lease vanished")?;
                tracing::info!(user = %user.name, lease = %lease.id, device = %id, "lease ready");
                Ok(self.view(&lease, &user))
            }
            result => {
                self.runtime.lock().expect("runtime mutex").remove(&lease.id);
                let (reason, billable, err) = match rt.end_reason.lock().expect("reason mutex").take() {
                    // Released while booting: the releaser's reason stands and the time is billed,
                    // so acquiring and releasing during boot is not free.
                    Some(reason) => (reason, true, anyhow!("the lease was released while the device booted")),
                    None => (
                        "boot_failed".to_string(),
                        false,
                        result
                            .err()
                            .unwrap_or_else(|| anyhow!("boot cancelled"))
                            .context("device failed to start (not billed)"),
                    ),
                };
                if self.store.end_lease(&lease.id, &reason, now(), billable)? {
                    self.devices.destroy(&ctx).await;
                }
                tracing::warn!(user = %user.name, lease = %lease.id, reason, "boot did not complete: {err:#}");
                Err(err)
            }
        }
    }

    /// Ends a lease and deletes its device. Returns false if it was already ended.
    pub async fn release(&self, lease_id: &str, reason: &str) -> Result<bool> {
        let rt = self.runtime.lock().expect("runtime mutex").remove(lease_id);
        let _op = match &rt {
            Some(rt) => {
                *rt.end_reason.lock().expect("reason mutex") = Some(reason.to_string());
                rt.cancel.cancel();
                Some(rt.op.lock().await)
            }
            None => None,
        };
        let Some(lease) = self.store.lease(lease_id)? else { return Ok(false) };
        if !self.store.end_lease(lease_id, reason, now(), true)? {
            // With a runtime entry, `acquire` was still booting: it ended the lease with this
            // reason and destroyed the device once the boot stopped.
            return Ok(rt.is_some());
        }
        let user = self.store.user_by_id(lease.user_id)?.context("lease owner missing")?;
        let ctx = self.ctx(&lease, self.account(&user)?);
        self.devices.destroy(&ctx).await;
        tracing::info!(user = %user.name, lease = %lease_id, reason, "lease released");
        Ok(true)
    }

    pub async fn release_for(&self, user: &User, lease_id: Option<&str>) -> Result<String> {
        let lease = self.resolve(user, lease_id, true)?;
        if !self.release(&lease.id, "released").await? {
            bail!("lease {} was already released", lease.id);
        }
        Ok(lease.id)
    }

    /// The caller's lease: the given id (must be theirs), or their only active lease.
    fn resolve(&self, user: &User, lease_id: Option<&str>, allow_booting: bool) -> Result<Lease> {
        let lease = match lease_id {
            Some(id) => self
                .store
                .lease(id)?
                .filter(|l| l.user_id == user.id && l.status != LeaseStatus::Ended)
                .with_context(|| format!("no active lease {id:?} for you"))?,
            None => {
                let mut mine: Vec<Lease> =
                    self.store.active_leases()?.into_iter().filter(|l| l.user_id == user.id).collect();
                match mine.len() {
                    0 => bail!("you hold no device; call acquire_device first"),
                    1 => mine.remove(0),
                    _ => bail!("you hold {} devices; pass lease_id", mine.len()),
                }
            }
        };
        if !allow_booting && lease.status != LeaseStatus::Ready {
            bail!("lease {} is still booting", lease.id);
        }
        Ok(lease)
    }

    /// Runs `f` on the caller's ready lease under its op lock, recording activity before and after.
    /// Fails if the lease is released meanwhile.
    pub(crate) async fn with_lease<T, F, Fut>(&self, user: &User, lease_id: Option<&str>, f: F) -> Result<T>
    where
        F: FnOnce(DeviceCtx, Lease, CancellationToken, Arc<LeaseRuntime>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.with_lease_inner(user, lease_id, true, f).await
    }

    /// Like `with_lease`, but returns `f`'s result even when the lease is released while it runs
    /// (a run records its own cancellation).
    pub(crate) async fn with_lease_keep<T, F, Fut>(&self, user: &User, lease_id: Option<&str>, f: F) -> Result<T>
    where
        F: FnOnce(DeviceCtx, Lease, CancellationToken, Arc<LeaseRuntime>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.with_lease_inner(user, lease_id, false, f).await
    }

    async fn with_lease_inner<T, F, Fut>(
        &self,
        user: &User,
        lease_id: Option<&str>,
        fail_if_released: bool,
        f: F,
    ) -> Result<T>
    where
        F: FnOnce(DeviceCtx, Lease, CancellationToken, Arc<LeaseRuntime>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let user = self.fresh_user(user)?;
        let lease = self.resolve(&user, lease_id, false)?;
        let rt =
            self.runtime.lock().expect("runtime mutex").get(&lease.id).cloned().context("lease is being released")?;
        let _op = rt.op.lock().await;
        if rt.cancel.is_cancelled() {
            bail!("lease {} was released", lease.id);
        }
        self.store.touch_lease(&lease.id, now())?;
        let ctx = self.ctx(&lease, self.account(&user)?);
        let result = f(ctx, lease.clone(), rt.cancel.clone(), rt.clone()).await;
        if fail_if_released && rt.cancel.is_cancelled() {
            let ended = self.store.lease(&lease.id)?.and_then(|l| l.end_reason).unwrap_or_default();
            bail!("lease {} was released during the operation ({ended})", lease.id);
        }
        self.store.touch_lease(&lease.id, now())?;
        result
    }

    /// The caller's lease, checked for ownership and readiness, without taking its op lock.
    pub(crate) fn peek_lease(&self, user: &User, lease_id: Option<&str>) -> Result<Lease> {
        self.resolve(&self.fresh_user(user)?, lease_id, false)
    }

    /// Every active lease with its owner, for an admin. The caller checks `adminUsers`.
    pub fn admin_leases(&self) -> Result<Vec<AdminLeaseView>> {
        let users: HashMap<i64, User> = self.store.users()?.into_iter().map(|u| (u.id, u)).collect();
        Ok(self
            .store
            .active_leases()?
            .into_iter()
            .filter_map(|l| {
                let u = users.get(&l.user_id)?;
                Some(AdminLeaseView { user_id: u.id, user: u.name.clone(), lease: self.view(&l, u) })
            })
            .collect())
    }

    /// Any user's ready lease and its enabled owner, for an admin. This is the one lookup that
    /// skips the owner check of `resolve`; its callers check `adminUsers` first.
    pub(crate) fn admin_lease(&self, lease_id: &str) -> Result<(User, Lease)> {
        let lease = self
            .store
            .lease(lease_id)?
            .filter(|l| l.status != LeaseStatus::Ended)
            .with_context(|| format!("no active lease {lease_id:?}"))?;
        if lease.status != LeaseStatus::Ready {
            bail!("lease {} is still booting", lease.id);
        }
        let owner = self
            .store
            .user_by_id(lease.user_id)?
            .filter(|u| !u.disabled)
            .with_context(|| format!("the user of lease {} is disabled or removed", lease.id))?;
        Ok((owner, lease))
    }

    pub(crate) fn runtime_of(&self, lease_id: &str) -> Option<Arc<LeaseRuntime>> {
        self.runtime.lock().expect("runtime mutex").get(lease_id).cloned()
    }

    pub async fn hierarchy(&self, user: &User, lease_id: Option<&str>) -> Result<String> {
        self.with_lease(user, lease_id, |ctx, lease, cancel, _| async move {
            match ctx.platform {
                Platform::Ios => Ok(crate::actions::render_tree(&self.devices.describe_ui(&ctx).await?)),
                Platform::Android => self.maestro.hierarchy(&ctx, lease.driver_port, &cancel).await,
            }
        })
        .await
    }

    pub async fn screenshot(&self, user: &User, lease_id: Option<&str>) -> Result<Vec<u8>> {
        self.with_lease(user, lease_id, |ctx, _, _, _| async move { self.devices.screenshot(&ctx).await }).await
    }

    pub async fn install(&self, user: &User, lease_id: Option<&str>, source: AppSource) -> Result<InstalledApp> {
        // Fail fast on a bad lease before a download that may take minutes.
        self.peek_lease(user, lease_id)?;
        let app = match source {
            AppSource::Uploaded(id) => {
                self.store.app(&id, user.id)?.with_context(|| format!("no uploaded app {id:?}"))?
            }
            AppSource::Url(url) => self.download_app(user, &url).await?,
        };
        let path = PathBuf::from(&app.path);
        self.with_lease(user, lease_id, |ctx, _, _, rt| async move {
            let installed = self.devices.install(&ctx, &path, &app.filename).await?;
            *rt.app.lock().expect("app mutex") = Some(installed.clone());
            Ok(installed)
        })
        .await
    }

    /// Refuses a new build when the user already keeps `maxBuildsPerUser`.
    pub fn check_build_room(&self, user: &User) -> Result<()> {
        let held = self.store.count_apps(user.id)?;
        if held >= self.config.max_builds_per_user {
            bail!(
                "you already keep {held} builds, the limit is {}; they expire {}h after upload",
                self.config.max_builds_per_user,
                self.config.app_retention_hours
            );
        }
        Ok(())
    }

    pub fn apps_dir(&self, user_id: i64, app_id: &str) -> PathBuf {
        self.config.data_dir.join("apps").join(user_id.to_string()).join(app_id)
    }

    async fn download_app(&self, user: &User, url: &str) -> Result<App> {
        self.check_build_room(user)?;
        let parsed = reqwest::Url::parse(url).context("invalid url")?;
        if parsed.scheme() != "https" {
            bail!("only https URLs are accepted");
        }
        let filename = parsed
            .path_segments()
            .and_then(|mut s| s.next_back())
            .filter(|s| !s.is_empty())
            .map(sanitize_filename)
            .context("url has no file name")?;
        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.url().scheme() != "https" {
                    attempt.error("redirect to a non-https URL")
                } else if attempt.previous().len() >= 5 {
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }))
            .build()?;
        let id = format!("a_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let dir = self.apps_dir(user.id, &id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(&filename);
        let fetched: Result<u64> = async {
            let mut resp = client.get(parsed).send().await?.error_for_status()?;
            let mut file = tokio::fs::File::create(&path).await?;
            let mut size: u64 = 0;
            while let Some(chunk) = resp.chunk().await? {
                size += chunk.len() as u64;
                if size > self.config.max_upload_bytes {
                    bail!("download exceeds {} bytes", self.config.max_upload_bytes);
                }
                tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            }
            tokio::io::AsyncWriteExt::flush(&mut file).await?;
            Ok(size)
        }
        .await;
        let size = match fetched {
            Ok(size) => size,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let app = App { id, user_id: user.id, filename, path: path.to_string_lossy().into_owned(), size: size as i64 };
        self.store.insert_app(&app)?;
        Ok(app)
    }

    /// One reaper pass: release leases that went idle, ran too long, or whose owner ran out of budget,
    /// and delete builds older than `appRetentionHours` and runs older than `evidence.retentionHours`.
    pub async fn reap(&self) -> Result<()> {
        let t = now();
        self.viewers.prune(t);
        for app in self.store.take_apps_before(t - self.config.app_retention_hours * 3600)? {
            if let Err(e) = std::fs::remove_dir_all(self.apps_dir(app.user_id, &app.id)) {
                tracing::warn!(app = %app.id, "deleting expired build: {e}");
            }
        }
        for run in self.store.take_runs_before(t - self.config.evidence.retention_hours * 3600)? {
            let dir = crate::evidence::run_dir(&self.config, run.user_id, &run.id);
            if let Err(e) = std::fs::remove_dir_all(&dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(run = %run.id, "deleting expired evidence: {e}");
            }
        }
        let (from, to) = month_window(t);
        for lease in self.store.active_leases()? {
            let Some(user) = self.store.user_by_id(lease.user_id)? else { continue };
            let busy =
                self.runtime.lock().expect("runtime mutex").get(&lease.id).is_some_and(|rt| rt.op.try_lock().is_err());
            let used = self.store.used_seconds(user.id, from, to, t)?;
            let reason = if user.disabled {
                Some("user_disabled")
            } else if !user.quota.monthly_unlimited && budget_exhausted(user.quota.monthly_minutes, used) {
                Some("budget_exhausted")
            } else if t - lease.started_at >= user.quota.max_lease_minutes * 60 {
                Some("max_lease_duration")
            } else if lease.status == LeaseStatus::Ready
                && !busy
                && t - lease.last_activity_at >= user.quota.idle_minutes * 60
            {
                Some("idle")
            } else {
                None
            };
            if let Some(reason) = reason
                && let Err(e) = self.release(&lease.id, reason).await
            {
                tracing::warn!(lease = %lease.id, "release ({reason}) failed: {e:#}");
            }
        }
        Ok(())
    }

    /// After a restart no device process survives as ours: end recorded leases, then sweep
    /// simulators nobody owns.
    pub async fn recover(&self) -> Result<()> {
        for run in self.store.fail_running_runs(now())? {
            tracing::warn!(run = %run.id, "run interrupted by the restart is marked as an error");
            let verdict = "server restarted during the run";
            let warning = "the server restarted during the run; only files written before that are kept";
            if let Err(e) = self.close_run_with_error(&run, verdict, warning) {
                tracing::warn!(run = %run.id, "writing its manifest: {e:#}");
            }
        }
        for lease in self.store.active_leases()? {
            // The crash time is unknown; bill up to the last recorded activity, not the downtime.
            self.store.end_lease(&lease.id, "server_restart", lease.last_activity_at.max(lease.started_at), true)?;
            if let Some(user) = self.store.user_by_id(lease.user_id)?
                && let Ok(account) = self.account(&user)
            {
                self.devices.destroy(&self.ctx(&lease, account)).await;
            }
        }
        let mut seen = HashSet::new();
        for user in self.store.users()? {
            let Ok(account) = self.account(&user) else { continue };
            if seen.insert(account.os_user.clone()) {
                self.devices.cleanup_orphans(&account, &[]).await;
            }
        }
        Ok(())
    }

    pub fn spawn_reaper(self: &Arc<Self>, shutdown: CancellationToken) {
        let pool = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(REAP_INTERVAL);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Err(e) = pool.reap().await {
                            tracing::warn!("reaper: {e:#}");
                        }
                    }
                    _ = shutdown.cancelled() => return,
                }
            }
        });
    }

    /// Releases every lease at once (graceful shutdown): cancels running flows and deletes devices.
    pub async fn release_all(&self, reason: &str) {
        let ids: Vec<String> =
            self.store.active_leases().map(|ls| ls.into_iter().map(|l| l.id).collect()).unwrap_or_default();
        futures_util::future::join_all(ids.iter().map(|id| async move {
            if let Err(e) = self.release(id, reason).await {
                tracing::warn!(lease = %id, "release on shutdown failed: {e:#}");
            }
        }))
        .await;
    }
}

pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect();
    let trimmed = cleaned.trim_start_matches('.');
    if trimmed.is_empty() { "app".to_string() } else { trimmed.to_string() }
}
