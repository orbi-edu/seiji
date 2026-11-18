use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tokio_util::sync::CancellationToken;

use seiji::config::{Config, IsolationMode, Platform, Quota};
use seiji::devices::{Capture, DeviceCtx, Devices, HostDevices};
use seiji::exec::{CmdSpec, Runner};
use seiji::pool::{self, Pool, account_for, free_console_port};
use seiji::store::{Store, month_window, now, parse_month};
use seiji::{http, tenant};

#[derive(Parser)]
#[command(
    name = "seiji",
    version,
    about = "Budgeted iOS simulator / Android emulator pool for AI agents (MCP + Maestro)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args)]
struct ConfigArg {
    /// Path to seiji.json. Accepted before or after the subcommand.
    #[arg(long, short, global = true, env = "SEIJI_CONFIG", default_value = "/usr/local/etc/seiji/seiji.json")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server.
    Serve(ConfigArg),
    /// Manage users (seats, tokens, quotas).
    User {
        #[command(flatten)]
        config: ConfigArg,
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Device-minutes per user for a month.
    Usage {
        #[command(flatten)]
        config: ConfigArg,
        /// YYYY-MM (UTC); current month by default.
        #[arg(long)]
        month: Option<String>,
    },
    /// macOS tenant accounts (os_user isolation). Run with sudo.
    Tenant {
        #[command(subcommand)]
        cmd: TenantCmd,
    },
    /// Boot, inspect and delete a throwaway device as a user, end to end.
    Doctor {
        #[command(flatten)]
        config: ConfigArg,
        user: String,
        #[arg(long, value_parser = ["ios", "android"], default_value = "ios")]
        platform: String,
    },
    /// Client: sign in to a Seiji server (the token is read from stdin and kept in the keychain).
    Login {
        /// Server URL, e.g. https://seiji.example.com
        #[arg(long)]
        url: String,
    },
    /// Client: forget the stored token.
    Logout,
    /// Client: the stored credential.
    Token {
        #[command(subcommand)]
        cmd: TokenCmd,
    },
    /// Client: let your device reach a port on this machine (Metro, a dev server) until Ctrl-C.
    Forward(ForwardCli),
    /// [root helper] Run a command as a tenant account inside its launchd domain.
    #[command(hide = true)]
    ExecAs(ExecArgsCli),
    #[command(hide = true)]
    ExecDrop(ExecArgsCli),
    /// [root helper] Signal a tenant's process group.
    #[command(hide = true)]
    KillAs { user: String, pgid: i32, signal: i32 },
}

#[derive(Subcommand)]
enum TokenCmd {
    /// Print the token (`--json`: url, token and user, for Tern's plugin).
    Print {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct ForwardCli {
    /// The local port the device should reach (Metro's is 8081).
    #[arg(default_value_t = 8081)]
    port: u16,
    /// The lease; the only device you hold by default.
    #[arg(long)]
    lease: Option<String>,
    /// Then open the tunnel in Expo Go (`exp://…`).
    #[arg(long, group = "point")]
    expo_go: bool,
    /// Then open the tunnel in an Expo development build with this URL scheme.
    #[arg(long, value_name = "SCHEME", group = "point")]
    dev_client: Option<String>,
    /// Then point a React Native debug build at the tunnel (`RCT_jsLocation`) and relaunch it;
    /// the app installed last when no bundle id is given.
    #[arg(long, value_name = "BUNDLE_ID", num_args = 0..=1, default_missing_value = "", group = "point")]
    react_native: Option<String>,
}

#[derive(Args)]
struct ExecArgsCli {
    user: String,
    #[arg(long)]
    pidfile: Option<PathBuf>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    env: Vec<String>,
    #[arg(last = true, required = true)]
    command: Vec<OsString>,
}

impl From<ExecArgsCli> for tenant::ExecArgs {
    fn from(a: ExecArgsCli) -> Self {
        Self { user: a.user, pidfile: a.pidfile, cwd: a.cwd, env: a.env, command: a.command }
    }
}

#[derive(Args, Default)]
struct QuotaFlags {
    /// Device-minutes per calendar month.
    #[arg(long)]
    monthly_minutes: Option<i64>,
    /// Devices held at the same time.
    #[arg(long)]
    max_concurrent: Option<i64>,
    /// Longest single lease.
    #[arg(long)]
    max_lease_minutes: Option<i64>,
    /// Release after this long without a tool call.
    #[arg(long)]
    idle_minutes: Option<i64>,
}

impl QuotaFlags {
    fn apply(&self, base: Quota) -> Result<Quota> {
        let q = Quota {
            monthly_minutes: self.monthly_minutes.unwrap_or(base.monthly_minutes),
            max_concurrent: self.max_concurrent.unwrap_or(base.max_concurrent),
            max_lease_minutes: self.max_lease_minutes.unwrap_or(base.max_lease_minutes),
            idle_minutes: self.idle_minutes.unwrap_or(base.idle_minutes),
        };
        if q.monthly_minutes < 0 || q.max_concurrent < 1 || q.max_lease_minutes < 1 || q.idle_minutes < 1 {
            bail!("quotas must be positive (monthly-minutes may be 0)");
        }
        Ok(q)
    }
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add a user and print its token (shown once).
    Add {
        name: String,
        #[command(flatten)]
        quota: QuotaFlags,
    },
    List,
    /// Change a user's quota.
    Set {
        name: String,
        #[command(flatten)]
        quota: QuotaFlags,
    },
    /// Block new devices; the reaper releases the ones held.
    Disable {
        name: String,
    },
    Enable {
        name: String,
    },
    RotateToken {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
enum TenantCmd {
    /// Create the hidden macOS account `<prefix><name>` in the seiji-tenants group.
    Provision {
        #[command(flatten)]
        config: ConfigArg,
        name: String,
    },
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 32
        || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("user names must be 1-32 chars of [a-z0-9-]");
    }
    Ok(())
}

fn os_user_for(config: &Config, name: &str) -> String {
    format!("{}{name}", config.isolation.user_prefix)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if !matches!(cli.cmd, Cmd::ExecAs(_) | Cmd::ExecDrop(_) | Cmd::KillAs { .. }) {
        // The database, builds and logs are the daemon's alone; Runner opens `run/` explicitly.
        unsafe { libc::umask(0o077) };
    }
    match cli.cmd {
        // Root helpers: never read the (daemon-writable) config.
        Cmd::ExecAs(a) => tenant::exec_as(a.into()),
        Cmd::ExecDrop(a) => tenant::exec_drop(a.into()),
        Cmd::KillAs { user, pgid, signal } => tenant::kill_as(&user, pgid, signal),
        Cmd::Tenant { cmd: TenantCmd::Provision { config, name } } => {
            valid_name(&name)?;
            let config = Config::load(&config.config)?;
            let account = tenant::provision(&os_user_for(&config, &name))?;
            println!("macOS account {} ready (uid {}, home {})", account.name, account.uid, account.home.display());
            println!("next: seiji user add {name}");
            Ok(())
        }
        Cmd::User { config, cmd } => user_cmd(&Config::load(&config.config)?, cmd),
        Cmd::Usage { config, month } => usage(&Config::load(&config.config)?, month.as_deref()),
        Cmd::Serve(config) => runtime()?.block_on(serve(Config::load(&config.config)?)),
        Cmd::Doctor { config, user, platform } => {
            runtime()?.block_on(doctor(Config::load(&config.config)?, &user, Platform::parse(&platform)?))
        }
        Cmd::Login { url } => runtime()?.block_on(seiji::client::login(&url)),
        Cmd::Logout => seiji::client::logout(),
        Cmd::Token { cmd: TokenCmd::Print { json } } => seiji::client::token_print(json),
        Cmd::Forward(f) => {
            use seiji::client::Point;
            let point = if f.expo_go {
                Some(Point::ExpoGo)
            } else if let Some(scheme) = f.dev_client {
                Some(Point::DevClient(scheme))
            } else {
                f.react_native.map(|b| Point::ReactNative(Some(b).filter(|b| !b.is_empty())))
            };
            runtime()?.block_on(seiji::client::forward(f.port, f.lease, point))
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().enable_all().build()?)
}

fn user_cmd(config: &Config, cmd: UserCmd) -> Result<()> {
    let store = Store::open(&config.db_path())?;
    match cmd {
        UserCmd::Add { name, quota } => {
            valid_name(&name)?;
            let quota = quota.apply(config.default_quota)?;
            let os_user = match config.isolation.mode {
                IsolationMode::OsUser => {
                    let os_user = os_user_for(config, &name);
                    tenant::validate_tenant(&os_user)
                        .with_context(|| format!("run `sudo seiji tenant provision {name}` first"))?;
                    Some(os_user)
                }
                IsolationMode::Shared => None,
            };
            let (user, token) = store.add_user(&name, os_user.as_deref(), quota, config.seats)?;
            println!("user {} added", user.name);
            println!("token (shown once): {token}");
        }
        UserCmd::List => {
            println!(
                "{:<16} {:<20} {:>8} {:>5} {:>9} {:>6}  status",
                "name", "os account", "min/mo", "conc", "lease-min", "idle"
            );
            for u in store.users()? {
                println!(
                    "{:<16} {:<20} {:>8} {:>5} {:>9} {:>6}  {}",
                    u.name,
                    u.os_user.as_deref().unwrap_or("-"),
                    u.quota.monthly_minutes,
                    u.quota.max_concurrent,
                    u.quota.max_lease_minutes,
                    u.quota.idle_minutes,
                    if u.disabled { "disabled" } else { "active" }
                );
            }
            println!("seats: {}/{}", store.users()?.len(), config.seats);
        }
        UserCmd::Set { name, quota } => {
            let user = store.user_by_name(&name)?.with_context(|| format!("no user {name:?}"))?;
            store.set_quota(&name, quota.apply(user.quota)?)?;
            println!("updated {name}");
        }
        UserCmd::Disable { name } => {
            store.set_disabled(&name, true)?;
            println!("disabled {name}; held devices are released within {}s", pool::REAP_INTERVAL.as_secs());
        }
        UserCmd::Enable { name } => {
            store.set_disabled(&name, false)?;
            println!("enabled {name}");
        }
        UserCmd::RotateToken { name } => println!("new token for {name} (shown once): {}", store.rotate_token(&name)?),
        UserCmd::Remove { name } => {
            let id = store.remove_user(&name)?;
            for kind in ["apps", "runs"] {
                let dir = config.data_dir.join(kind).join(id.to_string());
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).with_context(|| format!("deleting {}", dir.display()))?;
                }
            }
            println!("removed {name} (its macOS account, if any, is left in place)");
        }
    }
    Ok(())
}

fn usage(config: &Config, month: Option<&str>) -> Result<()> {
    let store = Store::open(&config.db_path())?;
    let t = now();
    let (from, to) = match month {
        Some(m) => parse_month(m)?,
        None => month_window(t),
    };
    let rows = store.usage(from, to, t)?;
    println!("month {}", pool::month_label(from));
    println!("{:<16} {:>10} {:>8} {:>6} {:>7}  est. USD", "user", "used min", "quota", "used%", "leases");
    let (mut total_min, mut total_usd) = (0.0, 0.0);
    for r in &rows {
        let used = r.used_seconds as f64 / 60.0;
        let pct =
            if r.user.quota.monthly_minutes > 0 { used / r.user.quota.monthly_minutes as f64 * 100.0 } else { 0.0 };
        let usd = config.cost_per_device_hour_usd.map(|rate| used / 60.0 * rate);
        total_min += used;
        total_usd += usd.unwrap_or(0.0);
        println!(
            "{:<16} {:>10.1} {:>8} {:>5.0}% {:>7}  {}",
            r.user.name,
            used,
            r.user.quota.monthly_minutes,
            pct,
            r.leases,
            usd.map_or("-".into(), |u| format!("{u:.2}"))
        );
    }
    println!(
        "{:<16} {:>10.1} {:>8} {:>6} {:>7}  {}",
        "total",
        total_min,
        "",
        "",
        "",
        config.cost_per_device_hour_usd.map_or("-".into(), |_| format!("{total_usd:.2}"))
    );
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "seiji=info".into())).init();
}

fn build_pool(config: Config) -> Result<Arc<Pool>> {
    let config = Arc::new(config);
    let store = Arc::new(Store::open(&config.db_path())?);
    let runner = Arc::new(Runner::new(&config)?);
    let devices: Arc<dyn Devices> = Arc::new(HostDevices::new(config.clone(), runner.clone()));
    Ok(Arc::new(Pool::new(config, store, runner, devices)))
}

async fn serve(config: Config) -> Result<()> {
    init_tracing();
    let addr = format!("{}:{}", config.listen.host, config.listen.port);
    let pool = build_pool(config)?;
    pool.recover().await?;
    let shutdown = CancellationToken::new();
    pool.spawn_reaper(shutdown.clone());
    let app = http::router(pool.clone(), shutdown.child_token());
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("binding {addr}"))?;
    tracing::info!("seiji listening on http://{addr}/mcp");
    let releaser = pool.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            // Stop the reaper and new MCP requests, then release every lease. Releasing cancels
            // running flows, so in-flight requests end quickly and the drain below is short.
            shutdown.cancel();
            tracing::info!("shutting down: releasing devices");
            releaser.release_all("server_shutdown").await;
        })
        .await?;
    Ok(())
}

/// Exercises the real host path for one user without touching their budget.
async fn doctor(config: Config, name: &str, platform: Platform) -> Result<()> {
    init_tracing();
    let config = Arc::new(config);
    let store = Store::open(&config.db_path())?;
    let user = store.user_by_name(name)?.with_context(|| format!("no user {name:?}"))?;
    let runner = Arc::new(Runner::new(&config)?);
    let devices = HostDevices::new(config.clone(), runner.clone());
    let account = account_for(&config, &user)?;
    let never = CancellationToken::new();
    let step = |s: &str| println!("== {s}");

    step("identity");
    let out = runner.run(&account, CmdSpec::new("/usr/bin/id"), Duration::from_secs(10), &never).await?.check("id")?;
    print!("{}", out.text());

    if let Some(maestro) = &config.maestro {
        step("maestro --version");
        let mut spec = CmdSpec::new(&maestro.bin).arg("--version");
        if let Some(java) = &maestro.java_home {
            spec = spec.env("JAVA_HOME", java.to_string_lossy());
        }
        print!(
            "{}",
            runner.run(&account, spec, Duration::from_secs(120), &never).await?.check("maestro --version")?.text()
        );
    }
    if platform == Platform::Ios {
        step("axe --version");
        let spec = CmdSpec::new(&config.axe.bin).arg("--version");
        print!("{}", runner.run(&account, spec, Duration::from_secs(30), &never).await?.check("axe --version")?.text());
    }

    let model =
        config.default_model(platform).with_context(|| format!("{} not configured", platform.as_str()))?.to_string();
    let lease_id = format!("doctor-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let mut ctx = DeviceCtx {
        lease_id: lease_id.clone(),
        platform,
        model,
        device_id: None,
        console_port: match platform {
            Platform::Ios => None,
            Platform::Android => {
                let used: HashSet<u16> = store
                    .active_leases()?
                    .iter()
                    .flat_map(|l| [l.console_port, l.console_port.map(|p| p + 1)])
                    .flatten()
                    .collect();
                Some(free_console_port(&config, &used)?)
            }
        },
        account: account.clone(),
    };
    step(&format!("create {} {}", platform.as_str(), ctx.model));
    let result: Result<()> = async {
        ctx.device_id = Some(devices.create(&ctx).await?);
        println!("device {}", ctx.device_id.as_deref().unwrap_or("?"));
        step("boot");
        let started = std::time::Instant::now();
        devices.boot(&ctx, &never).await?;
        println!("booted in {:.0}s", started.elapsed().as_secs_f64());
        step("screenshot");
        println!("{} bytes of JPEG", devices.screenshot(&ctx).await?.len());
        if platform == Platform::Ios {
            step("accessibility tree (axe describe-ui)");
            let tree = devices.describe_ui(&ctx).await?;
            let count = seiji::actions::nodes(&tree).len();
            anyhow::ensure!(count > 0, "simulator returned an empty accessibility tree");
            println!("{count} elements");
            step("video and log capture (3 s)");
            let dir = ctx.dir().join("doctor");
            let mkdir = CmdSpec::new("/bin/mkdir").arg("-p").arg(&dir);
            runner.run(&account, mkdir, Duration::from_secs(10), &never).await?.check("mkdir")?;
            let log = config.data_dir.join("run").join(format!("{lease_id}-doctor.log"));
            let video = devices.start_capture(&ctx, &Capture::Video { dest: dir.join("video.mp4") }).await?;
            let logs = devices
                .start_capture(&ctx, &Capture::Log { predicate: "process == \"SpringBoard\"".into(), out: log.clone() })
                .await?;
            tokio::time::sleep(Duration::from_secs(3)).await;
            devices.stop_capture(&ctx, &video).await;
            devices.stop_capture(&ctx, &logs).await;
            let size = CmdSpec::new("/usr/bin/stat").args(["-f", "%z"]).arg(dir.join("video.mp4"));
            let size = runner.run(&account, size, Duration::from_secs(10), &never).await?.check("video file")?;
            println!(
                "video {} bytes, log {} bytes",
                size.text().trim(),
                std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0)
            );
            let _ = std::fs::remove_file(&log);
        }
        Ok(())
    }
    .await;
    step("destroy");
    devices.destroy(&ctx).await;
    result?;
    println!(
        "OK: {} can boot, capture and delete {} devices",
        account.os_user.as_deref().unwrap_or(&account.name),
        platform.as_str()
    );
    Ok(())
}
