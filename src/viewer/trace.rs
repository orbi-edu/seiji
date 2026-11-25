//! The viewer trace: while an admin has it on, one JSON line for every input, every frame Seiji
//! writes to a viewer, and every frame and input answer a page reports, in
//! `<dataDir>/viewer-trace.jsonl`. Times are Unix microseconds (`_us`) or milliseconds (`_ms`)
//! on the host's clock. It holds timings only: no coordinates, no keys, no text.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;

/// The trace stops by itself at this size.
pub const MAX_BYTES: u64 = 64 << 20;
/// The most frames or input answers one page report may carry.
pub(super) const MAX_BATCH: usize = 1024;

pub struct Trace {
    path: PathBuf,
    on: AtomicBool,
    /// The open file and the bytes written to it.
    file: StdMutex<Option<(File, u64)>>,
}

impl Trace {
    pub fn new(path: PathBuf) -> Self {
        Self { path, on: AtomicBool::new(false), file: StdMutex::new(None) }
    }

    pub fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The size of the last trace, if there is one.
    pub fn bytes(&self) -> Option<u64> {
        std::fs::metadata(&self.path).ok().map(|m| m.len())
    }

    /// Starts a new trace, in place of the last one.
    pub fn start(&self) -> std::io::Result<()> {
        let file = OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&self.path)?;
        *self.file.lock().expect("trace mutex") = Some((file, 0));
        self.on.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        self.on.store(false, Ordering::Relaxed);
        *self.file.lock().expect("trace mutex") = None;
    }

    /// Appends the line `event` builds; builds nothing while the trace is off.
    pub(super) fn write(&self, event: impl FnOnce() -> serde_json::Value) {
        if !self.on() {
            return;
        }
        let mut line = serde_json::to_vec(&event()).unwrap_or_default();
        line.push(b'\n');
        let mut file = self.file.lock().expect("trace mutex");
        let Some((f, written)) = file.as_mut() else { return };
        if *written + line.len() as u64 > MAX_BYTES {
            self.on.store(false, Ordering::Relaxed);
            *file = None;
            tracing::warn!(path = %self.path.display(), "viewer trace reached 64 MiB and stopped");
            return;
        }
        match f.write_all(&line) {
            Ok(()) => *written += line.len() as u64,
            Err(e) => {
                self.on.store(false, Ordering::Relaxed);
                *file = None;
                tracing::warn!(path = %self.path.display(), error = %e, "viewer trace stopped");
            }
        }
    }
}

/// What a page reports while the trace is on, every second. Times are Unix milliseconds on the
/// server's clock, through the page's clock offset.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageTrace {
    /// The server's clock minus the page's, and the round trip it came from.
    pub offset_ms: f64,
    pub rtt_ms: Option<f64>,
    /// Frames shown: `(frame number, received, shown)`.
    #[serde(default)]
    pub frames: Vec<(u32, f64, f64)>,
    /// Inputs answered: `(the input's ts, when the page saw it, the answering frame's capture
    /// in µs, when that frame was shown)`.
    #[serde(default)]
    pub inputs: Vec<(f64, f64, i64, f64)>,
}

impl PageTrace {
    pub(super) fn fits(&self) -> bool {
        self.frames.len() <= MAX_BATCH && self.inputs.len() <= MAX_BATCH
    }

    pub(super) fn write(&self, trace: &Trace, lease: &str, viewer: &str) {
        trace.write(|| {
            serde_json::json!({
                "ev": "clock", "lease": lease, "viewer": viewer, "at_ms": super::now_ms(),
                "offset_ms": self.offset_ms, "rtt_ms": self.rtt_ms,
            })
        });
        for (seq, received, shown) in &self.frames {
            trace.write(|| {
                serde_json::json!({
                    "ev": "shown", "lease": lease, "viewer": viewer, "seq": seq,
                    "received_ms": received, "shown_ms": shown,
                })
            });
        }
        for (ts, seen, capture, shown) in &self.inputs {
            trace.write(|| {
                serde_json::json!({
                    "ev": "answer", "lease": lease, "viewer": viewer, "ts": ts, "seen_ms": seen,
                    "capture_us": capture, "shown_ms": shown,
                })
            });
        }
    }
}
