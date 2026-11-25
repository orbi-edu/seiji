//! Where a viewer's time goes, stage by stage, for its Stats overlay and the log.
//!
//! The frame path has six stages, each timed on one clock:
//!
//! 1. **Encode**: capture to the encoder's output, in `seiji-sim` (host clock).
//! 2. **Helper → server**: the encoder's output to the daemon reading it (host clock).
//! 3. **In server**: the daemon reading it to the frame written to the viewer's socket (host
//!    clock): the shared buffer, flow control and a write that waits on a slow link.
//! 4. **Network**: written to the socket to the page receiving it: the TCP path, any proxy.
//!    The page measures stages 3 and 4 together on the server's clock (its clock offset comes
//!    from the pings), and this side measures stage 3, so stage 4 is their difference.
//! 5. **Decode**: received to decoded (page clock).
//! 6. **Capture → shown**: the whole, the frame age.
//!
//! An input (a finger down or up, a key, a button) has its own path, timed the same way:
//!
//! 1. **Upload**: the page seeing it to the daemon reading it (server's clock, through the
//!    page's clock offset).
//! 2. **Input in server**: the daemon reading it to the helper having it. Of that, **lock** is
//!    the wait for the device's op lock and **pacing** the hold that keeps a gesture's spacing.
//! 3. **Screen reacts**: the helper having it to the first frame captured after (host clock):
//!    the simulator and the app drawing, and the capture's frame pacing.
//! 4. That frame's encode, helper → server, in server, network and decode, as above.
//! 5. **Input → shown**: the whole, on the page's clock.
//!
//! The daemon tells the page when it read and handed on each input (`applied`); the page does
//! the rest. Text goes through AXe and has one stage, read to typed (**text**).

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::Via;

/// Samples kept for each stage: about 8 s of frames at 30 fps.
const KEEP: usize = 256;
/// The longest a stage may take, in milliseconds: a page's number past it is dropped.
const MAX_MS: f64 = 120_000.0;

/// The last samples of one stage, in milliseconds.
#[derive(Default)]
pub(super) struct Samples(VecDeque<f64>);

impl Samples {
    pub(super) fn push(&mut self, ms: f64) {
        if self.0.len() == KEEP {
            self.0.pop_front();
        }
        self.0.push_back(ms);
    }

    /// The p50 and p95, rounded to 0.1 ms.
    pub(super) fn p50_p95(&self) -> Option<[f64; 2]> {
        if self.0.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = self.0.iter().copied().collect();
        v.sort_by(f64::total_cmp);
        let at = |p: f64| (v[((v.len() - 1) as f64 * p).round() as usize] * 10.0).round() / 10.0;
        Some([at(0.5), at(0.95)])
    }
}

/// What the page measured, sent every 5 s: `[p50, p95]` in milliseconds for each stage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PageReport {
    pub transport: Option<Via>,
    /// Capture to shown.
    pub age: Option<[f64; 2]>,
    pub encode: Option<[f64; 2]>,
    /// The encoder's output to the daemon reading it.
    pub helper: Option<[f64; 2]>,
    /// The daemon reading the frame to the page receiving it (in server and network).
    pub down: Option<[f64; 2]>,
    pub decode: Option<[f64; 2]>,
    /// An input seen by the page to read by the daemon.
    pub upload: Option<[f64; 2]>,
    /// An input with the helper to the first frame captured after it.
    pub react: Option<[f64; 2]>,
    /// An input seen by the page to that frame shown.
    pub photon: Option<[f64; 2]>,
    pub rtt: Option<f64>,
    pub jitter: Option<f64>,
    /// Percent lost in the last 3 s.
    pub lost: Option<f64>,
    pub fps: Option<f64>,
}

impl PageReport {
    /// Keeps only numbers that can be timings: a page's report goes into the log.
    pub(super) fn checked(mut self) -> Self {
        let ok = |v: f64| v.is_finite() && (0.0..=MAX_MS).contains(&v);
        let stages = [
            &mut self.age,
            &mut self.encode,
            &mut self.helper,
            &mut self.down,
            &mut self.decode,
            &mut self.upload,
            &mut self.react,
            &mut self.photon,
        ];
        for pair in stages {
            if pair.is_some_and(|[a, b]| !ok(a) || !ok(b)) {
                *pair = None;
            }
        }
        for one in [&mut self.rtt, &mut self.jitter, &mut self.lost, &mut self.fps] {
            if one.is_some_and(|v| !ok(v)) {
                *one = None;
            }
        }
        self
    }
}

/// One viewer's timings: the server's stages and the page's last report.
#[derive(Default)]
pub(super) struct Pipeline {
    /// Stage 3, frame by frame.
    pub server: Samples,
    /// The daemon reading an input message to the helper having it.
    pub input: Samples,
    /// Of `input`: the wait for the op lock.
    pub lock: Samples,
    /// Of `input`: the hold that keeps a gesture's spacing.
    pub pace: Samples,
    /// Text: read to typed or pasted.
    pub text: Samples,
    pub page: Option<PageReport>,
}

impl Pipeline {
    /// What the page shows of this side: `{"t": "server_stats", "server", "input", "lock",
    /// "pace", "text"}`.
    pub(super) fn message(&self) -> serde_json::Value {
        serde_json::json!({
            "t": "server_stats",
            "server": self.server.p50_p95(),
            "input": self.input.p50_p95(),
            "lock": self.lock.p50_p95(),
            "pace": self.pace.p50_p95(),
            "text": self.text.p50_p95(),
        })
    }
    /// One log line with every stage, when the page has reported.
    pub(super) fn log(&self, lease: &str, viewer: &str) {
        let Some(p) = &self.page else { return };
        let p50 = |v: Option<[f64; 2]>| v.map(|[a, _]| a);
        let p95 = |v: Option<[f64; 2]>| v.map(|[_, b]| b);
        let server = self.server.p50_p95();
        // The network is the page's "down" minus this side's part of it, to 0.1 ms.
        let diff = |d: f64, s: f64| ((d - s).max(0.0) * 10.0).round() / 10.0;
        let net = p.down.zip(server).map(|([d50, d95], [s50, s95])| [diff(d50, s50), diff(d95, s95)]);
        let (input, lock, pace, text) =
            (self.input.p50_p95(), self.lock.p50_p95(), self.pace.p50_p95(), self.text.p50_p95());
        let transport = p.transport.map(|t| if t == Via::Rtc { "rtc" } else { "ws" });
        // A stage the page could not measure is left out of the line.
        tracing::info!(
            lease = %lease,
            viewer = %viewer,
            transport,
            age_p50 = p50(p.age),
            age_p95 = p95(p.age),
            encode_p50 = p50(p.encode),
            encode_p95 = p95(p.encode),
            helper_p50 = p50(p.helper),
            helper_p95 = p95(p.helper),
            server_p50 = p50(server),
            server_p95 = p95(server),
            network_p50 = p50(net),
            network_p95 = p95(net),
            decode_p50 = p50(p.decode),
            decode_p95 = p95(p.decode),
            upload_p50 = p50(p.upload),
            upload_p95 = p95(p.upload),
            input_server_p50 = p50(input),
            input_server_p95 = p95(input),
            input_lock_p50 = p50(lock),
            input_lock_p95 = p95(lock),
            input_pace_p50 = p50(pace),
            input_pace_p95 = p95(pace),
            react_p50 = p50(p.react),
            react_p95 = p95(p.react),
            photon_p50 = p50(p.photon),
            photon_p95 = p95(p.photon),
            text_p50 = p50(text),
            text_p95 = p95(text),
            rtt = p.rtt,
            jitter = p.jitter,
            lost = p.lost,
            fps = p.fps,
            "viewer pipeline"
        );
    }
}
