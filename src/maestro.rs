//! Maestro CLI integration.
//!
//! `maestro mcp` hardcodes the iOS XCTest driver port (22087), so two iOS simulators cannot be
//! driven at once through it. Seiji calls the CLI per operation instead and gives every lease its
//! own `--driver-host-port`, which lets any number of leases run flows in parallel.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Maestro as MaestroConfig};
use crate::devices::DeviceCtx;
use crate::exec::{CmdSpec, Output, Runner};

pub struct Maestro {
    config: Arc<Config>,
    runner: Arc<Runner>,
}

pub struct FlowResult {
    pub passed: bool,
    pub output: Output,
}

impl Maestro {
    pub fn new(config: Arc<Config>, runner: Arc<Runner>) -> Self {
        Self { config, runner }
    }

    fn cfg(&self) -> Result<&MaestroConfig> {
        self.config.maestro.as_ref().context("Maestro is not configured on this host (`maestro` in seiji.json)")
    }

    fn base(&self, ctx: &DeviceCtx, driver_port: u16) -> Result<CmdSpec> {
        let cfg = self.cfg()?;
        let mut spec = CmdSpec::new(&cfg.bin)
            .arg("--device")
            .arg(ctx.device_id()?)
            .arg("--driver-host-port")
            .arg(driver_port.to_string())
            .cwd(ctx.dir())
            .env("MAESTRO_CLI_NO_ANALYTICS", "1")
            .env("MAESTRO_CLI_ANALYSIS_NOTIFICATION_DISABLED", "true")
            .env("MAESTRO_DISABLE_UPDATE_CHECK", "true");
        if let Some(java) = &cfg.java_home {
            spec = spec.env("JAVA_HOME", java.to_string_lossy());
        }
        if let Some(a) = &self.config.android {
            spec = spec.env("ANDROID_HOME", a.sdk_root.to_string_lossy());
        }
        Ok(spec)
    }

    /// Writes the YAML into `out_dir/flow.yaml` as the tenant and runs it with `maestro test`.
    /// Maestro's report, debug output and test output land under `out_dir/maestro/`.
    /// `flow_file` is a daemon-owned temp file holding the YAML.
    pub async fn run_flow(
        &self,
        ctx: &DeviceCtx,
        driver_port: u16,
        flow_file: &Path,
        env: &BTreeMap<String, String>,
        out_dir: &Path,
        cancel: &CancellationToken,
    ) -> Result<FlowResult> {
        let flow = out_dir.join("flow.yaml");
        let maestro_dir = out_dir.join("maestro");
        let write = CmdSpec::sh(r#"set -e; mkdir -p "$1"; cat > "$2""#, [maestro_dir.as_os_str(), flow.as_os_str()])
            .stdin_file(flow_file);
        self.runner.run(&ctx.account, write, Duration::from_secs(30), cancel).await?.check("writing flow")?;

        let mut spec = self
            .base(ctx, driver_port)?
            .arg("test")
            .arg("--no-ansi")
            .args(["--format", "junit", "--output"])
            .arg(maestro_dir.join("report.xml"))
            .arg("--debug-output")
            .arg(maestro_dir.join("debug"))
            .arg("--flatten-debug-output")
            .arg("--test-output-dir")
            .arg(maestro_dir.join("output"));
        for (k, v) in env {
            spec = spec.arg("-e").arg(format!("{k}={v}"));
        }
        spec = spec.arg(&flow);
        let timeout = Duration::from_secs(self.cfg()?.flow_timeout_seconds);
        let output = self.runner.run(&ctx.account, spec, timeout, cancel).await?;
        Ok(FlowResult { passed: output.success(), output })
    }

    /// `maestro hierarchy --compact`: the current screen's view hierarchy.
    pub async fn hierarchy(&self, ctx: &DeviceCtx, driver_port: u16, cancel: &CancellationToken) -> Result<String> {
        let spec = self.base(ctx, driver_port)?.arg("hierarchy").arg("--no-ansi").arg("--compact");
        let out =
            self.runner.run(&ctx.account, spec, Duration::from_secs(120), cancel).await?.check("maestro hierarchy")?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}
