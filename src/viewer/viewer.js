// Seiji's screen viewer. One WebSocket (`ws`, same origin, with the session cookie) carries the
// simulator's screen as H.264, decoded with WebCodecs onto a canvas, and the input: a finger that
// follows the pointer (down, move, up), wheel scrolls as drags, keys as HID usages, text through
// the simulator clipboard. The page acknowledges every frame so the server never queues more
// than a few frames for it on a slow link.
"use strict";

(() => {
  const $ = (id) => document.getElementById(id);
  const canvas = $("frame");
  const g2d = canvas.getContext("2d", { alpha: false, desynchronized: true });
  const screen = $("screen");
  const veil = $("veil");
  const veilText = $("veil-text");
  const note = $("note");
  const statsOverlay = $("stats-overlay");
  let info = null;
  let ended = false;
  let control = false;

  // ── helpers ────────────────────────────────────────────────────────────

  function showNote(text, kind) {
    if (!text) {
      note.hidden = true;
      return;
    }
    note.textContent = text;
    note.className = "note" + (kind === "error" ? " error" : "");
    note.hidden = false;
  }

  let noteTimer = 0;
  function flash(text, kind) {
    showNote(text, kind);
    clearTimeout(noteTimer);
    noteTimer = setTimeout(() => showNote(info && info.busy && control ? busyText : ""), 3500);
  }

  const busyText = "A run is using the device. Input is paused until it ends.";

  function end(message) {
    ended = true;
    veil.hidden = false;
    veilText.textContent = message;
    $("dot").className = "dot off";
    for (const b of document.querySelectorAll("button, select")) b.disabled = true;
    if (ws) ws.close();
    stopRtc();
    if (decoder && decoder.state !== "closed") decoder.close();
  }

  async function errorOf(resp) {
    try {
      const doc = await resp.json();
      return (doc && doc.error) || {};
    } catch {
      return {};
    }
  }

  function minutes(sec) {
    if (sec <= 0) return "ending";
    const m = Math.round(sec / 60);
    return m >= 60 ? `${Math.floor(m / 60)} h ${m % 60} min` : `${m} min`;
  }

  // ── info ───────────────────────────────────────────────────────────────

  async function refresh() {
    if (ended) return;
    let resp;
    try {
      resp = await fetch("info", { cache: "no-store" });
    } catch {
      $("dot").className = "dot off";
      return;
    }
    if (resp.status === 401 || resp.status === 410) {
      end((await errorOf(resp)).message || "The viewer closed.");
      return;
    }
    if (!resp.ok) return;
    info = await resp.json();
    control = !!info.control;
    // com.apple.CoreSimulator.SimRuntime.iOS-18-2 → iOS 18.2
    const runtime = (info.runtime || "")
      .replace(/^com\.apple\.CoreSimulator\.SimRuntime\./, "")
      .replace("-", " ")
      .replace(/-/g, ".");
    $("name").textContent = info.model || "Simulator";
    const app = info.app && info.app.bundle_id ? ` · ${info.app.bundle_id}` : "";
    $("meta").textContent = `${runtime}${app}`;
    $("lease").textContent = info.lease_id;
    $("ends").textContent = `ends in ${minutes(info.ends_at_latest - Date.now() / 1000)}`;
    $("viewers").textContent = info.viewers > 1 ? `${info.viewers} watching` : "";
    $("mode").hidden = control;
    for (const b of document.querySelectorAll("[data-control]")) b.hidden = !control;
    $("keys").hidden = !control;
    $("dot").className = "dot " + (info.busy ? "busy" : "live");
    showNote(info.busy && control ? busyText : "");
    if (info.preset) $("preset").value = info.preset;
  }

  // ── socket ─────────────────────────────────────────────────────────────

  let ws = null;
  let retry = 0;
  let clockOffset = 0; // server clock minus browser clock
  const offsets = []; // [round trip, offset] of the last pongs
  let rtt = null;

  // `false` when the socket is not open: the message was not sent.
  function send(msg) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return false;
    ws.send(JSON.stringify(msg));
    return true;
  }

  // Input goes on the WebRTC input channel once it is open, else on the socket. A gesture keeps
  // the path its `down` took, so its points never overtake each other on two paths.
  let gesturePath = null;
  function sendInput(msg) {
    let path = dc && dc.readyState === "open" ? "rtc" : "ws";
    // Keys and buttons carry the page's time too, so their way to the screen is timed.
    if ((msg.t === "key" || msg.t === "button") && msg.ts === undefined) msg.ts = performance.now();
    if (msg.t === "touch") {
      if (msg.phase === "down") gesturePath = path;
      else if (gesturePath) path = gesturePath;
      if (msg.phase === "up") gesturePath = null;
    }
    if (path === "rtc" && dc && dc.readyState === "open") {
      dc.send(JSON.stringify(msg));
      return true;
    }
    return send(msg);
  }

  function connect() {
    if (ended) return;
    if (!("VideoDecoder" in window)) {
      end("This browser cannot show the stream: it needs WebCodecs (a current Chrome, Edge, Safari or Firefox).");
      return;
    }
    veil.hidden = false;
    veilText.textContent = retry ? "Reconnecting…" : "Connecting…";
    const url = new URL("ws", location.href);
    url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    ws.onopen = () => {
      retry = 0;
      // A new socket starts a new sample: the old one's round trips say nothing about it.
      rtts.length = 0;
      resetLoss();
      lastPong = performance.now();
      ping();
      // After skipped frames, the server may resume this page at a delta frame that refers to a
      // long-term reference it has, not at a keyframe. Checked with the WebCodecs of WebKit and
      // Chrome on macOS (both VideoToolbox), where the pictures match the full stream; any other
      // decoder gets keyframes.
      const ua = navigator.userAgent;
      send({ t: "decoder", refresh: /Macintosh/.test(ua) && !/Firefox\//.test(ua) });
      startRtc();
    };
    ws.onmessage = (ev) => (typeof ev.data === "string" ? onText(JSON.parse(ev.data)) : onFrame(ev.data));
    ws.onclose = (ev) => {
      ws = null;
      resetVideo();
      // The session's WebRTC end closed with it; the next socket tries again.
      stopRtc();
      rtcTried = false;
      if (ended) return;
      // An input sent before the drop may or may not have reached the device: say so, never replay.
      if (pending.size) {
        flash(`The connection dropped: ${pending.size === 1 ? "an input" : `${pending.size} inputs`} may not have been applied.`, "error");
        pending.clear();
      }
      // 4401: the session expired. 4410: the device was released.
      if (ev.code === 4401 || ev.code === 4410) {
        end(ev.reason || "The viewer closed.");
        return;
      }
      veil.hidden = false;
      veilText.textContent = "Reconnecting…";
      refresh();
      retry = Math.min(retry + 1, 5);
      setTimeout(connect, 600 * retry);
    };
  }

  // Over the input channel when it is open: the round trip of the path input takes.
  function ping() {
    const msg = { t: "ping", at: performance.now() };
    if (dc && dc.readyState === "open") dc.send(JSON.stringify(msg));
    else send(msg);
  }
  setInterval(ping, 1000);

  // ── link quality ───────────────────────────────────────────────────────

  // The last round trips, for the jitter: how much one differs from the one before.
  const rtts = [];
  let lastPong = 0;
  let poorSince = 0;
  let poorNotedAt = -Infinity;
  const linkEl = $("link");

  function jitter() {
    if (rtts.length < 2) return 0;
    let sum = 0;
    for (let i = 1; i < rtts.length; i++) sum += Math.abs(rtts[i] - rtts[i - 1]);
    return sum / (rtts.length - 1);
  }

  function onPong(sample) {
    rtts.push(sample);
    if (rtts.length > 10) rtts.shift();
    lastPong = performance.now();
  }

  // Frames lost on the way, in windows of 3 s, as Moonlight judges a link: poor at 30 % in one
  // window or 15 % in two in a row, fine again at 5 % or less. Over the WebSocket a lost frame is
  // one the server skipped for this page (a gap in the frame numbers); over WebRTC, a lost
  // packet.
  const loss = { got: 0, lost: 0, since: performance.now(), pct: null, last: 0, poor: false };
  function countFrames(got, lost) {
    loss.got += got;
    loss.lost += lost;
  }
  function sampleLoss(now) {
    if (now - loss.since < 3000) return;
    const total = loss.got + loss.lost;
    const pct = total ? (loss.lost * 100) / total : null;
    if (pct !== null) {
      if (pct >= 30 || (pct >= 15 && loss.last >= 15)) loss.poor = true;
      else if (pct <= 5) loss.poor = false;
      loss.last = pct;
    }
    loss.pct = pct;
    loss.got = 0;
    loss.lost = 0;
    loss.since = now;
  }
  function resetLoss() {
    Object.assign(loss, { got: 0, lost: 0, since: performance.now(), pct: null, last: 0, poor: false });
  }

  function renderLink() {
    if (ended) {
      linkEl.hidden = true;
      return;
    }
    const now = performance.now();
    sampleLoss(now);
    let text;
    let quality;
    if (!ws || ws.readyState !== WebSocket.OPEN) {
      text = "offline";
      quality = "poor";
    } else if (lastPong && now - lastPong > 3000) {
      // Nothing came back for a while: the link is stalled, whatever the last round trip was.
      text = `no answer ${Math.round((now - lastPong) / 1000)} s`;
      quality = "poor";
    } else if (rtt !== null) {
      const j = jitter();
      text = `${Math.round(rtt)} ms ±${Math.round(j)}`;
      quality = rtt < 150 && j < 30 ? "good" : rtt < 350 && j < 100 ? "fair" : "poor";
      if (loss.pct !== null && loss.pct >= 1) text += ` · ${Math.round(loss.pct)}% lost`;
      if (loss.poor) quality = "poor";
      else if (quality === "good" && loss.pct !== null && loss.pct > 5) quality = "fair";
    } else {
      linkEl.hidden = true;
      return;
    }
    linkEl.textContent = text;
    linkEl.className = `link ${quality}`;
    linkEl.title =
      "Round trip to the Seiji host, how much it varies (jitter), and the share of frames lost on the way. " +
      "Over a poor link the picture and your input lag; Smooth needs less bandwidth.";
    linkEl.hidden = false;
    // Say so once a minute at most, after the link stayed poor for a few seconds.
    if (quality !== "poor") {
      poorSince = 0;
    } else if (!poorSince) {
      poorSince = now;
    } else if (now - poorSince > 3000 && now - poorNotedAt > 60000) {
      poorNotedAt = now;
      flash("The link to the device is slow or unstable: the picture and your input lag. Smooth needs less bandwidth.", "warning");
    }
  }
  setInterval(renderLink, 500);

  const pending = new Map(); // input id → what it was
  let nextId = 1;

  function onText(msg) {
    switch (msg.t) {
      case "format":
        configure(msg);
        break;
      case "pong": {
        const now = performance.now();
        rtt = now - msg.at;
        onPong(rtt);
        // The server's clock at the middle of the round trip, from the fastest of the last pongs:
        // the least queueing, so the closest to an even split.
        offsets.push([rtt, msg.server_ms - (Date.now() - rtt / 2)]);
        if (offsets.length > 10) offsets.shift();
        clockOffset = offsets.reduce((a, b) => (b[0] < a[0] ? b : a))[1];
        break;
      }
      case "preset":
        flash(`Preset: ${msg.preset}`);
        break;
      case "rtc_answer":
        onRtcAnswer(msg);
        break;
      case "server_stats":
        serverStages = { server: msg.server, input: msg.input, lock: msg.lock, pace: msg.pace, text: msg.text };
        tracing = msg.trace === true;
        if (!tracing) traced = { frames: [], inputs: [] };
        break;
      case "applied":
        onApplied(msg);
        break;
      case "done": {
        const kind = pending.get(msg.id);
        pending.delete(msg.id);
        if (kind === "paste") {
          flash(msg.via === "typed"
            ? "Typed into the focused field (the device clipboard is not reachable; US keyboard characters only)."
            : "Pasted into the focused field.");
        }
        break;
      }
      case "error":
        if (msg.code === "rtc_unavailable") {
          // No WebRTC on this host or no port free: the WebSocket stream goes on, nothing to say.
          stopRtc();
          break;
        }
        if (msg.id) pending.delete(msg.id);
        flash(msg.message || "Input failed.", msg.code === "busy" ? "warning" : "error");
        if (msg.code === "busy") refresh();
        break;
    }
  }

  // ── video ──────────────────────────────────────────────────────────────

  let decoder = null;
  let waitingKey = true;
  let format = null;
  let decodeErrors = [];
  const stats = { frames: [], bytes: [], age: null, drawn: 0 };
  // Where each frame's time goes (see `pipeline.rs`): the last samples of each stage, in
  // milliseconds. "In server" and the input's server stages come from the server every 5 s.
  const stages = { age: [], encode: [], helper: [], down: [], decode: [], upload: [], react: [], photon: [] };
  let serverStages = { server: null, input: null, lock: null, pace: null, text: null };
  function sample(name, ms) {
    if (!Number.isFinite(ms) || ms < 0 || ms > 120000) return;
    const a = stages[name];
    a.push(ms);
    if (a.length > 256) a.shift();
  }
  function p50p95(a) {
    if (!a.length) return null;
    const s = [...a].sort((x, y) => x - y);
    const at = (p) => Math.round(s[Math.round((s.length - 1) * p)] * 10) / 10;
    return [at(0.5), at(0.95)];
  }

  // An input's way to the screen: the server says when it read the input and when the helper
  // had it (`applied`); the first frame captured after that is the screen's answer, whatever
  // changed it. Inputs still waiting for that frame: [page time, helper time in µs].
  const awaiting = [];
  // Frames shown in the last second, [capture µs, page time], for an `applied` that comes late.
  const shown = [];
  const REACT_MAX_MS = 2000;
  function onApplied(msg) {
    // The page's time of the input, on the server's clock.
    const sentAt = Date.now() - (performance.now() - msg.ts) + clockOffset;
    sample("upload", msg.read_us / 1000 - sentAt);
    if (rtcOn) return;
    const frame = shown.find(([capturedUs]) => capturedUs > msg.sent_us);
    if (frame) answered(msg.ts, msg.sent_us, frame[0], frame[1]);
    else awaiting.push([msg.ts, msg.sent_us]);
  }
  function answered(ts, sentUs, capturedUs, at) {
    const react = (capturedUs - sentUs) / 1000;
    if (react > REACT_MAX_MS) return;
    sample("react", react);
    sample("photon", at - ts);
    if (tracing) traced.inputs.push([ts, serverMs(ts), capturedUs, serverMs(at)]);
  }

  // The viewer trace (an admin turns it on; `server_stats` says so): every frame shown and
  // every input answered goes to the server each second, on the server's clock.
  let tracing = false;
  let traced = { frames: [], inputs: [] };
  function serverMs(pageTime) {
    return Date.now() - (performance.now() - pageTime) + clockOffset;
  }
  setInterval(() => {
    if (!tracing || (!traced.frames.length && !traced.inputs.length)) return;
    // The server takes at most 1024 of each; a page that falls that far behind loses the oldest.
    send({
      t: "trace",
      offset_ms: clockOffset,
      rtt_ms: rtt,
      frames: traced.frames.slice(-1024),
      inputs: traced.inputs.slice(-1024),
    });
    traced = { frames: [], inputs: [] };
  }, 1000);
  function onShown(capturedUs, at) {
    shown.push([capturedUs, at]);
    while (shown.length && at - shown[0][1] > 1000) shown.shift();
    for (let i = awaiting.length - 1; i >= 0; i--) {
      const [ts, sentUs] = awaiting[i];
      if (capturedUs > sentUs) answered(ts, sentUs, capturedUs, at);
      if (capturedUs > sentUs || at - ts > REACT_MAX_MS * 2) awaiting.splice(i, 1);
    }
  }
  // Frames sent to the decoder and not yet shown: capture time → [frame number, when sent]. A
  // frame is acknowledged once it is shown, so a slow decoder holds the stream back like a slow
  // link.
  const decoding = new Map();
  // At most this many frames wait in the decoder; past it, frames are dropped up to a keyframe.
  // A jittery link delivers frames in bursts: the decoder takes 2–5 ms a frame and catches up,
  // where a drop costs a keyframe's round trip.
  const MAX_DECODE_QUEUE = 16;

  function base64Bytes(s) {
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }

  // Acknowledges every frame still in the decoder (it was reset, or stalled) and starts over at a
  // keyframe.
  function flushDecoding() {
    let last = null;
    for (const [seq] of decoding.values()) last = seq;
    decoding.clear();
    if (last !== null) send({ t: "ack", seq: last });
    waitingKey = true;
    send({ t: "keyframe" });
  }

  // The last frame number seen on the socket, for the frames the server skipped.
  let lastSeq = null;

  function resetVideo() {
    decoding.clear();
    waitingKey = true;
    lastSeq = null;
  }

  function newDecoder() {
    return new VideoDecoder({
      output: (frame) => {
        const entry = decoding.get(frame.timestamp);
        if (entry !== undefined) {
          decoding.delete(frame.timestamp);
          send({ t: "ack", seq: entry[0] });
          sample("decode", performance.now() - entry[1]);
          if (tracing) traced.frames.push([entry[0], serverMs(entry[1]), serverMs(performance.now())]);
        }
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        g2d.drawImage(frame, 0, 0);
        onShown(frame.timestamp, performance.now());
        const now = Date.now();
        stats.age = now + clockOffset - frame.timestamp / 1000;
        sample("age", stats.age);
        stats.frames.push(now);
        frame.close();
        veil.hidden = true;
      },
      error: (e) => {
        decoder = null;
        // A browser that cannot decode this stream fails the same way every time: stop there.
        const now = Date.now();
        decodeErrors = decodeErrors.filter((t) => t > now - 10000).concat(now);
        if (e && e.name === "NotSupportedError") {
          end(`This browser cannot decode the stream (${format ? format.codec : "H.264"}).`);
        } else if (decodeErrors.length > 3) {
          end("The stream could not be decoded. Open the device again.");
        } else if (format) {
          configure(format);
          flushDecoding();
        }
      },
    });
  }

  // While the decoder is being set up, frames from a keyframe on are held and decoded once it is
  // done, so the stream starts without asking for another keyframe. A keyframe dropped anyway
  // (none was held) is asked for again once the decoder is ready: on a slow link the format and
  // the first keyframe arrive together, before the setup is done.
  let configuring = false;
  let droppedKey = false;
  let held = [];
  const MAX_HELD = 60;

  async function configure(f) {
    format = f;
    configuring = true;
    const config = {
      codec: f.codec,
      codedWidth: f.width,
      codedHeight: f.height,
      description: base64Bytes(f.description),
      optimizeForLatency: true,
    };
    waitingKey = true;
    try {
      const support = await VideoDecoder.isConfigSupported(config);
      if (!support.supported) {
        end(`This browser cannot decode the stream (${f.codec}, ${f.width}×${f.height}).`);
        return;
      }
    } catch {
      end(`This browser cannot decode the stream (${f.codec}).`);
      return;
    }
    if (format !== f || ended) return;
    if (!decoder || decoder.state === "closed") decoder = newDecoder();
    decoder.configure(config);
    decoding.clear();
    configuring = false;
    const queued = held;
    held = [];
    for (const [seq, key, capturedUs, buf] of queued) decode(seq, key, capturedUs, buf);
    if (droppedKey) {
      droppedKey = false;
      if (!queued.length) send({ t: "keyframe" });
    }
  }

  function onFrame(buf) {
    const view = new DataView(buf);
    const key = (view.getUint8(0) & 1) === 1;
    const seq = view.getUint32(1);
    const capturedUs = Number(view.getBigInt64(5));
    const encodedUs = Number(view.getBigInt64(13));
    const receivedUs = Number(view.getBigInt64(21));
    stats.bytes.push([Date.now(), buf.byteLength]);
    sample("encode", (encodedUs - capturedUs) / 1000);
    sample("helper", (receivedUs - encodedUs) / 1000);
    // Read from the helper to here, on the server's clock: in server plus the network.
    sample("down", Date.now() + clockOffset - receivedUs / 1000);
    countFrames(1, lastSeq !== null && seq > lastSeq ? seq - lastSeq - 1 : 0);
    lastSeq = seq;
    const ready = decoder && decoder.state === "configured" && !configuring;
    if (ready && !key && decoder.decodeQueueSize >= MAX_DECODE_QUEUE) {
      // The decoder is behind: drop up to the next keyframe rather than fall further behind.
      waitingKey = true;
      send({ t: "keyframe" });
    }
    if (!ready && configuring && (key || held.length) && held.length < MAX_HELD) {
      if (key) held = [];
      held.push([seq, key, capturedUs, buf]);
      return;
    }
    if (!ready || (waitingKey && !key)) {
      if (!ready && key) droppedKey = true;
      if (!ready) held = [];
      send({ t: "ack", seq });
      return;
    }
    decode(seq, key, capturedUs, buf);
  }

  function decode(seq, key, capturedUs, buf) {
    waitingKey = false;
    decoding.set(capturedUs, [seq, performance.now()]);
    decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: capturedUs, data: new Uint8Array(buf, 29) }));
  }

  // A decoder that holds a frame for a second has stalled: let the stream go on.
  setInterval(() => {
    const oldest = decoding.values().next().value;
    if (oldest && performance.now() - oldest[1] > 1000) flushDecoding();
  }, 500);

  // ── WebRTC ─────────────────────────────────────────────────────────────

  // With `info.webrtc`, the page also offers a WebRTC session over the socket: the stream as
  // H.264 over UDP into a <video> laid over the canvas, and input on the `input` data channel.
  // A lost packet then holds back only its own frame, not the input or the frames after it.
  // The socket keeps the replies, and the video goes back to it when WebRTC fails or never
  // connects.
  const video = $("rtc");
  let pc = null;
  let dc = null;
  let rtcOn = false; // the page shows the WebRTC video
  let rtcTried = false; // one try per socket
  let rtcTimer = 0;
  let rtcBytes = null;

  function rtcUsable() {
    return !!(info && info.webrtc && "RTCPeerConnection" in window && "requestVideoFrameCallback" in HTMLVideoElement.prototype);
  }

  async function startRtc() {
    if (pc || rtcTried || ended || !rtcUsable()) return;
    rtcTried = true;
    const peer = new RTCPeerConnection({ bundlePolicy: "max-bundle" });
    pc = peer;
    const tr = peer.addTransceiver("video", { direction: "recvonly" });
    // Show each frame as soon as it is decoded: no jitter buffer beyond what decoding needs.
    if ("jitterBufferTarget" in tr.receiver) tr.receiver.jitterBufferTarget = 0;
    if ("playoutDelayHint" in tr.receiver) tr.receiver.playoutDelayHint = 0;
    dc = peer.createDataChannel("input", { ordered: true });
    dc.onmessage = (ev) => {
      try {
        onText(JSON.parse(ev.data));
      } catch { /* not ours */ }
    };
    peer.ontrack = (ev) => {
      video.srcObject = new MediaStream([ev.track]);
      placeVideo();
      video.play().catch(() => {});
      video.requestVideoFrameCallback(onRtcFrame);
    };
    peer.onconnectionstatechange = () => {
      if (pc === peer && ["failed", "closed"].includes(peer.connectionState)) {
        stopRtc("The WebRTC link failed: the video is back on the WebSocket.");
      }
    };
    try {
      await peer.setLocalDescription(await peer.createOffer());
    } catch {
      stopRtc();
      return;
    }
    if (pc !== peer || !send({ t: "rtc_offer", sdp: peer.localDescription.sdp })) {
      stopRtc();
      return;
    }
    // A page that cannot reach the host over UDP stays on the WebSocket.
    rtcTimer = setTimeout(() => {
      if (!rtcOn) stopRtc();
    }, 10000);
  }

  async function onRtcAnswer(msg) {
    if (!pc) return;
    try {
      await pc.setRemoteDescription({ type: "answer", sdp: msg.sdp });
    } catch {
      stopRtc();
    }
  }

  function stopRtc(why) {
    clearTimeout(rtcTimer);
    if (pc) {
      pc.ontrack = null;
      pc.onconnectionstatechange = null;
      pc.close();
    }
    pc = null;
    dc = null;
    rtcBytes = null;
    video.srcObject = null;
    video.classList.remove("on");
    if (rtcOn) {
      rtcOn = false;
      send({ t: "video_via", via: "ws" });
      if (why) flash(why, "warning");
    }
  }

  // The video covers the canvas exactly; the canvas below keeps the pointer.
  function placeVideo() {
    const r = canvas.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    video.style.left = `${r.left - s.left}px`;
    video.style.top = `${r.top - s.top}px`;
    video.style.width = `${r.width}px`;
    video.style.height = `${r.height}px`;
  }
  window.addEventListener("resize", () => {
    if (rtcOn) placeVideo();
  });

  function onRtcFrame(_now, meta) {
    if (!pc) return;
    // The canvas keeps the picture's size, so the layout and the pointer map stay right.
    if (canvas.width !== meta.width || canvas.height !== meta.height) {
      canvas.width = meta.width;
      canvas.height = meta.height;
    }
    if (!rtcOn) {
      rtcOn = true;
      clearTimeout(rtcTimer);
      send({ t: "video_via", via: "rtc" });
      video.classList.add("on");
      veil.hidden = true;
      resetVideo();
    }
    placeVideo();
    stats.frames.push(Date.now());
    // The RTP time is the capture time at 90 kHz, modulo 2^32: the frame's age on the server's
    // clock.
    if (meta.rtpTimestamp !== undefined) {
      const wrap = 2 ** 32 / 90;
      const age = (((Date.now() + clockOffset - meta.rtpTimestamp / 90) % wrap) + wrap) % wrap;
      if (age < 10000) {
        stats.age = age;
        sample("age", age);
      }
    }
    video.requestVideoFrameCallback(onRtcFrame);
  }

  // The received bytes, for the bitrate in Stats, and the packets lost, for the link badge.
  setInterval(async () => {
    if (!pc || !rtcOn) return;
    let bytes = 0;
    let got = 0;
    let lost = 0;
    (await pc.getStats()).forEach((r) => {
      if (r.type === "inbound-rtp" && r.kind === "video") {
        bytes += r.bytesReceived;
        got += r.packetsReceived;
        lost += r.packetsLost;
      }
    });
    if (rtcBytes !== null) {
      stats.bytes.push([Date.now(), bytes - rtcBytes.bytes]);
      countFrames(Math.max(0, got - rtcBytes.got), Math.max(0, lost - rtcBytes.lost));
    }
    rtcBytes = { bytes, got, lost };
  }, 1000);

  // ── stats overlay ──────────────────────────────────────────────────────

  let statsOn = false;
  let statsTimer = 0;

  function renderStats() {
    const now = Date.now();
    stats.frames = stats.frames.filter((t) => t > now - 1000);
    stats.bytes = stats.bytes.filter(([t]) => t > now - 1000);
    const bits = stats.bytes.reduce((s, [, n]) => s + n, 0) * 8;
    const lines = [
      `FPS: ${stats.frames.length}`,
      `Bitrate: ${(bits / 1e6).toFixed(2)} Mbit/s`,
      `Resolution: ${canvas.width}×${canvas.height}`,
      `Codec: ${format ? format.codec : "—"}`,
      `Transport: ${rtcOn ? "WebRTC (UDP)" : "WebSocket"}`,
      `Round trip: ${rtt === null ? "—" : `${Math.round(rtt)} ms`}`,
      `Jitter: ${rtts.length < 2 ? "—" : `${Math.round(jitter())} ms`}`,
      `Lost (3 s): ${loss.pct === null ? "—" : `${loss.pct.toFixed(1)} %`}`,
      `Frame age: ${stats.age === null ? "—" : `${Math.round(stats.age)} ms`}`,
      `Decode queue: ${rtcOn ? "—" : decoder ? decoder.decodeQueueSize : 0}`,
      ...pipelineLines(),
    ];
    statsOverlay.textContent = lines.join("\n");
  }

  // The stages of the last frames, p50 / p95 in ms. The network is what the server's part does
  // not explain of "read from the helper to here".
  function pipelineLines() {
    const f = (v) => (v ? `${v[0]} / ${v[1]} ms` : "—");
    const down = p50p95(stages.down);
    const s = serverStages;
    const net = down && s.server ? [Math.max(0, Math.round(down[0] - s.server[0])), Math.max(0, Math.round(down[1] - s.server[1]))] : null;
    return [
      "",
      "Where the time goes (p50 / p95):",
      `  Encode: ${f(p50p95(stages.encode))}`,
      `  Helper → server: ${f(p50p95(stages.helper))}`,
      `  In server: ${rtcOn ? "—" : f(s.server)}`,
      `  Network: ${rtcOn ? "—" : f(net)}`,
      `  Decode: ${rtcOn ? "—" : f(p50p95(stages.decode))}`,
      `  Capture → shown: ${f(p50p95(stages.age))}`,
      "",
      "Input (taps, keys, buttons):",
      `  Upload: ${f(p50p95(stages.upload))}`,
      `  In server: ${f(s.input)} (lock ${f(s.lock)}, pacing ${f(s.pace)})`,
      `  Screen reacts: ${rtcOn ? "—" : f(p50p95(stages.react))}`,
      `  Input → shown: ${rtcOn ? "—" : f(p50p95(stages.photon))}`,
      `  Text: ${f(s.text)}`,
    ];
  }

  // The page's stages go to the server every 5 s, for its log.
  setInterval(() => {
    if (!stages.age.length && !stages.down.length) return;
    send({
      t: "telemetry",
      report: {
        transport: rtcOn ? "rtc" : "ws",
        age: p50p95(stages.age),
        encode: p50p95(stages.encode),
        helper: p50p95(stages.helper),
        down: rtcOn ? null : p50p95(stages.down),
        decode: rtcOn ? null : p50p95(stages.decode),
        upload: p50p95(stages.upload),
        react: p50p95(stages.react),
        photon: p50p95(stages.photon),
        rtt: rtt === null ? null : Math.round(rtt),
        jitter: rtts.length < 2 ? null : Math.round(jitter()),
        lost: loss.pct,
        fps: stats.frames.filter((t) => t > Date.now() - 1000).length,
      },
    });
  }, 5000);

  function toggleStats() {
    statsOn = !statsOn;
    statsOverlay.hidden = !statsOn;
    clearInterval(statsTimer);
    if (statsOn) {
      renderStats();
      statsTimer = setInterval(renderStats, 500);
    }
  }

  $("stats-btn").addEventListener("click", toggleStats);

  // ── touch ──────────────────────────────────────────────────────────────

  // Pointer position as 0-1 of the picture, kept inside it.
  function norm(ev) {
    const r = canvas.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (ev.clientY - r.top) / r.height)),
    };
  }

  // A gesture that starts at a screen edge is a system gesture (home indicator, Notification Centre).
  function edgeOf(p) {
    if (p.y >= 0.97) return "bottom";
    if (p.y <= 0.015) return "top";
    if (p.x <= 0.015) return "left";
    if (p.x >= 0.985) return "right";
    return undefined;
  }

  const fingerDot = $("finger");
  // Where a 0-1 point of the picture is, in CSS pixels of the screen element.
  function toScreen(p) {
    const r = canvas.getBoundingClientRect();
    const s = screen.getBoundingClientRect();
    return { x: r.left - s.left + p.x * r.width, y: r.top - s.top + p.y * r.height };
  }
  function showFinger(p) {
    if (!p) {
      fingerDot.hidden = true;
      return;
    }
    const q = toScreen(p);
    fingerDot.style.left = `${q.x}px`;
    fingerDot.style.top = `${q.y}px`;
    fingerDot.hidden = false;
  }

  // The finger's path and a ripple for each tap, drawn at once: input feels immediate though
  // the device's answer takes a round trip.
  const trail = $("trail");
  const t2d = trail.getContext("2d");
  const TRAIL_MS = 400;
  const RIPPLE_MS = 350;
  const accent = getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#2563eb";
  const points = []; // {x, y, t, start}: screen CSS pixels; `start` begins a new stroke
  const ripples = [];
  let drawing = false;
  function trailAdd(p, start) {
    points.push({ ...toScreen(p), t: performance.now(), start });
    kickTrail();
  }
  function ripple(p) {
    ripples.push({ ...toScreen(p), t: performance.now() });
    kickTrail();
  }
  function kickTrail() {
    if (drawing) return;
    drawing = true;
    requestAnimationFrame(drawTrail);
  }
  function drawTrail() {
    const s = screen.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const w = Math.round(s.width * dpr);
    const h = Math.round(s.height * dpr);
    if (trail.width !== w || trail.height !== h) {
      trail.width = w;
      trail.height = h;
    }
    t2d.setTransform(dpr, 0, 0, dpr, 0, 0);
    t2d.clearRect(0, 0, s.width, s.height);
    const now = performance.now();
    while (points.length && now - points[0].t > TRAIL_MS) points.shift();
    while (ripples.length && now - ripples[0].t > RIPPLE_MS) ripples.shift();
    t2d.strokeStyle = accent;
    t2d.lineCap = "round";
    t2d.lineWidth = 6;
    for (let i = 1; i < points.length; i++) {
      const a = points[i - 1];
      const b = points[i];
      if (b.start) continue;
      t2d.globalAlpha = 0.55 * (1 - (now - b.t) / TRAIL_MS);
      t2d.beginPath();
      t2d.moveTo(a.x, a.y);
      t2d.lineTo(b.x, b.y);
      t2d.stroke();
    }
    t2d.lineWidth = 2;
    for (const r of ripples) {
      const k = (now - r.t) / RIPPLE_MS;
      t2d.globalAlpha = 0.7 * (1 - k);
      t2d.beginPath();
      t2d.arc(r.x, r.y, 12 + 18 * k, 0, 2 * Math.PI);
      t2d.stroke();
    }
    t2d.globalAlpha = 1;
    if (points.length || ripples.length) requestAnimationFrame(drawTrail);
    else drawing = false;
  }

  // Each touch carries when the page saw it (`ts`), so the server can hand a gesture that the
  // link delayed or bunched to the device with its own spacing.
  function touch(phase, p, ts, edge) {
    sendInput({ t: "touch", phase, x: p.x, y: p.y, edge, ts });
  }

  // Moves are sent at most once per animation frame, always the latest point.
  let moveAt = null;
  let moveTs = 0;
  let moveScheduled = false;
  function queueMove(p, ts) {
    moveAt = p;
    moveTs = ts;
    if (moveScheduled) return;
    moveScheduled = true;
    requestAnimationFrame(flushMove);
  }
  function flushMove() {
    moveScheduled = false;
    if (moveAt && (finger || wheel.active)) touch("move", moveAt, moveTs);
    moveAt = null;
  }

  let finger = null;
  let downAt = null;
  canvas.addEventListener("pointerdown", (ev) => {
    if (!control || ev.button !== 0 || finger) return;
    ev.preventDefault();
    screen.focus();
    canvas.setPointerCapture(ev.pointerId);
    endWheel();
    const p = norm(ev);
    finger = ev.pointerId;
    downAt = p;
    touch("down", p, ev.timeStamp, edgeOf(p));
    showFinger(p);
    trailAdd(p, true);
  });
  canvas.addEventListener("pointermove", (ev) => {
    if (ev.pointerId !== finger) return;
    const p = norm(ev);
    queueMove(p, ev.timeStamp);
    showFinger(p);
    trailAdd(p, false);
  });
  function lift(ev) {
    if (ev.pointerId !== finger) return;
    finger = null;
    flushMove();
    const p = norm(ev);
    touch("up", p, ev.timeStamp);
    showFinger(null);
    // A tap, not a drag: show where it landed.
    if (downAt && Math.hypot(p.x - downAt.x, p.y - downAt.y) < 0.01) ripple(p);
    downAt = null;
  }
  canvas.addEventListener("pointerup", lift);
  canvas.addEventListener("pointercancel", lift);

  // Wheel and trackpad scrolls become a drag that follows the deltas, lifted after a pause.
  const wheel = { active: false, at: null, origin: null, timer: 0 };
  function endWheel() {
    clearTimeout(wheel.timer);
    if (!wheel.active) return;
    wheel.active = false;
    flushMove();
    touch("up", wheel.at, performance.now());
  }
  canvas.addEventListener(
    "wheel",
    (ev) => {
      if (!control || finger) return;
      ev.preventDefault();
      const r = canvas.getBoundingClientRect();
      if (!wheel.active) {
        const p = norm(ev);
        wheel.origin = { x: Math.min(0.9, Math.max(0.1, p.x)), y: Math.min(0.85, Math.max(0.15, p.y)) };
        wheel.at = { ...wheel.origin };
        wheel.active = true;
        touch("down", wheel.at, ev.timeStamp);
        trailAdd(wheel.at, true);
      }
      const scale = ev.deltaMode === 1 ? 16 : ev.deltaMode === 2 ? r.height : 1;
      const x = wheel.at.x - (ev.deltaX * scale) / r.width;
      const y = wheel.at.y - (ev.deltaY * scale) / r.height;
      if (x < 0.02 || x > 0.98 || y < 0.05 || y > 0.95) {
        // Out of room: lift and start the next drag where this one began.
        endWheel();
      } else {
        wheel.at = { x, y };
        queueMove(wheel.at, ev.timeStamp);
        trailAdd(wheel.at, false);
      }
      clearTimeout(wheel.timer);
      wheel.timer = setTimeout(endWheel, 120);
    },
    { passive: false },
  );

  // ── keyboard ───────────────────────────────────────────────────────────

  // HID usages (page 7) of a US keyboard. A character is [usage, shift].
  const SPECIAL = {
    Enter: 40, Escape: 41, Backspace: 42, Tab: 43, Delete: 76,
    ArrowRight: 79, ArrowLeft: 80, ArrowDown: 81, ArrowUp: 82,
    Home: 74, End: 77, PageUp: 75, PageDown: 78,
  };
  const US = {};
  for (let i = 0; i < 26; i++) {
    US[String.fromCharCode(97 + i)] = [4 + i, false];
    US[String.fromCharCode(65 + i)] = [4 + i, true];
  }
  "1234567890".split("").forEach((c, i) => (US[c] = [30 + i, false]));
  "!@#$%^&*()".split("").forEach((c, i) => (US[c] = [30 + i, true]));
  const PAIRS = [[" ", 44], ["-", 45, "_"], ["=", 46, "+"], ["[", 47, "{"], ["]", 48, "}"], ["\\", 49, "|"],
    [";", 51, ":"], ["'", 52, "\""], ["`", 53, "~"], [",", 54, "<"], [".", 55, ">"], ["/", 56, "?"]];
  for (const [c, u, s] of PAIRS) {
    US[c] = [u, false];
    if (s) US[s] = [u, true];
  }
  const SHIFT = 225;
  const GUI = 227;
  const OPTION = 226;

  // Accented letters and a few symbols, typed as on a Mac's US keyboard: a dead key with Option
  // (´ ` ˆ ˜ ¨), then the letter, or one Option key. Checked on the simulator. The clipboard
  // would make iOS ask "Allow Paste" in the app, and the keys typed meanwhile go to that prompt.
  const KEYED = {};
  const DEAD = [[8, "\u0301", "aeiouAEIOU"], [53, "\u0300", "aeiouAEIOU"], [12, "\u0302", "aeiouAEIOU"],
    [17, "\u0303", "anoANO"], [24, "\u0308", "aeiouyAEIOU"]];
  for (const [usage, mark, letters] of DEAD) {
    for (const c of letters) {
      const [u, shift] = US[c];
      KEYED[(c + mark).normalize("NFC")] = [[usage, [OPTION]], [u, shift ? [SHIFT] : []]];
    }
  }
  for (const [c, usage, shift] of [["ç", 6], ["Ç", 6, true], ["ß", 22], ["ª", 38], ["º", 39], ["¡", 30], ["¿", 56, true], ["€", 31, true]]) {
    KEYED[c] = [[usage, shift ? [OPTION, SHIFT] : [OPTION]]];
  }

  // Other characters a US keyboard cannot type (emoji, other scripts) go through the clipboard.
  // While such text waits, later characters join it, so the order stays.
  let text = "";
  let textTimer = 0;
  function flushText() {
    clearTimeout(textTimer);
    if (text) paste(text);
    text = "";
  }
  // Sends a text input and remembers it for its answer; says so when it could not be sent.
  function request(kind, t) {
    const id = nextId++;
    if (!sendInput({ t: kind, id, text: t })) {
      flash("Not connected: the text was not sent.", "error");
      return false;
    }
    pending.set(id, kind);
    return true;
  }
  function paste(t) {
    request("paste", t);
  }
  function key(usage, mods) {
    flushText();
    sendInput({ t: "key", usage, mods: mods || [] });
  }

  screen.addEventListener("keydown", (ev) => {
    if (!control) return;
    const k = ev.key.toLowerCase();
    if (ev.metaKey && ev.shiftKey && k === "h") {
      ev.preventDefault();
      flushText();
      sendInput({ t: "button", name: "home" });
    } else if (ev.metaKey && k === "l") {
      ev.preventDefault();
      flushText();
      sendInput({ t: "button", name: "lock" });
    } else if (ev.metaKey && k === "s") {
      ev.preventDefault();
      save();
    } else if (ev.metaKey && ev.shiftKey && k === "v") {
      ev.preventDefault();
      flushText();
      doPaste();
    } else if (ev.altKey && ev.code === "KeyI") {
      ev.preventDefault();
      toggleStats();
    } else if (ev.metaKey && ["a", "c", "x", "z"].includes(k)) {
      // Editing shortcuts go to the simulator; the browser keeps the rest.
      ev.preventDefault();
      key(US[k][0], ev.shiftKey ? [GUI, SHIFT] : [GUI]);
    } else if (ev.metaKey || ev.ctrlKey) {
      // Leave the browser's own shortcuts alone.
    } else if (SPECIAL[ev.key]) {
      ev.preventDefault();
      key(SPECIAL[ev.key], ev.shiftKey ? [SHIFT] : []);
    } else if (ev.key.length === 1 || [...ev.key].length === 1) {
      ev.preventDefault();
      const us = US[ev.key];
      const keyed = KEYED[ev.key];
      if (us && !text) {
        key(us[0], us[1] ? [SHIFT] : []);
      } else if (keyed && !text) {
        for (const [u, mods] of keyed) key(u, mods);
      } else {
        text += ev.key;
        clearTimeout(textTimer);
        textTimer = setTimeout(flushText, 250);
      }
    }
  });
  screen.addEventListener("paste", (ev) => {
    if (!control) return;
    const t = ev.clipboardData && ev.clipboardData.getData("text/plain");
    if (t) {
      ev.preventDefault();
      flushText();
      paste(t);
    }
  });

  // ── paste box ──────────────────────────────────────────────────────────

  const pasteBox = $("paste-box");
  const pasteText = $("paste-text");

  async function doPaste() {
    // Try the Clipboard API first (needs a user gesture and permission).
    try {
      const t = await navigator.clipboard.readText();
      if (t) {
        paste(t);
        return;
      }
    } catch { /* fall through to the text box */ }
    pasteBox.hidden = false;
    pasteText.value = "";
    pasteText.focus();
  }

  $("paste").addEventListener("click", doPaste);

  $("paste-send").addEventListener("click", () => {
    const t = pasteText.value;
    pasteBox.hidden = true;
    if (t) paste(t);
    screen.focus();
  });

  $("paste-type").addEventListener("click", () => {
    const t = pasteText.value;
    pasteBox.hidden = true;
    if (t && request("paste_and_type", t)) {
      flash("Typing pasted text…");
    }
    screen.focus();
  });

  $("paste-cancel").addEventListener("click", () => {
    pasteBox.hidden = true;
    screen.focus();
  });

  // ── toolbar ────────────────────────────────────────────────────────────

  $("preset").addEventListener("change", (ev) => send({ t: "preset", name: ev.target.value }));

  function save() {
    const a = document.createElement("a");
    a.href = "screenshot.jpg";
    a.download = "";
    document.body.appendChild(a);
    a.click();
    a.remove();
  }
  $("home").addEventListener("click", () => sendInput({ t: "button", name: "home" }));
  $("lock").addEventListener("click", () => sendInput({ t: "button", name: "lock" }));
  $("shot").addEventListener("click", save);

  refresh().then(connect);
  setInterval(refresh, 5000);
})();
