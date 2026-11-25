//! WebRTC for one viewer session: the shared H.264 stream over SRTP and the page's input over a
//! data channel, on one UDP port from `viewer.webrtc.ports`.
//!
//! Seiji is ICE-lite. It answers the page's offer (sent over the WebSocket) with a host candidate
//! for each address in `viewer.webrtc.candidates`, on the port it bound, and waits for the page to
//! connect. Only a peer that has the offer's answer (its ICE password and DTLS fingerprint) can
//! connect, and the answer goes only to the session's own WebSocket. Video and input are
//! encrypted (DTLS-SRTP, SCTP over DTLS). The WebSocket stays open for the replies, presets and
//! the fallback. Dropping the [`RtcSession`] ends the session and frees its port.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use base64::Engine;
use str0m::bwe::{Bitrate, BweKind};
use str0m::change::SdpOffer;
use str0m::channel::{ChannelData, ChannelId};
use str0m::format::Codec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::rtp::Extension;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use super::{ClientMsg, FRAME_HEADER, Hub, Video, ViewError, now_ms, view_only};
use crate::config::Webrtc;

/// A page that does not connect within this long loses its WebRTC session (it keeps the
/// WebSocket).
const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// The largest UDP datagram read.
const DATAGRAM: usize = 2000;
/// The data channel the page sends its input on.
const INPUT_CHANNEL: &str = "input";
/// The stream takes this share of the bandwidth estimate, and leaves the rest for
/// retransmissions, RTCP and the input channel.
const ESTIMATE_SHARE: f64 = 0.85;
/// The lowest rate the estimator holds traffic at.
const BWE_MIN: u64 = 150_000;

/// A running WebRTC session; dropping it ends the session.
pub(super) struct RtcSession {
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RtcSession {
    /// Ends the session and waits until its port is free.
    pub(super) async fn close(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RtcSession {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// What the session lends the WebRTC task.
pub(super) struct Peer {
    pub hub: Arc<Hub>,
    pub control: bool,
    /// The session's input queue, shared with the WebSocket.
    pub input: mpsc::Sender<ClientMsg>,
    /// Replies, over the WebSocket.
    pub out: mpsc::Sender<Message>,
}

/// Answers the page's offer and starts the session.
pub(super) async fn start(cfg: &Webrtc, offer: &str, peer: Peer) -> Result<(String, RtcSession), ViewError> {
    let offer = SdpOffer::from_sdp_string(offer).map_err(|e| ViewError::Bad(format!("invalid offer: {e}")))?;
    let socket = bind(cfg).await?;
    let port = socket.local_addr().map_err(anyhow::Error::from)?.port();
    let ips: Vec<IpAddr> = if cfg.candidates.is_empty() { vec![cfg.bind] } else { cfg.candidates.clone() };
    let mut rtc = Rtc::builder()
        .set_ice_lite(true)
        .clear_codecs()
        .enable_h264(true)
        // Chrome's own id for it: the page plays each frame at once (see `write`).
        .set_extension(5, Extension::PlayoutDelay)
        .enable_bwe(Some(Bitrate::bps(u64::from(peer.hub.opts().bitrate))))
        .build(Instant::now());
    let mut locals = Vec::with_capacity(ips.len());
    for ip in ips {
        let addr = SocketAddr::new(ip, port);
        let candidate = Candidate::host(addr, "udp").map_err(|e| anyhow::anyhow!("candidate {addr}: {e}"))?;
        rtc.add_local_candidate(candidate);
        locals.push(addr);
    }
    let answer =
        rtc.sdp_api().accept_offer(offer).map_err(|e| ViewError::Bad(format!("the offer cannot be answered: {e}")))?;
    // Probe up to the preset's bitrate, not past it.
    let desired = u64::from(peer.hub.rate.lock().expect("rate mutex").target.bitrate);
    rtc.bwe().set_bitrate(Bitrate::bps(BWE_MIN), Bitrate::bps(desired));
    let stop = CancellationToken::new();
    let video = peer.hub.video.subscribe();
    let params = peer.hub.format.lock().expect("format mutex").as_deref().and_then(parameter_sets);
    let cap = peer.hub.next_cap.fetch_add(1, Ordering::Relaxed);
    let state = State { mid: None, pt: None, channel: None, params, need_key: true, connected: false, cap };
    let task = tokio::spawn(run(rtc, socket, locals, video, peer, state, stop.clone()));
    Ok((answer.to_sdp_string(), RtcSession { stop, task: Some(task) }))
}

/// The first free port of the range.
async fn bind(cfg: &Webrtc) -> Result<UdpSocket, ViewError> {
    for port in cfg.ports[0]..=cfg.ports[1] {
        if let Ok(s) = UdpSocket::bind((cfg.bind, port)).await {
            return Ok(s);
        }
    }
    Err(ViewError::Rtc("Every WebRTC port is in use: the video stays on the WebSocket.".into()))
}

/// The SPS and PPS of the stream, and its NAL length size.
struct Params {
    /// The parameter sets as Annex B, put before each keyframe.
    sets: Vec<u8>,
    len_size: usize,
}

struct State {
    mid: Option<Mid>,
    pt: Option<Pt>,
    channel: Option<ChannelId>,
    params: Option<Params>,
    need_key: bool,
    connected: bool,
    /// This session's name for its bandwidth cap on the shared stream.
    cap: u64,
}

async fn run(
    mut rtc: Rtc,
    socket: UdpSocket,
    locals: Vec<SocketAddr>,
    mut video: broadcast::Receiver<Video>,
    peer: Peer,
    mut s: State,
    stop: CancellationToken,
) {
    let started = Instant::now();
    let mut buf = vec![0u8; DATAGRAM];
    loop {
        // Every change to `rtc` is followed by this drain, to the next timeout.
        let Some(timeout) = drain(&mut rtc, &socket, &mut s, &peer).await else { break };
        if !s.connected && started.elapsed() > CONNECT_WITHIN {
            break;
        }
        let wait = timeout.saturating_duration_since(Instant::now());
        tokio::select! {
            _ = stop.cancelled() => break,
            r = socket.recv_from(&mut buf) => {
                let Ok((n, source)) = r else { break };
                let Ok(contents) = buf[..n].try_into() else { continue };
                // The address the page sent to: the candidate of the same family.
                let destination = locals.iter().find(|a| a.is_ipv4() == source.is_ipv4()).copied().unwrap_or(locals[0]);
                let input = Input::Receive(Instant::now(), Receive { proto: Protocol::Udp, source, destination, contents });
                if rtc.accepts(&input) && rtc.handle_input(input).is_err() {
                    break;
                }
            }
            v = video.recv(), if s.connected => {
                if !s.write(&mut rtc, v, &peer.hub) {
                    break;
                }
            }
            _ = tokio::time::sleep(wait) => {
                if rtc.handle_input(Input::Timeout(Instant::now())).is_err() {
                    break;
                }
            }
        }
    }
    rtc.disconnect();
    peer.hub.cap(s.cap, None);
}

/// Sends what `rtc` has to send and handles its events. `None`: the session is over.
async fn drain(rtc: &mut Rtc, socket: &UdpSocket, s: &mut State, peer: &Peer) -> Option<Instant> {
    loop {
        match rtc.poll_output() {
            Ok(Output::Timeout(t)) => return Some(t),
            Ok(Output::Transmit(t)) => {
                let _ = socket.send_to(&t.contents, t.destination).await;
            }
            Ok(Output::Event(e)) => {
                if !s.on_event(rtc, e, peer) {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
}

impl State {
    fn on_event(&mut self, rtc: &mut Rtc, e: Event, peer: &Peer) -> bool {
        match e {
            Event::Connected => {
                self.connected = true;
                self.need_key = true;
                peer.hub.keyframe(false);
            }
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => return false,
            Event::MediaAdded(m) if m.kind == MediaKind::Video => {
                self.mid = Some(m.mid);
                self.pt = h264_pt(rtc, m.mid);
            }
            Event::ChannelOpen(id, label) if label == INPUT_CHANNEL => self.channel = Some(id),
            Event::ChannelData(d) => self.on_input(rtc, &d, peer),
            Event::KeyframeRequest(_) => peer.hub.keyframe(false),
            // The sender's pacer holds back what goes above the estimate, up to 2 s of it: keep
            // the shared stream under it instead.
            Event::EgressBitrateEstimate(BweKind::Twcc { estimate, .. }) => {
                let share = (estimate.as_u64() as f64 * ESTIMATE_SHARE).min(f64::from(u32::MAX)) as u32;
                peer.hub.cap(self.cap, Some(share));
            }
            _ => {}
        }
        true
    }

    /// One message on the input channel: a ping, answered on the channel, or input, queued with
    /// the WebSocket's. Replies to input go over the WebSocket.
    fn on_input(&mut self, rtc: &mut Rtc, d: &ChannelData, peer: &Peer) {
        let Ok(t) = std::str::from_utf8(&d.data) else { return };
        let reply = match serde_json::from_str::<ClientMsg>(t) {
            Ok(ClientMsg::Ping { at }) => {
                let pong = serde_json::json!({ "t": "pong", "at": at, "server_ms": now_ms() }).to_string();
                if let Some(mut ch) = rtc.channel(d.id) {
                    let _ = ch.write(false, pong.as_bytes());
                }
                None
            }
            Ok(
                msg @ (ClientMsg::Touch { .. }
                | ClientMsg::Key { .. }
                | ClientMsg::Button { .. }
                | ClientMsg::Type { .. }
                | ClientMsg::Paste { .. }
                | ClientMsg::PasteAndType { .. }),
            ) => {
                if !peer.control {
                    view_only(t)
                } else {
                    peer.input.try_send(msg).err().map(|_| ViewError::Busy.message(None))
                }
            }
            Ok(_) => Some(ViewError::Bad("the input channel takes input and pings only".into()).message(None)),
            Err(e) => Some(ViewError::Bad(format!("invalid message: {e}")).message(None)),
        };
        if let Some(r) = reply {
            let _ = peer.out.try_send(r);
        }
    }

    /// Hands one frame of the shared stream to the page. `false`: the stream ended.
    fn write(&mut self, rtc: &mut Rtc, v: Result<Video, broadcast::error::RecvError>, hub: &Hub) -> bool {
        let f = match v {
            Ok(Video::Frame(f)) => f,
            Ok(Video::Format(f)) => {
                self.params = parameter_sets(&f);
                self.need_key = true;
                return true;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                self.need_key = true;
                hub.keyframe(false);
                return true;
            }
            Err(broadcast::error::RecvError::Closed) => return false,
        };
        if self.need_key && !f.key {
            hub.keyframe(false);
            return true;
        }
        let (Some(mid), Some(pt), Some(params)) = (self.mid, self.pt, self.params.as_ref()) else { return true };
        let m = &f.message;
        let captured_us = i64::from_be_bytes(m[5..FRAME_HEADER].try_into().expect("8 bytes")).max(0) as u64;
        let Some(data) = annex_b(&m[FRAME_HEADER..], params, f.key) else { return true };
        let Some(writer) = rtc.writer(mid) else { return true };
        // RTP time is the capture time at 90 kHz, so the page can tell the frame's age. A
        // playout delay of 0 asks the browser to show each frame as soon as it is decoded,
        // without a jitter buffer: a late frame is late, never held for the ones after it.
        let time = MediaTime::new(captured_us * 9 / 100, Frequency::NINETY_KHZ);
        if writer.playout_delay(MediaTime::ZERO, MediaTime::ZERO).write(pt, Instant::now(), time, data).is_ok() {
            self.need_key = false;
        }
        true
    }
}

/// The payload type for H.264 High profile with packetization mode 1, else any H.264 one.
fn h264_pt(rtc: &mut Rtc, mid: Mid) -> Option<Pt> {
    let writer = rtc.writer(mid)?;
    let h264: Vec<_> = writer.payload_params().filter(|p| p.spec().codec == Codec::H264).collect();
    let high = |p: &&&str0m::format::PayloadParams| {
        let f = p.spec().format;
        f.packetization_mode == Some(1) && f.profile_level_id.is_some_and(|id| id >> 16 == 0x64)
    };
    let mode1 = |p: &&&str0m::format::PayloadParams| p.spec().format.packetization_mode == Some(1);
    h264.iter().find(high).or_else(|| h264.iter().find(mode1)).or(h264.first()).map(|p| p.pt())
}

/// The SPS and PPS of a `format` message's avcC `description`.
fn parameter_sets(format: &str) -> Option<Params> {
    let v: serde_json::Value = serde_json::from_str(format).ok()?;
    let avcc = base64::engine::general_purpose::STANDARD.decode(v["description"].as_str()?).ok()?;
    let len_size = usize::from(*avcc.get(4)? & 3) + 1;
    let mut sets = Vec::new();
    let mut at = 5;
    for mask in [0x1f, 0xff] {
        let count = avcc.get(at)? & mask;
        at += 1;
        for _ in 0..count {
            let len = usize::from(u16::from_be_bytes([*avcc.get(at)?, *avcc.get(at + 1)?]));
            sets.extend_from_slice(&[0, 0, 0, 1]);
            sets.extend_from_slice(avcc.get(at + 2..at + 2 + len)?);
            at += 2 + len;
        }
    }
    Some(Params { sets, len_size })
}

/// A frame of length-prefixed NAL units (as the helper encodes them) as Annex B, with the
/// parameter sets before a keyframe. `None` for a frame that does not parse.
fn annex_b(frame: &[u8], params: &Params, key: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(frame.len() + params.sets.len() + 16);
    if key {
        out.extend_from_slice(&params.sets);
    }
    let mut at = 0;
    while at < frame.len() {
        let len = frame.get(at..at + params.len_size)?.iter().fold(0usize, |n, b| n << 8 | usize::from(*b));
        at += params.len_size;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(frame.get(at..at + len)?);
        at += len;
    }
    Some(out)
}
