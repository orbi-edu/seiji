//! macOS tenant accounts: provisioning and the privileged `exec-as` / `kill-as` helpers.
//!
//! The daemon runs unprivileged. sudoers lets it run exactly
//! `/usr/local/bin/seiji exec-as *` and `/usr/local/bin/seiji kill-as *` as root. Both refuse any
//! account that is not a member of the tenant group, is an admin, or is a system account, so the
//! daemon can never act as anyone but a tenant.

use std::ffi::{CStr, CString, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Fixed, not configurable: the root helpers must not trust a daemon-writable config for who
/// counts as a tenant.
pub const TENANT_GROUP: &str = "seiji-tenants";
const ADMIN_GID: u32 = 80;
const MIN_TENANT_UID: u32 = 501;
const TENANT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, Clone)]
pub struct OsAccount {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

pub fn lookup(name: &str) -> Result<Option<OsAccount>> {
    let cname = CString::new(name)?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(cname.as_ptr(), &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc != 0 {
        bail!("getpwnam_r({name}) failed: {}", std::io::Error::from_raw_os_error(rc));
    }
    if result.is_null() {
        return Ok(None);
    }
    let home = unsafe { CStr::from_ptr(pwd.pw_dir) };
    Ok(Some(OsAccount {
        name: name.to_string(),
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home: PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes())),
    }))
}

fn group_gid(group: &str) -> Result<Option<u32>> {
    let cname = CString::new(group)?;
    let gr = unsafe { libc::getgrnam(cname.as_ptr()) };
    Ok((!gr.is_null()).then(|| unsafe { (*gr).gr_gid }))
}

fn groups_of(account: &OsAccount) -> Result<Vec<u32>> {
    let cname = CString::new(account.name.as_str())?;
    let mut n: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::c_int; n as usize];
        let base = account.gid as libc::c_int;
        let rc = unsafe { libc::getgrouplist(cname.as_ptr(), base, groups.as_mut_ptr(), &mut n) };
        if rc >= 0 {
            groups.truncate(n as usize);
            return Ok(groups.into_iter().map(|g| g as u32).collect());
        }
        n *= 2;
        if n > 4096 {
            bail!("too many groups for {}", account.name);
        }
    }
}

/// The account must exist, be in [`TENANT_GROUP`], not be an admin and not be a system account.
pub fn validate_tenant(name: &str) -> Result<OsAccount> {
    let account = lookup(name)?.with_context(|| format!("no macOS account {name:?}"))?;
    if account.uid < MIN_TENANT_UID {
        bail!("{name:?} is a system account");
    }
    let gid = group_gid(TENANT_GROUP)?.with_context(|| format!("no group {TENANT_GROUP:?}"))?;
    let groups = groups_of(&account)?;
    if !groups.contains(&gid) {
        bail!("{name:?} is not in {TENANT_GROUP:?}");
    }
    if groups.contains(&ADMIN_GID) {
        bail!("{name:?} is an admin; refusing");
    }
    Ok(account)
}

pub struct ExecArgs {
    pub user: String,
    /// Written by the tenant (after the privilege drop) with the process-group id.
    pub pidfile: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<String>,
    pub command: Vec<OsString>,
}

fn require_root() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("must run as root (through sudo)");
    }
    Ok(())
}

/// Stage 1 (root, via sudo): re-exec through `launchctl asuser` so the command joins the tenant's
/// per-user launchd domain, where its CoreSimulatorService lives.
pub fn exec_as(args: ExecArgs) -> Result<()> {
    require_root()?;
    let account = validate_tenant(&args.user)?;
    let me = std::env::current_exe()?;
    let mut cmd = Command::new("/bin/launchctl");
    cmd.arg("asuser").arg(account.uid.to_string()).arg(me).arg("exec-drop").arg(&args.user);
    if let Some(p) = &args.pidfile {
        cmd.arg("--pidfile").arg(p);
    }
    if let Some(c) = &args.cwd {
        cmd.arg("--cwd").arg(c);
    }
    for e in &args.env {
        cmd.arg("--env").arg(e);
    }
    cmd.arg("--").args(&args.command);
    Err(cmd.exec()).context("exec launchctl asuser")
}

/// Stage 2 (root, inside the tenant's launchd domain): new session, drop to the tenant, exec.
pub fn exec_drop(args: ExecArgs) -> Result<()> {
    require_root()?;
    let account = validate_tenant(&args.user)?;
    let (program, rest) = args.command.split_first().context("no command")?;

    let mut cmd = Command::new(program);
    cmd.args(rest).env_clear();
    cmd.env("HOME", &account.home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("SHELL", "/bin/zsh")
        .env("PATH", TENANT_PATH);
    for e in &args.env {
        let (k, v) = e.split_once('=').with_context(|| format!("--env {e:?} is not K=V"))?;
        cmd.env(k, v);
    }
    let cwd = CString::new(args.cwd.as_deref().unwrap_or(&account.home).as_os_str().as_bytes())?;

    let pidfile = args.pidfile.clone();
    let user = CString::new(account.name.as_str())?;
    let (uid, gid) = (account.uid, account.gid);
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            drop_to(&user, uid, gid)?;
            // Everything below runs as the tenant, never as root: the paths come from the
            // unprivileged daemon, so the kernel checks them against the tenant's rights.
            if libc::chdir(cwd.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(p) = &pidfile {
                std::fs::write(p, format!("{}\n", libc::getpid()))?;
                // The daemon reads it; the rest of the tenant's files stay private.
                std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
            }
            libc::umask(0o077);
            Ok(())
        });
    }
    Err(cmd.exec()).with_context(|| format!("exec {}", Path::new(program).display()))
}

/// `initgroups`, `setgid`, `setuid`, in that order, then proves root cannot be regained.
unsafe fn drop_to(user: &CStr, uid: u32, gid: u32) -> std::io::Result<()> {
    unsafe {
        if libc::initgroups(user.as_ptr(), gid as libc::c_int) != 0 || libc::setgid(gid) != 0 || libc::setuid(uid) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        if libc::setuid(0) == 0 || libc::geteuid() != uid || libc::getegid() != gid {
            return Err(std::io::Error::other("privilege drop did not stick"));
        }
    }
    Ok(())
}

/// Sends `signal` to process group `pgid` as the tenant: the kernel only lets it reach the
/// tenant's own processes.
pub fn kill_as(user: &str, pgid: i32, signal: i32) -> Result<()> {
    require_root()?;
    if pgid <= 1 {
        bail!("refusing pgid {pgid}");
    }
    let account = validate_tenant(user)?;
    let cuser = CString::new(user)?;
    unsafe {
        drop_to(&cuser, account.uid, account.gid).with_context(|| format!("dropping to {user}"))?;
        if libc::killpg(pgid, signal) != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                bail!("killpg({pgid}, {signal}): {err}");
            }
        }
    }
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).output().with_context(|| format!("running {program}"))?;
    if !out.status.success() {
        bail!(
            "{program} {} failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Smallest uid above every existing one, and at least 600.
fn next_uid() -> Result<u32> {
    let out = run("/usr/bin/dscl", &[".", "-list", "/Users", "UniqueID"])?;
    let max = out.lines().filter_map(|l| l.split_whitespace().last()?.parse::<u32>().ok()).max().unwrap_or(0);
    Ok((max + 1).max(600))
}

/// Creates the tenant group and a hidden, non-admin macOS account in it that cannot log in,
/// with a private home. Re-running it repairs a tenant account. It refuses to touch an existing
/// account that is not already a tenant. Must run as root.
pub fn provision(os_user: &str) -> Result<OsAccount> {
    require_root()?;
    if !os_user.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("account names may only use [A-Za-z0-9_-]");
    }
    if group_gid(TENANT_GROUP)?.is_none() {
        run("/usr/sbin/dseditgroup", &["-o", "create", "-r", "Seiji tenants", TENANT_GROUP])?;
    }
    let record = format!("/Users/{os_user}");
    match lookup(os_user)? {
        Some(_) => {
            validate_tenant(os_user).with_context(|| {
                format!("{os_user:?} already exists and is not a Seiji tenant account; refusing to change it")
            })?;
        }
        None => {
            // No password and no login shell: the account only owns processes and files.
            let uid = next_uid()?.to_string();
            let home = format!("/Users/{os_user}");
            let attrs: [(&str, &str); 6] = [
                ("UniqueID", &uid),
                ("PrimaryGroupID", "20"),
                ("RealName", "Seiji tenant"),
                ("NFSHomeDirectory", &home),
                ("UserShell", "/usr/bin/false"),
                ("Password", "*"),
            ];
            run("/usr/bin/dscl", &[".", "-create", &record])?;
            for (key, value) in attrs {
                run("/usr/bin/dscl", &[".", "-create", &record, key, value])?;
            }
            run("/usr/sbin/dseditgroup", &["-o", "edit", "-a", os_user, "-t", "user", TENANT_GROUP])?;
        }
    }
    run("/usr/bin/dscl", &[".", "-create", &record, "IsHidden", "1"])?;
    run("/usr/sbin/createhomedir", &["-c", "-u", os_user])?;
    let account = validate_tenant(os_user)?;
    let home = account.home.to_string_lossy().into_owned();
    // Private home: other tenants (and the daemon) cannot read it; the daemon goes through exec-as.
    run("/bin/chmod", &["700", &home])?;
    Ok(account)
}
